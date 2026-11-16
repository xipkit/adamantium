#![feature(rustc_private)]

extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_lint;
extern crate rustc_middle;
extern crate rustc_session;

use std::collections::HashSet;

use rustc_hir::{
    Body, Expr, ExprKind, HirId, MatchSource,
    def::DefKind,
    def_id::DefId,
    intravisit::{self, Visitor},
};
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::ty::{self, Ty};
use rustc_session::{declare_lint, impl_lint_pass};

dylint_linting::dylint_library!();

declare_lint! {
    pub ADAMANTIUM_BLOCKING_IN_ASYNC,
    Warn,
    "a known blocking operation executes on an async task"
}
declare_lint! {
    pub ADAMANTIUM_NESTED_ACTOR_ACCESS,
    Warn,
    "actor access executes inside another actor callback"
}
declare_lint! {
    pub ADAMANTIUM_RACE_WITH_TIMER,
    Warn,
    "adamantium::race pairs a timer with a future that may not be drop-safe"
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum LintKind {
    Blocking,
    NestedActor,
    RaceWithTimer,
}

#[derive(Default)]
struct Execution {
    reported: HashSet<(HirId, LintKind)>,
    visited: HashSet<(HirId, bool, bool)>,
}
impl_lint_pass!(
    Execution => [
        ADAMANTIUM_BLOCKING_IN_ASYNC,
        ADAMANTIUM_NESTED_ACTOR_ACCESS,
        ADAMANTIUM_RACE_WITH_TIMER,
    ]
);

/// Registers the experimental execution-context lints with Dylint.
#[unsafe(no_mangle)]
pub fn register_lints(_: &rustc_session::Session, store: &mut rustc_lint::LintStore) {
    store.register_lints(&[
        ADAMANTIUM_BLOCKING_IN_ASYNC,
        ADAMANTIUM_NESTED_ACTOR_ACCESS,
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
        let asynchronous = cx
            .tcx
            .coroutine_kind(owner)
            .is_some_and(|kind| kind.is_async_desugaring());
        Walker {
            cx,
            reported: &mut self.reported,
            visited: &mut self.visited,
            asynchronous,
            actor: false,
        }
        .visit_body(body);
    }
}

struct Walker<'a, 'tcx> {
    cx: &'a LateContext<'tcx>,
    reported: &'a mut HashSet<(HirId, LintKind)>,
    visited: &'a mut HashSet<(HirId, bool, bool)>,
    asynchronous: bool,
    actor: bool,
}

impl<'tcx> Walker<'_, 'tcx> {
    fn types(&self, expr: &'tcx Expr<'tcx>) -> &'tcx ty::TypeckResults<'tcx> {
        self.cx.tcx.typeck(expr.hir_id.owner.def_id)
    }

    fn function(&self, expr: &'tcx Expr<'tcx>) -> Option<DefId> {
        match self.types(expr).expr_ty(expr).kind() {
            ty::FnDef(id, _) => Some(*id),
            _ => None,
        }
    }

    fn callback(&mut self, expr: &'tcx Expr<'tcx>, asynchronous: bool, actor: bool) {
        // A closure's type retains its identity through local aliases. A future
        // is only constructed here; its coroutine is checked independently.
        if let ty::Closure(id, _) = self.types(expr).expr_ty(expr).peel_refs().kind()
            && let Some(id) = id.as_local()
            && let Some(body) = self.cx.tcx.hir_maybe_body_owned_by(id)
        {
            let previous = (self.asynchronous, self.actor);
            self.asynchronous = asynchronous;
            self.actor = actor;
            self.visit_body(body);
            (self.asynchronous, self.actor) = previous;
        }
    }

    fn call(
        &mut self,
        expr: &'tcx Expr<'tcx>,
        id: DefId,
        receiver: Option<&'tcx Expr<'tcx>>,
        args: &'tcx [Expr<'tcx>],
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
        if actor && self.actor && self.reported.insert((expr.hir_id, LintKind::NestedActor)) {
            cx.tcx.emit_node_span_lint(
                ADAMANTIUM_NESTED_ACTOR_ACCESS,
                expr.hir_id,
                expr.span.source_callsite(),
                rustc_errors::DiagDecorator(|diag| {
                    diag.primary_message("actor access inside an actor callback panics at runtime");
                    diag.help("access the second actor after the outer callback returns");
                }),
            );
        }
        let receiver_ty = receiver.map(|r| self.types(r).expr_ty(r));
        if self.asynchronous
            && blocking(cx, &path, &name, owner, receiver_ty)
            && self.reported.insert((expr.hir_id, LintKind::Blocking))
        {
            cx.tcx.emit_node_span_lint(
                ADAMANTIUM_BLOCKING_IN_ASYNC,
                expr.hir_id,
                expr.span.source_callsite(),
                rustc_errors::DiagDecorator(|diag| {
                    diag.primary_message("blocking operation executes inside async code");
                    diag.help(
                        "use adamantium::blocking or Tasks::spawn_blocking for blocking work",
                    );
                }),
            );
        }
        if let Some(arms) = race_arms(&path, receiver, args)
            && race_has_unsafe_timer(cx, arms)
            && self.reported.insert((expr.hir_id, LintKind::RaceWithTimer))
        {
            cx.tcx.emit_node_span_lint(
                ADAMANTIUM_RACE_WITH_TIMER,
                expr.hir_id,
                expr.span.source_callsite(),
                rustc_errors::DiagDecorator(|diag| {
                    diag.primary_message("racing a timer against a future that may not be drop-safe");
                    diag.help(
                        "use adamantium::until or a *_until method, or race only documented drop-safe operations",
                    );
                }),
            );
        }
        if actor && name != "snapshot" {
            if let Some(callback) = args.last() {
                self.callback(callback, self.asynchronous, true);
            }
        } else if inline_callback(&path, &name, owner_path.as_deref()) {
            for arg in args
                .iter()
                .rev()
                .take(if name == "map_or_else" { 2 } else { 1 })
            {
                self.callback(arg, self.asynchronous, self.actor);
            }
        }
        // Unknown callback APIs are not followed. Blocking-pool callbacks
        // consequently start in their own context. Argument expressions are
        // still visited in the caller context by walk_expr below.
    }
}

