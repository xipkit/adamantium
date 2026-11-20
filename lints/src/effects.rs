use std::collections::{BTreeSet, VecDeque};

use rustc_data_structures::fx::{FxHashMap, FxHashSet, FxIndexSet};
use rustc_hir::def_id::LocalDefId;
use rustc_lint::LateContext;
use rustc_span::Span;

use crate::{LintKind, Walker};

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Context {
    pub(crate) asynchronous: bool,
    pub(crate) actor: bool,
    pub(crate) limit: Option<LimitKind>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum LimitKind {
    Ordinary,
    Keyed,
}

impl Context {
    pub(crate) fn enter(self, inner: Self) -> Self {
        Self {
            asynchronous: self.asynchronous || inner.asynchronous,
            actor: self.actor || inner.actor,
            limit: inner.limit.or(self.limit),
        }
    }
}

impl LimitKind {
    pub(crate) fn effect(self) -> Effect {
        match self {
            Self::Ordinary => Effect::Limit,
            Self::Keyed => Effect::KeyedLimit,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    Blocking,
    Actor,
    Limit,
    KeyedLimit,
}

impl Effect {
    pub(crate) const ALL: [Self; 4] = [Self::Blocking, Self::Actor, Self::Limit, Self::KeyedLimit];

    pub(crate) fn violation(self, context: Context) -> Option<LintKind> {
        match self {
            Self::Blocking if context.actor => Some(LintKind::BlockingInActor),
            Self::Blocking => context.asynchronous.then_some(LintKind::BlockingInAsync),
            Self::Actor => context.actor.then_some(LintKind::NestedActor),
            Self::Limit => context.limit.is_some().then_some(LintKind::NestedLimit),
            Self::KeyedLimit => {
                (context.limit == Some(LimitKind::Keyed)).then_some(LintKind::NestedLimit)
            }
        }
    }

    pub(crate) fn operation(self) -> &'static str {
        match self {
            Self::Blocking => "the blocking operation",
            Self::Actor => "the actor access",
            Self::Limit | Self::KeyedLimit => "the limit entered",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Origin {
    pub(crate) span: Span,
    pub(crate) via: Option<LocalDefId>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Use {
    pub(crate) param: u32,
    pub(crate) how: How,
    pub(crate) context: Context,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum How {
    Invoke,
    Poll,
}

#[derive(Clone, Default)]
pub(crate) struct Summary {
    effects: [Option<Origin>; Effect::ALL.len()],
    pub(crate) uses: BTreeSet<Use>,
}

impl Summary {
    pub(crate) fn has(&self, effect: Effect) -> bool {
        self.effects[effect as usize].is_some()
    }

    pub(crate) fn record(&mut self, effect: Effect, origin: Origin) {
        self.effects[effect as usize].get_or_insert(origin);
    }

    fn merge(&mut self, other: Self) -> bool {
        let mut grew = false;
        for (mine, theirs) in self.effects.iter_mut().zip(other.effects) {
            if mine.is_none() && theirs.is_some() {
                *mine = theirs;
                grew = true;
            }
        }
        for used in other.uses {
            grew |= self.uses.insert(used);
        }
        grew
    }
}

#[derive(Default)]
pub(crate) struct Summaries {
    table: FxHashMap<LocalDefId, Summary>,
    dependents: FxHashMap<LocalDefId, FxIndexSet<LocalDefId>>,
    pending: VecDeque<LocalDefId>,
    queued: FxHashSet<LocalDefId>,
}

impl Summaries {
    pub(crate) fn solved<'tcx>(&mut self, cx: &LateContext<'tcx>, node: LocalDefId) -> Summary {
        self.add(node);
        while let Some(next) = self.pending.pop_front() {
            self.queued.remove(&next);
            let found = Walker::summarize(cx, self, next);
            if self.table.entry(next).or_default().merge(found)
                && let Some(dependents) = self.dependents.get(&next)
            {
                for &dependent in dependents {
                    if self.queued.insert(dependent) {
                        self.pending.push_back(dependent);
                    }
                }
            }
        }
        self.table.get(&node).cloned().unwrap_or_default()
    }

    pub(crate) fn read(&mut self, reader: LocalDefId, node: LocalDefId) -> Summary {
        self.add(node);
        self.dependents.entry(node).or_default().insert(reader);
        self.table.get(&node).cloned().unwrap_or_default()
    }

    fn add(&mut self, node: LocalDefId) {
        if !self.table.contains_key(&node) {
            self.table.insert(node, Summary::default());
            self.queued.insert(node);
            self.pending.push_back(node);
        }
    }

    pub(crate) fn chain(&self, effect: Effect, mut via: Option<LocalDefId>) -> Vec<Origin> {
        let mut origins = Vec::new();
        while let Some(node) = via
            && let Some(origin) = self
                .table
                .get(&node)
                .and_then(|summary| summary.effects[effect as usize])
        {
            origins.push(origin);
            via = origin.via;
        }
        origins
    }
}
