use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use syn::visit::Visit;

use super::{Edit, source::Text};
use construct::rewrite;
use find::{Finder, Found};

mod analysis;
mod construct;
mod find;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Oneshot,
    Watch,
    Mpsc,
}

impl Kind {
    pub(super) fn section(self) -> &'static str {
        match self {
            Self::Oneshot => "a-single-reply",
            Self::Watch => "the-latest-value",
            Self::Mpsc => "a-bounded-queue",
        }
    }

    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Oneshot => "a Tokio oneshot channel",
            Self::Watch => "a Tokio watch channel",
            Self::Mpsc => "a Tokio mpsc channel",
        }
    }
}

/// Where each flagged constructor call starts, with where it ends and its kind.
pub(super) type Constructors = BTreeMap<usize, (usize, Kind)>;

pub(super) struct Refusal {
    pub(super) line: usize,
    pub(super) name: String,
    pub(super) kind: Kind,
    pub(super) reason: &'static str,
}

pub(super) struct Outcome {
    pub(super) edits: Vec<Edit>,
    pub(super) refusals: Vec<Refusal>,
}

const REPLY: &str = "::adamantium::Reply";
const WATCH: &str = "::adamantium::Watch";
const CHANNEL: &str = "::adamantium::Channel";

pub(super) const IMPORTS: &[&[&str]] = &[
    &["tokio", "sync"],
    &["tokio", "sync", "oneshot"],
    &["tokio", "sync", "oneshot", "channel"],
    &["tokio", "sync", "watch"],
    &["tokio", "sync", "watch", "channel"],
    &["tokio", "sync", "mpsc"],
    &["tokio", "sync", "mpsc", "channel"],
];

const UNBOUND: &str =
    "its sender and receiver aren't bound by a `let (sender, receiver) = ...` statement";
const TYPED: &str = "its halves have a type annotation that fix would have to rewrite";
const ATTRIBUTE: &str = "it is used under an attribute such as `#[cfg]`";
const ATTRIBUTE_MACRO: &str = "its function has an attribute macro, which might rewrite its body";
const MACRO: &str = "it is used in a macro";
const USE: &str = "a half is used where fix can't follow it, such as an argument, a field, a return value or a reference";
const RESULT: &str = "the result of a send or receive is used in a way whose meaning would change";
const UNLOCATED: &str = "fix couldn't find its source text";
const OVERLAP: &str = "rewriting it would overlap the rewrite of another channel";
const WATCH_SEND: &str = "a send's result is used, but Tokio's `send` fails once every receiver is gone and `Watch::send` keeps the value";
const WATCH_UNWATCHED: &str =
    "a send might run once no receiver is alive, when Tokio drops the value and `Watch` keeps it";
const WATCH_SENDER: &str = "its sender is read, subscribed to or asked about receivers, which would show a value Tokio dropped because no receiver was alive";
const HAS_CHANGED: &str = "Tokio's `has_changed` fails once the sender is gone, even with an unseen value, where `Watcher::has_changed` doesn't";
const BORROW_SEEN: &str = "`borrow` leaves the value unseen, but `Watcher::get` marks it seen, and the receiver also waits for changes";
const AWAIT_CAPTURED: &str = "it is awaited inside a closure or an async block without `move`, which would borrow the receiver that awaiting Tokio's moves";
const CAPACITY: &str = "its capacity isn't a positive integer literal, and Tokio panics on zero where `Channel` rounds it up to one";
const SENDER_CLONED: &str = "its sender is cloned, and Tokio admits waiting sends in the order they arrived, where `Channel` lets a later send take freed room first";
const SEND_SHARED: &str = "a send isn't awaited where it is called, or runs in a closure or an async block that borrows the sender, so two sends might wait at once, and `Channel` doesn't admit them in order as Tokio does";
const RECEIVE_UNAWAITED: &str =
    "a receive isn't awaited where it is called, so fix can't follow its future";
const CLOSE_VALUE: &str =
    "the value of `close` is used, which is `()` in Tokio and a `bool` for `Channel`";
const MPSC_METHOD: &str = "a half calls a method that `Channel` lacks or that means something else there, such as `reserve`, `recv_many`, `blocking_send`, `closed` or `capacity`";
const READ_IN_PLACE: &str = "`*rx.borrow()` reads the value where it is, as in a comparison, without the copy that `Watcher::get` would make with `Clone`";
const BORROW_CAPTURED: &str = "`borrow` would become `Watcher::get`, which needs the receiver mutably, inside a closure or async block";

pub(super) fn plan(file: &Path, source: &str, constructors: &Constructors) -> Outcome {
    let mut outcome = Outcome {
        edits: Vec::new(),
        refusals: Vec::new(),
    };
    let Ok(tree) = syn::parse_file(source) else {
        return outcome;
    };
    let text = Text::new(source, &tree);
    let mut finder = Finder {
        text: &text,
        constructors,
        hazards: 0,
        found: Vec::new(),
    };
    finder.visit_file(&tree);
    let mut seen = BTreeSet::new();
    let mut groups = Vec::new();
    for found in finder.found {
        seen.insert(found.start);
        groups.push(group(file, &text, &found));
    }
    for (&start, &(end, kind)) in constructors {
        if !seen.contains(&start) {
            let line = source
                .get(..start)
                .map_or(1, |before| before.matches('\n').count() + 1);
            groups.push(Group {
                line,
                name: source
                    .get(start..end)
                    .map_or_else(|| "channel".to_owned(), |callee| format!("{callee}(..)")),
                kind,
                edits: Vec::new(),
                refusal: Some(UNBOUND),
            });
        }
    }
    refuse_overlaps(&mut groups);
    for group in groups {
        match group.refusal {
            Some(reason) => outcome.refusals.push(Refusal {
                line: group.line,
                name: group.name,
                kind: group.kind,
                reason,
            }),
            None => outcome.edits.extend(group.edits),
        }
    }
    outcome.refusals.sort_by_key(|refusal| refusal.line);
    outcome
}

struct Group {
    line: usize,
    name: String,
    kind: Kind,
    edits: Vec<Edit>,
    refusal: Option<&'static str>,
}

fn refuse_overlaps(groups: &mut [Group]) {
    let mut overlapping = BTreeSet::new();
    for (index, group) in groups.iter().enumerate() {
        for (other_index, other) in groups.iter().enumerate().skip(index + 1) {
            if group.refusal.is_some() || other.refusal.is_some() {
                continue;
            }
            let touches = group.edits.iter().any(|edit| {
                other
                    .edits
                    .iter()
                    .any(|other| edit.start <= other.end && other.start <= edit.end)
            });
            if touches {
                overlapping.insert(index);
                overlapping.insert(other_index);
            }
        }
    }
    for index in overlapping {
        if let Some(group) = groups.get_mut(index) {
            group.refusal = Some(OVERLAP);
        }
    }
}

fn group(file: &Path, text: &Text<'_>, found: &Found<'_>) -> Group {
    let pat_text = text.slice(&found.local.pat).unwrap_or("channel");
    let mut group = Group {
        line: text.line(found.local.let_token.span),
        name: pat_text.to_owned(),
        kind: found.kind,
        edits: Vec::new(),
        refusal: found.refusal,
    };
    if group.refusal.is_none() {
        match rewrite(file, text, found) {
            Ok(edits) => group.edits = edits,
            Err(reason) => group.refusal = Some(reason),
        }
    }
    group
}
