#![feature(rustc_private)]

extern crate rustc_data_structures;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_lint;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

mod effects;

use std::collections::HashSet;

use rustc_hir::{
    Body, Expr, ExprKind, HirId, MatchSource,
    attrs::lang_items::LangItem,
    def::DefKind,
    def_id::{DefId, LocalDefId},
    intravisit::{self, Visitor},
};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, GenericArgsRef, Ty, TypeVisitableExt};
use rustc_session::{declare_lint, impl_lint_pass};

use effects::{Context, Effect, How, LimitKind, Origin, Summaries, Summary, Use};

dylint_linting::dylint_library!();

declare_lint! {
    pub ADAMANTIUM_BLOCKING_IN_ASYNC,
    Warn,
    "a known blocking operation executes on an async task"
}
declare_lint! {
    pub ADAMANTIUM_BLOCKING_IN_ACTOR,
    Warn,
    "a known blocking operation executes inside an actor callback"
}
declare_lint! {
    pub ADAMANTIUM_NESTED_ACTOR_ACCESS,
    Warn,
    "actor access executes inside another actor callback"
}
declare_lint! {
    pub ADAMANTIUM_NESTED_LIMIT,
    Warn,
    "a Limit or KeyedLimit is entered inside limit work that forbids it"
}
declare_lint! {
    pub ADAMANTIUM_RACE_WITH_TIMER,
    Warn,
    "adamantium::race pairs a timer with a future that may not be drop-safe"
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LintKind {
    BlockingInAsync,
    BlockingInActor,
    NestedActor,
    NestedLimit,
    RaceWithTimer,
}

impl LintKind {
    /// The lint, its message and its help.
    fn diagnostic(self) -> (&'static rustc_lint::Lint, &'static str, &'static str) {
        match self {
            Self::BlockingInAsync => (
                ADAMANTIUM_BLOCKING_IN_ASYNC,
                "blocking operation executes inside async code",
                "use adamantium::blocking or Tasks::spawn_blocking for blocking work",
            ),
            Self::BlockingInActor => (
                ADAMANTIUM_BLOCKING_IN_ACTOR,
                "blocking operation holds an actor's lock while it waits",
                "do the blocking work before or after the callback, and record its result in the actor",
            ),
            Self::NestedActor => (
                ADAMANTIUM_NESTED_ACTOR_ACCESS,
                "actor access inside an actor callback panics at runtime",
                "access the second actor after the outer callback returns",
            ),
            Self::NestedLimit => (
                ADAMANTIUM_NESTED_LIMIT,
                "limit entered inside limit work panics at runtime",
                "enter only a KeyedLimit inside Limit work, and other limits after the work finishes",
            ),
            Self::RaceWithTimer => (
                ADAMANTIUM_RACE_WITH_TIMER,
                "racing a timer against a future that may not be drop-safe",
                "use adamantium::until or a *_until method, or race only documented drop-safe operations",
            ),
        }
    }
}

#[derive(Default)]
struct Execution {
    reported: HashSet<(HirId, LintKind)>,
    visited: HashSet<(HirId, Context)>,
    summaries: Summaries,
}
impl_lint_pass!(
    Execution => [
        ADAMANTIUM_BLOCKING_IN_ASYNC,
        ADAMANTIUM_BLOCKING_IN_ACTOR,
        ADAMANTIUM_NESTED_ACTOR_ACCESS,
        ADAMANTIUM_NESTED_LIMIT,
        ADAMANTIUM_RACE_WITH_TIMER,
    ]
);

/// Registers the experimental execution-context lints with Dylint.
#[unsafe(no_mangle)]
pub fn register_lints(_: &rustc_session::Session, store: &mut rustc_lint::LintStore) {
    store.register_lints(&[
        ADAMANTIUM_BLOCKING_IN_ASYNC,
        ADAMANTIUM_BLOCKING_IN_ACTOR,
        ADAMANTIUM_NESTED_ACTOR_ACCESS,
        ADAMANTIUM_NESTED_LIMIT,
        ADAMANTIUM_RACE_WITH_TIMER,
    ]);
    store.register_late_lint_pass(Box::new(|_| Box::<Execution>::default()));
}

impl<'tcx> LateLintPass<'tcx> for Execution {
    fn check_body(&mut self, cx: &LateContext<'tcx>, body: &Body<'tcx>) {
        let body = cx.tcx.hir_body(body.id());
        let owner = cx.tcx.hir_body_owner_def_id(body.id());
        // Constant arguments can be type-checked separately from their HIR
        // owner. They cannot execute runtime I/O or actor methods; nested
        // runtime closures are still checked through their own bodies.
        if !matches!(
            cx.tcx.def_kind(owner),
            DefKind::Fn | DefKind::AssocFn | DefKind::Closure
        ) {
            return;
        }
        let context = Context {
            asynchronous: is_async(cx, owner),
            ..Context::default()
        };
        Walker {
            cx,
            summaries: &mut self.summaries,
            context,
            mode: Mode::Check {
                reported: &mut self.reported,
                visited: &mut self.visited,
            },
        }
        .visit_body(body);
    }
}

/// How a walk uses what it finds.
enum Mode<'a> {
    /// Reports effects that conflict with the walk's context.
    Check {
        reported: &'a mut HashSet<(HirId, LintKind)>,
        visited: &'a mut HashSet<(HirId, Context)>,
    },
    /// Collects the effects of one body, whatever its caller's context.
    Summarize {
        node: LocalDefId,
        root: DefId,
        summary: Summary,
        visited: HashSet<(HirId, Context)>,
    },
}