impl<'tcx> Visitor<'tcx> for Walker<'_, 'tcx> {
    fn visit_body(&mut self, body: &Body<'tcx>) {
        // A closure can be invoked repeatedly through aliases. The two context
        // bits bound traversal to four visits per body instead of expanding a
        // potentially exponential call tree.
        if self
            .visited
            .insert((body.id().hir_id, self.asynchronous, self.actor))
        {
            intravisit::walk_body(self, body);
        }
    }

    fn visit_expr(&mut self, expr: &'tcx Expr<'tcx>) {
        match expr.kind {
            ExprKind::Closure(_) => return,
            ExprKind::MethodCall(_, receiver, args, _) => {
                if let Some(id) = self.types(expr).type_dependent_def_id(expr.hir_id) {
                    self.call(expr, id, Some(receiver), args);
                }
            }
            ExprKind::Call(function, args) => {
                if let Some(id) = self.function(function) {
                    self.call(expr, id, args.first(), args);
                } else {
                    self.callback(function, self.asynchronous, self.actor);
                }
            }
            _ => {}
        }
        intravisit::walk_expr(self, expr);
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
            && let ExprKind::Call(func, [future]) = scrutinee.kind
            && let ty::FnDef(id, _) = self
                .cx
                .tcx
                .typeck(scrutinee.hir_id.owner.def_id)
                .expr_ty(func)
                .kind()
            && definition_path(self.cx, *id) == "core::future::into_future::IntoFuture::into_future"
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
            | (Some("adamantium::watch::Watcher"), "receive" | "wait_for")
            | (Some("adamantium::tasks::Cancellation"), "cancelled")
    );
    if drop_safe {
        FutureSafety::DropSafe
    } else {
        FutureSafety::Other
    }
}
