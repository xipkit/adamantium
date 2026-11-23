use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use super::{Group, Outcome, Reason, Refusal, SHARED};
use crate::fix::{
    scan::{Crate, Hazard},
    source::coalesce,
};

pub(in crate::fix) struct Draft {
    pub(super) file: PathBuf,
    pub(super) groups: Vec<Group>,
    pub(super) links: Vec<(usize, usize)>,
    pub(super) callees: Vec<Followed>,
}

/// A function whose parameters or return value `fix` rewrites, with the
/// number of calls it followed and the groups those rewrites belong to.
pub(super) struct Followed {
    pub(super) name: String,
    pub(super) calls: usize,
    pub(super) groups: Vec<usize>,
}

pub(in crate::fix) fn settle(mut drafts: Vec<Draft>, krate: &Crate) -> Vec<(PathBuf, Outcome)> {
    for draft in &mut drafts {
        draft.share();
    }
    let callbacks: BTreeSet<String> = drafts
        .iter()
        .flat_map(|draft| &draft.groups)
        .filter(|group| group.refusal.is_none())
        .flat_map(|group| group.callbacks.iter().cloned())
        .collect();
    for draft in &mut drafts {
        draft.reach(krate, &callbacks);
        draft.share();
    }
    drafts.into_iter().map(Draft::finish).collect()
}

impl Draft {
    fn components(&self) -> Vec<Vec<usize>> {
        let mut parent: Vec<usize> = (0..self.groups.len()).collect();
        fn root(parent: &mut [usize], mut node: usize) -> usize {
            while let Some(&up) = parent.get(node)
                && up != node
            {
                node = up;
            }
            node
        }
        for &(a, b) in &self.links {
            let (a, b) = (root(&mut parent, a), root(&mut parent, b));
            if let Some(slot) = parent.get_mut(a) {
                *slot = b;
            }
        }
        let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for group in 0..self.groups.len() {
            components
                .entry(root(&mut parent, group))
                .or_default()
                .push(group);
        }
        components.into_values().collect()
    }

    fn share(&mut self) {
        for component in self.components() {
            let refused = component.iter().any(|&group| {
                self.groups
                    .get(group)
                    .is_some_and(|group| group.refusal.is_some())
            });
            if refused {
                for &group in &component {
                    if let Some(group) = self.groups.get_mut(group) {
                        group.refuse(SHARED);
                    }
                }
            }
        }
    }

    fn reach(&mut self, krate: &Crate, callbacks: &BTreeSet<String>) {
        for group in &mut self.groups {
            if let Some(reason) = krate.unreadable_reason() {
                group.refuse(Reason::Unfollowed(reason));
            } else if let Some(member) = &group.member
                && let Some(reason) = Self::unseen(krate, member, &self.file)
            {
                group.refuse(reason);
            }
        }
        for callee in &self.callees {
            if krate.occurrences(&callee.name) != callee.calls + 1 {
                for &group in &callee.groups {
                    if let Some(group) = self.groups.get_mut(group) {
                        group.refuse(Reason::Unfollowed(
                            "its function is named somewhere fix doesn't follow as a call",
                        ));
                    }
                }
            }
        }
        for component in self.components() {
            let groups: Vec<&Group> = component
                .iter()
                .filter_map(|&group| self.groups.get(group))
                .collect();
            if groups.iter().any(|group| group.refusal.is_some()) {
                continue;
            }
            let (reason, boundary) = if groups.iter().any(|group| group.escapes) {
                (
                    Some(Reason::Nested(
                        "it is locked in a closure or an async block, which might run inside an actor's callback",
                    )),
                    None,
                )
            } else {
                let locking = groups
                    .iter()
                    .flat_map(|g| g.functions.iter().cloned())
                    .collect();
                let blocks = groups
                    .iter()
                    .flat_map(|g| g.poll_blocks.iter().cloned())
                    .collect();
                let reached = krate.callers(&locking);
                match krate.poll_hazard(&reached, &blocks) {
                    Some(boundary) => (Some(Reason::Poll), Some(boundary)),
                    None => (Self::hazard(&reached, krate, callbacks), None),
                }
            };
            if let Some(reason) = reason
                && let Some(group) = component
                    .first()
                    .and_then(|&first| self.groups.get_mut(first))
            {
                group.refuse(reason);
                group.boundary = boundary;
            }
        }
    }

    fn unseen(krate: &Crate, member: &str, file: &Path) -> Option<Reason> {
        if krate.field_declarations(member) > 1 {
            return Some(Reason::Unfollowed(
                "another struct, union or variant in its package has a field of its name",
            ));
        }
        krate
            .names_member_elsewhere(member, file)
            .then_some(Reason::Unfollowed(
                "another file in its package names a field of its name",
            ))
    }

    fn hazard(
        reached: &BTreeSet<String>,
        krate: &Crate,
        callbacks: &BTreeSet<String>,
    ) -> Option<Reason> {
        if let Some(hazard) = krate.hazard(reached) {
            return Some(Reason::Nested(match hazard {
                Hazard::Trait => {
                    "it is locked through a trait method, which might run inside an actor's callback"
                }
                Hazard::Public => {
                    "it is locked through a public function, which another crate might call inside an actor's callback"
                }
                Hazard::Escapes => {
                    "a function that locks it is used as a value, or in a closure, an async block or a macro, which might run inside an actor's callback"
                }
                Hazard::Callbacks => {
                    "an actor callback in its package calls code that might lock it"
                }
            }));
        }
        (!reached.is_disjoint(callbacks)).then_some(Reason::Nested(
            "a function that locks it is called in a statement that becomes an actor's callback",
        ))
    }

    fn finish(self) -> (PathBuf, Outcome) {
        let mut outcome = Outcome {
            edits: Vec::new(),
            refusals: Vec::new(),
        };
        for group in self.groups {
            match group.refusal {
                Some(reason) => outcome.refusals.push(Refusal {
                    line: group.line,
                    name: group.name,
                    kind: group.kind,
                    reason,
                    boundary: group.boundary,
                }),
                None => outcome.edits.extend(group.edits),
            }
        }
        outcome.edits = coalesce(outcome.edits);
        (self.file, outcome)
    }
}

pub(super) fn refuse_overlaps(groups: &mut [Group]) {
    let mut ranges: Vec<(usize, usize, usize)> = groups
        .iter()
        .enumerate()
        .filter(|(_, group)| group.refusal.is_none())
        .flat_map(|(index, group)| {
            group
                .edits
                .iter()
                .map(move |edit| (edit.start, edit.end, index))
        })
        .collect();
    ranges.sort_unstable();
    let mut overlapping = BTreeSet::new();
    for (position, &(start, end, index)) in ranges.iter().enumerate() {
        for &(other_start, _, other) in ranges.iter().skip(position + 1) {
            if other_start >= end && other_start > start {
                break;
            }
            overlapping.insert(index);
            overlapping.insert(other);
        }
    }
    for index in overlapping {
        if let Some(group) = groups.get_mut(index) {
            group.refuse(Reason::Unfollowed(
                "rewriting it would overlap the rewrite of another binding",
            ));
        }
    }
}