/// Follows one body through the code that runs synchronously with it.
///
/// Closures that are invoked inline and futures that are polled by an
/// `.await` are lexically part of the walked body, so the walk enters them
/// with the context they run in and reports inside them. Local functions and
/// the futures of local async functions run wherever they are called from,
/// so they are represented by their [`Summary`] and reported where they are
/// called or awaited.
struct Walker<'a, 'tcx> {
    cx: &'a LateContext<'tcx>,
    summaries: &'a mut Summaries,
    context: Context,
    mode: Mode<'a>,
}

impl<'a, 'tcx> Walker<'a, 'tcx> {
    /// The effects of running `node`'s body. Reads of other summaries are
    /// recorded as dependencies so that [`Summaries`] can recompute `node`
    /// when one of them grows.
    fn summarize(
        cx: &'a LateContext<'tcx>,
        summaries: &'a mut Summaries,
        node: LocalDefId,
    ) -> Summary {
        let tcx = cx.tcx;
        // An opaque future stands for its hidden type, which is polled from
        // the expression its defining function returns.
        let opaque = tcx.def_kind(node) == DefKind::OpaqueTy;
        let owner = if opaque { tcx.local_parent(node) } else { node };
        let Some(body) = tcx.hir_maybe_body_owned_by(owner) else {
            return Summary::default();
        };
        let mut walker = Walker {
            cx,
            summaries,
            context: Context::default(),
            mode: Mode::Summarize {
                node,
                root: tcx.typeck_root_def_id(owner.to_def_id()),
                summary: Summary::default(),
                visited: HashSet::new(),
            },
        };
        if opaque {
            let hidden = tcx.type_of(node).instantiate_identity().skip_norm_wip();
            walker.poll(tail(body.value), hidden, Context::default());
        } else {
            walker.visit_body(body);
        }
        match walker.mode {
            Mode::Summarize { summary, .. } => summary,
            Mode::Check { .. } => unreachable!("a summary walk stays in summary mode"),
        }
    }

    fn types(&self, expr: &'tcx Expr<'tcx>) -> &'tcx ty::TypeckResults<'tcx> {
        self.cx.tcx.typeck(expr.hir_id.owner.def_id)
    }

    fn within(&mut self, context: Context, walk: impl FnOnce(&mut Self)) {
        let previous = std::mem::replace(&mut self.context, context);
        walk(self);
        self.context = previous;
    }

    /// Records or reports `effect` happening at `site`. `via` names the local
    /// body whose summary produced it; `None` means `site` is the operation.
    ///
    /// Blocking is not reported where a future is polled: the future's own
    /// body is async code, so its blocking calls are reported there.
    fn occur(
        &mut self,
        effect: Effect,
        site: &'tcx Expr<'tcx>,
        via: Option<LocalDefId>,
        polled: bool,
    ) {
        let context = self.context;
        match &mut self.mode {
            Mode::Summarize { summary, .. } => summary.record(
                effect,
                Origin {
                    span: site.span.source_callsite(),
                    via,
                },
            ),
            Mode::Check { reported, .. } => {
                if let Some(lint) = effect.violation(context)
                    && !(polled && effect == Effect::Blocking)
                    && reported.insert((site.hir_id, lint))
                {
                    let notes = self.summaries.chain(effect, via);
                    report(self.cx, lint, effect, site, notes);
                }
            }
        }
    }

