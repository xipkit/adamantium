//! What running a local body may do to the thread it runs on, and the fixed
//! point that computes it for recursive code.

use std::collections::{BTreeSet, VecDeque};

use rustc_data_structures::fx::{FxHashMap, FxHashSet, FxIndexSet};
use rustc_hir::def_id::LocalDefId;
use rustc_lint::LateContext;
use rustc_span::Span;

use crate::{LintKind, Walker};

/// Where code runs, which decides which effects are violations.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Context {
    /// Code runs while a future is polled.
    pub(crate) asynchronous: bool,
    /// Code runs inside an actor's `with` or `read` callback.
    pub(crate) actor: bool,
    /// Code runs inside the work of a limit.
    pub(crate) limit: Option<LimitKind>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum LimitKind {
    Ordinary,
    Keyed,
}

impl Context {
    /// The context of code that runs in `inner`, relative to a body, when
    /// that body runs in `self`.
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

/// Something a body may do on the thread that runs it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Performs a known blocking operation.
    Blocking,
    /// Calls `Actor::with`, `read` or `snapshot`.
    Actor,
    /// Polls `Limit::run` or `run_until`.
    Limit,
    /// Polls `KeyedLimit::run` or `run_until`.
    KeyedLimit,
}

impl Effect {
    pub(crate) const ALL: [Self; 4] = [Self::Blocking, Self::Actor, Self::Limit, Self::KeyedLimit];

    /// The lint `self` violates in `context`, if any. Limit work may enter
    /// a `KeyedLimit`; keyed work may enter no limit at all. Blocking inside
    /// an actor callback holds the actor's lock while it waits, and the
    /// callback is synchronous, so it is reported as such even when the
    /// callback runs in async code: the work has to move out of the callback
    /// either way.
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

    /// The note on the operation at the end of a chain of calls.
    pub(crate) fn operation(self) -> &'static str {
        match self {
            Self::Blocking => "the blocking operation",
            Self::Actor => "the actor access",
            Self::Limit | Self::KeyedLimit => "the limit entered",
        }
    }
}

/// Where a body first produced an effect.
#[derive(Clone, Copy)]
pub(crate) struct Origin {
    pub(crate) span: Span,
    /// The local body whose summary carried the effect to `span`, or `None`
    /// when `span` is the operation itself.
    pub(crate) via: Option<LocalDefId>,
}

/// A use of a value whose type is one of a body's generic parameters, which
/// only the body's callers can follow.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Use {
    /// The parameter's index into the generics that callers instantiate.
    pub(crate) param: u32,
    pub(crate) how: How,
    /// The context the body adds around the use, such as an actor callback
    /// or limit work that the body enters before invoking the value.
    pub(crate) context: Context,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum How {
    /// Calls it inline, like a closure parameter.
    Invoke,
    /// Awaits it, like a future parameter.
    Poll,
}

/// What running a body may do, whatever context it runs in.
///
/// For a function this is calling it. For a coroutine this is polling it,
/// so calling an async function, which only constructs its future, has no
/// effects; awaiting the future applies its coroutine's summary.
#[derive(Clone, Default)]
pub(crate) struct Summary {
    effects: [Option<Origin>; Effect::ALL.len()],
    pub(crate) uses: BTreeSet<Use>,
}

impl Summary {
    pub(crate) fn has(&self, effect: Effect) -> bool {
        self.effects[effect as usize].is_some()
    }

    /// Keeps the first origin of each effect. An origin that goes through
    /// another body was recorded after that body had the effect, so
    /// following origins always ends at an operation, even in recursion.
    pub(crate) fn record(&mut self, effect: Effect, origin: Origin) {
        self.effects[effect as usize].get_or_insert(origin);
    }

    /// Adds what `other` found and reports whether anything was new.
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

/// Summaries of the local bodies needed so far.
///
/// Each summary starts empty and only grows, and there are finitely many
/// effects and parameters, so recomputing a body whenever a summary it read
/// has grown reaches the least fixed point: a function that could block only
/// by calling itself does not block, and one that blocks on any path through
/// any depth of recursion does. The bodies a walk reads can grow as callback
/// summaries grow, so dependencies are recorded per walk rather than taken
/// from a call graph built beforehand.
#[derive(Default)]
pub(crate) struct Summaries {
    table: FxHashMap<LocalDefId, Summary>,
    /// For each body, the bodies whose walks read its summary.
    dependents: FxHashMap<LocalDefId, FxIndexSet<LocalDefId>>,
    pending: VecDeque<LocalDefId>,
    queued: FxHashSet<LocalDefId>,
}

impl Summaries {
    /// The final summary of `node`, solving every summary it depends on.
    ///
    /// Once the queue is empty, every summary in the table is final: a body
    /// that a later request adds was not read by any earlier walk.
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

    /// The current, possibly partial, summary of `node` for the walk of
    /// `reader`, which is recomputed if `node`'s summary grows.
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

    /// The origins of `effect` from the body `via` down to the operation.
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