    /// Records that the walked body invokes or polls a value of the generic
    /// type parameter `param`, which the caller's arguments decide.
    fn record_use(&mut self, site: &'tcx Expr<'tcx>, param: u32, how: How) {
        let site_root = self
            .cx
            .tcx
            .typeck_root_def_id(site.hir_id.owner.to_def_id());
        let context = self.context;
        if let Mode::Summarize { root, summary, .. } = &mut self.mode
            && *root == site_root
        {
            summary.uses.insert(Use {
                param,
                how,
                context,
            });
        }
    }

    fn summary(&mut self, node: LocalDefId) -> Summary {
        match &self.mode {
            Mode::Check { .. } => self.summaries.solved(self.cx, node),
            Mode::Summarize { node: reader, .. } => self.summaries.read(*reader, node),
        }
    }

    /// Applies the summary of `node`, instantiated with `args`, at `site`.
    fn apply(
        &mut self,
        site: &'tcx Expr<'tcx>,
        node: LocalDefId,
        args: GenericArgsRef<'tcx>,
        polled: bool,
    ) {
        let summary = self.summary(node);
        for effect in Effect::ALL {
            if summary.has(effect) {
                self.occur(effect, site, Some(node), polled);
            }
        }
        for used in summary.uses {
            if let Some(ty) = args.get(used.param as usize).and_then(|arg| arg.as_type()) {
                let context = self.context.enter(used.context);
                match used.how {
                    How::Invoke => self.invoke(site, ty, context),
                    How::Poll => self.poll(site, ty, context),
                }
            }
        }
    }

    /// Follows a call of a value of type `ty` at `site`, run in `context`.
    fn invoke(&mut self, site: &'tcx Expr<'tcx>, ty: Ty<'tcx>, context: Context) {
        self.within(context, |walker| match *ty.peel_refs().kind() {
            ty::Closure(id, _) => {
                if let Some(id) = id.as_local()
                    && let Some(body) = walker.cx.tcx.hir_maybe_body_owned_by(id)
                {
                    walker.visit_body(body);
                }
            }
            ty::FnDef(id, args) => {
                let args = args.skip_binder();
                let receiver = walker
                    .cx
                    .tcx
                    .trait_of_assoc(id)
                    .and_then(|_| args.get(0))
                    .and_then(|arg| arg.as_type());
                walker.call(site, id, args, receiver, &[]);
            }
            ty::Param(param) => walker.record_use(site, param.index, How::Invoke),
            _ => {}
        });
    }

    /// Follows the polling of a future of type `ty` at `site`, run in
    /// `context`. Only polling runs a future's body; constructing it does not.
    fn poll(&mut self, site: &'tcx Expr<'tcx>, ty: Ty<'tcx>, context: Context) {
        let tcx = self.cx.tcx;
        let polled = Context {
            asynchronous: true,
            ..context
        };
        self.within(polled, |walker| match *pointee(ty).kind() {
            ty::Coroutine(id, _) => {
                if let Some(id) = id.as_local()
                    && is_async(walker.cx, id)
                    && let Some(body) = tcx.hir_maybe_body_owned_by(id)
                {
                    walker.visit_body(body);
                }
            }
            ty::Alias(_, alias) => {
                let ty::AliasTyKind::Opaque { def_id } = alias.kind else {
                    return;
                };
                if let Some(opaque) = def_id.as_local() {
                    // An async function's future is its coroutine. Any other
                    // local future, such as another function's future that a
                    // helper returns, is summarized as its hidden type.
                    if let ty::Coroutine(coroutine, _) = *tcx
                        .type_of(def_id)
                        .instantiate_identity()
                        .skip_norm_wip()
                        .kind()
                        && let Some(coroutine) = coroutine.as_local()
                        && is_async(walker.cx, coroutine)
                        && tcx.hir_maybe_body_owned_by(coroutine).is_some()
                    {
                        walker.apply(site, coroutine, alias.args, true);
                    } else {
                        walker.apply(site, opaque, alias.args, true);
                    }
                } else if let Some(function) = tcx.opt_parent(def_id)
                    && let Some(kind) = limit_kind(walker.cx, function)
                {
                    walker.enter_limit(site, kind, function, alias.args);
                }
            }
            ty::Param(param) => walker.record_use(site, param.index, How::Poll),
            _ => {}
        });
    }

    /// Follows the polling of a `Limit` or `KeyedLimit` `run` future: the
    /// limit is entered, then its work is called and the work's future is
    /// polled while the place is held.
    fn enter_limit(
        &mut self,
        site: &'tcx Expr<'tcx>,
        kind: LimitKind,
        function: DefId,
        args: GenericArgsRef<'tcx>,
    ) {
        let tcx = self.cx.tcx;
        self.occur(kind.effect(), site, None, true);
        let args = args.truncate_to(tcx, tcx.generics_of(function));
        let signature = tcx.fn_sig(function).instantiate(tcx, args).skip_norm_wip();
        let Some(&work) = signature.skip_binder().inputs().last() else {
            return;
        };
        let held = Context {
            limit: Some(kind),
            ..self.context
        };
        self.invoke(site, work, held);
        if let Some(future) = work_future(self.cx, function, args, work) {
            self.poll(site, future, held);
        }
    }

    fn call(
        &mut self,
        site: &'tcx Expr<'tcx>,
        id: DefId,
        args: GenericArgsRef<'tcx>,
        receiver: Option<Ty<'tcx>>,
        arguments: &'tcx [Expr<'tcx>],
    ) {
        let cx = self.cx;
        let name = cx.tcx.item_name(id).as_str().to_owned();
        let path = definition_path(cx, id);
        let owner = cx
            .tcx
            .inherent_impl_of_assoc(id)
            .map(|id| cx.tcx.type_of(id).instantiate_identity().skip_norm_wip());
        let owner_path = owner.and_then(|ty| adt_path(cx, ty));
        let actor = owner_path.as_deref() == Some("adamantium::actor::Actor")
            && matches!(name.as_str(), "with" | "read" | "snapshot");
        if actor {
            self.occur(Effect::Actor, site, None, false);
        }
        if blocking(cx, &path, &name, owner, receiver) {
            self.occur(Effect::Blocking, site, None, false);
        }
        if actor && name != "snapshot" {
            if let Some(callback) = arguments.last() {
                let context = Context {
                    actor: true,
                    ..self.context
                };
                self.invoke(site, self.types(callback).expr_ty(callback), context);
            }
        } else if inline_callback(&path, &name, owner_path.as_deref()) {
            for arg in arguments
                .iter()
                .rev()
                .take(if name == "map_or_else" { 2 } else { 1 })
            {
                self.invoke(site, self.types(arg).expr_ty(arg), self.context);
            }
        } else if let Some((node, args)) = self.resolve(site, id, args) {
            self.apply(site, node, args, false);
        }
        // Unknown callback APIs are not followed. Blocking-pool callbacks
        // consequently start in their own context. Argument expressions are
        // still visited in the caller context by walk_expr.
    }

    /// The local function body that a call of `id` with `args` runs, if it
    /// can be named without knowing the caller's generic parameters.
    fn resolve(
        &self,
        site: &'tcx Expr<'tcx>,
        id: DefId,
        args: GenericArgsRef<'tcx>,
    ) -> Option<(LocalDefId, GenericArgsRef<'tcx>)> {
        let tcx = self.cx.tcx;
        let (id, args) = if tcx.trait_of_assoc(id).is_some() {
            if args.has_escaping_bound_vars() {
                return None;
            }
            let root = tcx.typeck_root_def_id(site.hir_id.owner.to_def_id());
            let typing_env = ty::TypingEnv::post_analysis(tcx, root);
            let args = tcx.erase_and_anonymize_regions(args);
            let instance = ty::Instance::try_resolve(tcx, typing_env, id, args).ok()??;
            let ty::InstanceKind::Item(id) = instance.def else {
                return None;
            };
            (id, instance.args)
        } else {
            (id, args)
        };
        // Coroutine bodies are polled, never called; `poll` follows them.
        let local = id.as_local()?;
        (matches!(tcx.def_kind(id), DefKind::Fn | DefKind::AssocFn)
            && tcx.hir_maybe_body_owned_by(local).is_some())
        .then_some((local, args))
    }

    fn race(
        &mut self,
        site: &'tcx Expr<'tcx>,
        id: DefId,
        receiver: Option<&'tcx Expr<'tcx>>,
        arguments: &'tcx [Expr<'tcx>],
    ) {
        let Mode::Check { reported, .. } = &mut self.mode else {
            return;
        };
        let path = definition_path(self.cx, id);
        if let Some(arms) = race_arms(&path, receiver, arguments)
            && race_has_unsafe_timer(self.cx, arms)
            && reported.insert((site.hir_id, LintKind::RaceWithTimer))
        {
            let (lint, message, help) = LintKind::RaceWithTimer.diagnostic();
            self.cx.tcx.emit_node_span_lint(
                lint,
                site.hir_id,
                site.span.source_callsite(),
                rustc_errors::DiagDecorator(|diag| {
                    diag.primary_message(message);
                    diag.help(help);
                }),
            );
        }
    }
}

impl<'tcx> Visitor<'tcx> for Walker<'_, 'tcx> {
    fn visit_body(&mut self, body: &Body<'tcx>) {
        // A closure can be invoked repeatedly through aliases. The context
        // bounds traversal to a few visits per body instead of expanding a
        // potentially exponential call tree.
        let key = (body.id().hir_id, self.context);
        let first = match &mut self.mode {
            Mode::Check { visited, .. } => visited.insert(key),
            Mode::Summarize { visited, .. } => visited.insert(key),
        };
        if first {
            intravisit::walk_body(self, body);
        }
    }

    fn visit_expr(&mut self, expr: &'tcx Expr<'tcx>) {
        match expr.kind {
            ExprKind::Closure(_) => return,
            ExprKind::MethodCall(_, receiver, args, _) => {
                let types = self.types(expr);
                if let Some(id) = types.type_dependent_def_id(expr.hir_id) {
                    self.race(expr, id, Some(receiver), args);
                    let receiver_ty = types.expr_ty(receiver);
                    self.call(
                        expr,
                        id,
                        types.node_args(expr.hir_id),
                        Some(receiver_ty),
                        args,
                    );
                }
            }
            ExprKind::Call(function, args) => {
                let types = self.types(expr);
                let callee = types.expr_ty(function);
                if let ty::FnDef(id, generic) = *callee.kind() {
                    self.race(expr, id, args.first(), args);
                    let receiver = args.first().map(|arg| types.expr_ty(arg));
                    self.call(expr, id, generic.skip_binder(), receiver, args);
                } else {
                    self.invoke(expr, callee, self.context);
                }
            }
            ExprKind::Match(scrutinee, _, MatchSource::AwaitDesugar) => {
                // Constructing the future runs first, then it is polled.
                intravisit::walk_expr(self, expr);
                if let Some(future) = awaited(self.cx, scrutinee) {
                    self.poll(expr, self.types(future).expr_ty(future), self.context);
                }
                return;
            }
            _ => {}
        }
        intravisit::walk_expr(self, expr);
    }
}

fn report<'tcx>(
    cx: &LateContext<'tcx>,
    kind: LintKind,
    effect: Effect,
    site: &'tcx Expr<'tcx>,
    notes: Vec<Origin>,
) {
    let (lint, message, help) = kind.diagnostic();
    let last = notes.len().saturating_sub(1);
    cx.tcx.emit_node_span_lint(
        lint,
        site.hir_id,
        site.span.source_callsite(),
        rustc_errors::DiagDecorator(|diag| {
            diag.primary_message(message);
            for (index, origin) in notes.iter().enumerate() {
                if index == last {
                    diag.span_note(origin.span, effect.operation());
                } else {
                    diag.span_note(origin.span, "reached through this call");
                }
            }
            diag.help(help);
        }),
    );
}

fn is_async(cx: &LateContext<'_>, id: LocalDefId) -> bool {
    cx.tcx
        .coroutine_kind(id)
        .is_some_and(|kind| kind.is_async_desugaring())
}

/// The future an `.await` polls, before `IntoFuture::into_future`.
fn awaited<'tcx>(cx: &LateContext<'tcx>, scrutinee: &'tcx Expr<'tcx>) -> Option<&'tcx Expr<'tcx>> {
    if let ExprKind::Call(function, [future]) = scrutinee.kind
        && let ty::FnDef(id, _) = cx
            .tcx
            .typeck(scrutinee.hir_id.owner.def_id)
            .expr_ty(function)
            .kind()
        && definition_path(cx, *id) == "core::future::into_future::IntoFuture::into_future"
    {
        Some(future)
    } else {
        None
    }
}

/// The future polled through references, `Pin` and `Box`.
fn pointee(mut ty: Ty<'_>) -> Ty<'_> {
    loop {
        ty = match *ty.kind() {
            ty::Ref(_, inner, _) => inner,
            ty::Adt(def, args) if def.is_box() || def.is_pin() => args.type_at(0),
            _ => return ty,
        };
    }
}

/// The future that limit work returns, from the `FnOnce` bound on the
/// work in `function`'s signature. A generic work type keeps its future
/// generic, so a helper that wraps a limit records polling it.
fn work_future<'tcx>(
    cx: &LateContext<'tcx>,
    function: DefId,
    args: GenericArgsRef<'tcx>,
    work: Ty<'tcx>,
) -> Option<Ty<'tcx>> {
    cx.tcx
        .clauses_of(function)
        .instantiate_own(cx.tcx, args)
        .find_map(|(clause, _)| {
            let projection = clause.skip_norm_wip().as_projection_clause()?.skip_binder();
            (cx.tcx
                .is_lang_item(projection.def_id().into(), LangItem::FnOnceOutput)
                && projection.self_ty() == work)
                .then(|| projection.term.as_type())
                .flatten()
        })
}

/// The expression a body evaluates to, inside any enclosing blocks.
fn tail<'tcx>(mut expr: &'tcx Expr<'tcx>) -> &'tcx Expr<'tcx> {
    while let ExprKind::Block(block, _) = expr.kind
        && let Some(inner) = block.expr
    {
        expr = inner;
    }
    expr
}

/// Which limit a `run` or `run_until` function enters, if any.
fn limit_kind(cx: &LateContext<'_>, function: DefId) -> Option<LimitKind> {
    if !matches!(cx.tcx.item_name(function).as_str(), "run" | "run_until") {
        return None;
    }
    let owner = cx.tcx.inherent_impl_of_assoc(function)?;
    let owner = adt_path(
        cx,
        cx.tcx.type_of(owner).instantiate_identity().skip_norm_wip(),
    )?;
    match owner.as_str() {
        "adamantium::limit::Limit" => Some(LimitKind::Ordinary),
        "adamantium::limit::keyed::KeyedLimit" => Some(LimitKind::Keyed),
        _ => None,
    }
}

fn definition_path(cx: &LateContext<'_>, id: DefId) -> String {
    let mut path = cx.tcx.crate_name(id.krate).to_string();
    for segment in cx.tcx.def_path(id).data {
        if let Some(name) = segment.data.get_opt_name() {
            path.push_str("::");
            path.push_str(name.as_str());
        }
    }
    path
}

fn adt_path(cx: &LateContext<'_>, ty: Ty<'_>) -> Option<String> {
    match ty.peel_refs().kind() {
        ty::Adt(def, _) => Some(definition_path(cx, def.did())),
        _ => None,
    }
}

fn inline_callback(path: &str, name: &str, owner: Option<&str>) -> bool {
    (matches!(owner, Some("core::option::Option" | "core::result::Result"))
        && matches!(
            name,
            "map"
                | "map_err"
                | "map_or"
                | "map_or_else"
                | "and_then"
                | "or_else"
                | "unwrap_or_else"
                | "inspect"
                | "inspect_err"
        ))
        || (path.starts_with("core::iter::traits::iterator::Iterator::")
            && matches!(
                name,
                "for_each"
                    | "try_for_each"
                    | "fold"
                    | "try_fold"
                    | "any"
                    | "all"
                    | "find"
                    | "find_map"
                    | "position"
            ))
}

fn blocking(
    cx: &LateContext<'_>,
    path: &str,
    name: &str,
    owner: Option<Ty<'_>>,
    receiver: Option<Ty<'_>>,
) -> bool {
    if matches!(
        path,
        "std::thread::functions::sleep"
            | "std::thread::functions::sleep_ms"
            | "std::thread::functions::park"
            | "std::thread::functions::park_timeout"
            | "std::fs::read"
            | "std::fs::read_to_string"
            | "std::fs::write"
            | "std::fs::copy"
            | "std::fs::rename"
            | "std::fs::remove_file"
            | "std::fs::remove_dir"
            | "std::fs::remove_dir_all"
            | "std::fs::create_dir"
            | "std::fs::create_dir_all"
            | "std::fs::read_dir"
            | "std::fs::metadata"
            | "std::fs::symlink_metadata"
            | "std::fs::canonicalize"
            | "std::fs::read_link"
            | "std::fs::hard_link"
            | "std::fs::set_permissions"
    ) {
        return true;
    }
    let owner = owner.and_then(|ty| adt_path(cx, ty));
    // Only standard I/O traits count. A user trait named `read` on a File
    // is not evidence of blocking I/O.
    let io_trait = [
        "alloc::io::read::Read::",
        "core::io::write::Write::",
        "core::io::seek::Seek::",
        "std::io::Seek::",
        "std::io::Read::",
        "std::io::Write::",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix));
    let receiver = receiver
        .filter(|_| io_trait)
        .and_then(|ty| adt_path(cx, ty));
    match owner.as_deref().or(receiver.as_deref()) {
        Some("std::fs::File") => matches!(
            name,
            "open"
                | "create"
                | "create_new"
                | "sync_all"
                | "sync_data"
                | "set_len"
                | "metadata"
                | "set_permissions"
                | "read"
                | "read_exact"
                | "read_to_end"
                | "read_to_string"
                | "write"
                | "write_all"
                | "flush"
                | "seek"
                | "stream_position"
                | "rewind"
        ),
        Some("std::fs::OpenOptions") => name == "open",
        Some("std::path::Path") => matches!(
            name,
            "exists"
                | "try_exists"
                | "is_file"
                | "is_dir"
                | "metadata"
                | "symlink_metadata"
                | "canonicalize"
                | "read_link"
                | "read_dir"
        ),
        Some(
            "std::net::tcp::TcpStream" | "std::net::tcp::TcpListener" | "std::net::udp::UdpSocket",
        ) => matches!(
            name,
            "connect"
                | "connect_timeout"
                | "bind"
                | "accept"
                | "read"
                | "read_exact"
                | "read_to_end"
                | "read_to_string"
                | "write"
                | "write_all"
                | "flush"
                | "peek"
                | "recv"
                | "recv_from"
                | "send"
                | "send_to"
                | "peek_from"
        ),
        Some("std::process::Command") => matches!(name, "status" | "output"),
        Some("std::process::Child") => matches!(name, "wait" | "wait_with_output"),
        Some("std::thread::join_handle::JoinHandle") => name == "join",
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FutureSafety {
    Timer,
    DropSafe,
    Other,
}

struct ArmKind {
    has_timer: bool,
    all_drop_safe: bool,
}

fn peel_wrappers<'tcx>(mut expr: &'tcx Expr<'tcx>) -> &'tcx Expr<'tcx> {
    loop {
        expr = match expr.kind {
            ExprKind::DropTemps(inner) => inner,
            ExprKind::Block(block, _) if block.stmts.is_empty() => match block.expr {
                Some(inner) => inner,
                None => return expr,
            },
            _ => return expr,
        };
    }
}

fn race_arms<'tcx>(
    path: &str,
    receiver: Option<&'tcx Expr<'tcx>>,
    args: &'tcx [Expr<'tcx>],
) -> Option<&'tcx [Expr<'tcx>]> {
    let tuple = match path {
        "adamantium::race::race" if args.len() == 1 => &args[0],
        "adamantium::race::Race::race" => {
            if args.is_empty() {
                receiver?
            } else if args.len() == 1 {
                &args[0]
            } else {
                return None;
            }
        }
        _ => return None,
    };
    match peel_wrappers(tuple).kind {
        ExprKind::Tup(arms) => Some(arms),
        _ => None,
    }
}

fn race_has_unsafe_timer<'tcx>(cx: &LateContext<'tcx>, arms: &'tcx [Expr<'tcx>]) -> bool {
    let mut any_timer = false;
    let mut all_drop_safe = true;
    for arm in arms {
        let kind = classify_race_arm(cx, arm);
        any_timer |= kind.has_timer;
        all_drop_safe &= kind.all_drop_safe;
    }
    any_timer && !all_drop_safe
}

fn classify_race_arm<'tcx>(cx: &LateContext<'tcx>, arm: &'tcx Expr<'tcx>) -> ArmKind {
    let arm = peel_wrappers(arm);
    if let ExprKind::Closure(closure) = arm.kind
        && cx
            .tcx
            .coroutine_kind(closure.def_id)
            .is_some_and(|kind| kind.is_async_desugaring())
    {
        let body = cx.tcx.hir_body(closure.body);
        let mut scanner = AwaitScanner {
            cx,
            has_timer: false,
            all_drop_safe: true,
        };
        scanner.visit_expr(body.value);
        return ArmKind {
            has_timer: scanner.has_timer,
            all_drop_safe: scanner.all_drop_safe,
        };
    }
    match classify_future(cx, arm) {
        FutureSafety::Timer => ArmKind {
            has_timer: true,
            all_drop_safe: true,
        },
        FutureSafety::DropSafe => ArmKind {
            has_timer: false,
            all_drop_safe: true,
        },
        FutureSafety::Other => ArmKind {
            has_timer: false,
            all_drop_safe: false,
        },
    }
}

struct AwaitScanner<'a, 'tcx> {
    cx: &'a LateContext<'tcx>,
    has_timer: bool,
    all_drop_safe: bool,
}

impl<'tcx> Visitor<'tcx> for AwaitScanner<'_, 'tcx> {
    fn visit_expr(&mut self, expr: &'tcx Expr<'tcx>) {
        if let ExprKind::Closure(_) = expr.kind {
            return;
        }
        if let ExprKind::Match(scrutinee, _, MatchSource::AwaitDesugar) = expr.kind
            && let Some(future) = awaited(self.cx, scrutinee)
        {
            match classify_future(self.cx, future) {
                FutureSafety::Timer => self.has_timer = true,
                FutureSafety::DropSafe => {}
                FutureSafety::Other => self.all_drop_safe = false,
            }
            self.visit_expr(future);
            return;
        }
        intravisit::walk_expr(self, expr);
    }
}

fn classify_future<'tcx>(cx: &LateContext<'tcx>, expr: &'tcx Expr<'tcx>) -> FutureSafety {
    let expr = peel_wrappers(expr);
    let typeck = cx.tcx.typeck(expr.hir_id.owner.def_id);
    let id = match expr.kind {
        ExprKind::Call(function, _) => match typeck.expr_ty(function).kind() {
            ty::FnDef(id, _) => *id,
            _ => return FutureSafety::Other,
        },
        ExprKind::MethodCall(_, _, _, _) => match typeck.type_dependent_def_id(expr.hir_id) {
            Some(id) => id,
            None => return FutureSafety::Other,
        },
        _ => return FutureSafety::Other,
    };
    let path = definition_path(cx, id);
    if matches!(
        path.as_str(),
        "adamantium::time::sleep" | "adamantium::time::sleep_until"
    ) {
        return FutureSafety::Timer;
    }
    let name = cx.tcx.item_name(id);
    let owner_path = cx
        .tcx
        .inherent_impl_of_assoc(id)
        .map(|impl_id| {
            cx.tcx
                .type_of(impl_id)
                .instantiate_identity()
                .skip_norm_wip()
        })
        .and_then(|ty| adt_path(cx, ty));
    let drop_safe = matches!(
        (owner_path.as_deref(), name.as_str()),
        (Some("adamantium::channel::Channel"), "receive")
            | (Some("adamantium::channel::Receiver"), "receive")
            | (Some("adamantium::reply::ReplyReceiver"), "receive")
            | (Some("adamantium::reply::Reply"), "closed")
            | (Some("adamantium::fanout::Feed"), "receive")
            | (
                Some("adamantium::watch::Watcher"),
                "receive" | "changed" | "wait_for"
            )
            | (Some("adamantium::tasks::Cancellation"), "cancelled")
    );
    if drop_safe {
        FutureSafety::DropSafe
    } else {
        FutureSafety::Other
    }
}
