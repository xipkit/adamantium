use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde_json::Value;
use syn::{
    BinOp, Expr, ExprAwait, ExprCall, GenericArgument, Lit, Pat, PathArguments, Stmt, Type,
    UseTree, punctuated::Pair, visit::Visit,
};

use super::{
    Api, Edit, disallowed_path, primary_locations,
    source::{Text, bare, coalesce, names_in},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Call {
    Sleep,
    SleepUntil,
    Now,
    Blocking,
    Rand,
    RandRange,
    Fastrand,
}

impl Call {
    pub(super) fn api(self) -> Api {
        match self {
            Self::Rand | Self::RandRange | Self::Fastrand => Api::Core,
            _ => Api::Tokio,
        }
    }
}

type Calls = BTreeMap<(usize, usize), Call>;

pub(super) const IMPORTS: &[&[&str]] = &[
    &["tokio", "time"],
    &["tokio", "time", "sleep"],
    &["tokio", "time", "sleep_until"],
    &["tokio", "time", "Instant"],
    &["tokio", "task", "spawn_blocking"],
    &["rand", "random"],
    &["rand", "random_range"],
    &["fastrand", "u64"],
];

pub(super) fn callees(diagnostics: Vec<&Value>, root: &Path) -> BTreeMap<PathBuf, Calls> {
    let uses_fastrand_state = diagnostics.iter().any(|diagnostic| {
        matches!(
            disallowed_path(diagnostic, "clippy::disallowed_methods"),
            Some("fastrand::seed" | "fastrand::get_seed")
        )
    });
    let mut files: BTreeMap<PathBuf, Calls> = BTreeMap::new();
    for diagnostic in diagnostics {
        let call = match disallowed_path(diagnostic, "clippy::disallowed_methods") {
            Some("tokio::time::sleep") => Call::Sleep,
            Some("tokio::time::sleep_until") => Call::SleepUntil,
            Some("tokio::time::Instant::now") => Call::Now,
            Some("tokio::task::spawn_blocking") => Call::Blocking,
            Some("rand::random") => Call::Rand,
            Some("rand::random_range") => Call::RandRange,
            Some("fastrand::u64") if !uses_fastrand_state => Call::Fastrand,
            _ => continue,
        };
        for (file, range) in primary_locations(diagnostic, root) {
            files.entry(file).or_default().insert(range, call);
        }
    }
    files
}

pub(super) fn plan(file: &Path, source: &str, calls: &Calls) -> Vec<Vec<Edit>> {
    let Ok(tree) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut names = TypeNames::default();
    if calls
        .values()
        .any(|call| matches!(call, Call::Now | Call::Rand | Call::RandRange))
    {
        names.visit_file(&tree);
    }
    let mut planner = Planner {
        file,
        text: Text::new(source, &tree),
        calls,
        names,
        deadlines: BTreeMap::new(),
        edits: Vec::new(),
    };
    planner.visit_file(&tree);
    planner.edits
}

struct Planner<'a> {
    file: &'a Path,
    text: Text<'a>,
    calls: &'a Calls,
    names: TypeNames,
    deadlines: BTreeMap<usize, Edit>,
    edits: Vec<Vec<Edit>>,
}

impl Planner<'_> {
    fn resolved(&self, callee: &Expr) -> Option<Call> {
        let Expr::Path(path) = callee else {
            return None;
        };
        if path.qself.is_some() || !path.attrs.is_empty() {
            return None;
        }
        let range = self.text.of(path)?;
        self.calls.get(&(range.start, range.end)).copied()
    }

    fn edit(&self, node: &impl syn::spanned::Spanned, replacement: &str) -> Option<Edit> {
        let range = self.text.of(node)?;
        Some(Edit {
            file: self.file.to_path_buf(),
            start: range.start,
            end: range.end,
            replacement: replacement.to_owned(),
        })
    }

    fn deadline(&self, expression: &Expr) -> Option<Edit> {
        let base = match unparen(expression) {
            Expr::Binary(sum)
                if matches!(sum.op, BinOp::Add(_)) && self.names.duration(&sum.right) =>
            {
                unparen(&sum.left)
            }
            base => base,
        };
        let Expr::Call(now) = base else {
            return None;
        };
        (self.resolved(&now.func) == Some(Call::Now) && now.args.is_empty())
            .then(|| self.edit(&now.func, "::adamantium::time::now"))?
    }

    fn until<'e>(&self, awaited: &'e ExprAwait) -> Option<&'e ExprCall> {
        let Expr::Call(call) = &*awaited.base else {
            return None;
        };
        (call.args.len() == 1
            && (self.resolved(&call.func) == Some(Call::SleepUntil)
                || self.names.unshadowed("adamantium") && adamantium_wait(&call.func)))
        .then_some(call)
    }

    fn contains_clock_read(&self, arg: &Expr) -> bool {
        self.text.of(arg).is_some_and(|range| {
            self.calls
                .range((range.start, 0)..(range.end, 0))
                .any(|(&(_, end), &kind)| end <= range.end && kind == Call::Now)
        })
    }

    fn local_deadline(&self, pair: &[Stmt], text: &str) -> Option<(usize, Edit)> {
        let [Stmt::Local(local), Stmt::Expr(Expr::Await(awaited), _)] = pair else {
            return None;
        };
        if !local.attrs.is_empty() {
            return None;
        }
        let pat = match &local.pat {
            Pat::Type(typed) => &*typed.pat,
            pat => pat,
        };
        let Pat::Ident(binding) = pat else {
            return None;
        };
        let init = local.init.as_ref()?;
        let argument = unparen(self.until(awaited)?.args.first()?);
        let Expr::Path(argument) = argument else {
            return None;
        };
        let name = bare(&binding.ident);
        if binding.by_ref.is_some()
            || binding.mutability.is_some()
            || binding.subpat.is_some()
            || init.diverge.is_some()
            || !argument.attrs.is_empty()
            || argument.qself.is_some()
            || argument
                .path
                .get_ident()
                .is_none_or(|ident| bare(ident) != name)
            || names_in(text, &name) != Some(2)
        {
            return None;
        }
        Some((self.text.of(awaited)?.start, self.deadline(&init.expr)?))
    }

    fn random(&self, call: &ExprCall, kind: Call) -> Option<Vec<Edit>> {
        let Expr::Path(callee) = &*call.func else {
            return None;
        };
        let generics = &callee.path.segments.last()?.arguments;
        let typed = match generics {
            PathArguments::AngleBracketed(args) => {
                matches!(args.args.first(), Some(GenericArgument::Type(ty)) if self.names.primitive_u64(ty))
            }
            _ => false,
        };
        if kind == Call::Rand {
            return (call.args.is_empty() && typed).then(|| {
                self.edit(callee, "::adamantium::random::u64")
                    .map(|edit| vec![edit])
            })?;
        }
        if call.args.len() != 1 {
            return None;
        }
        let (argument, comma) = match call.args.pairs().next()? {
            Pair::Punctuated(argument, comma) => (argument, Some(comma)),
            Pair::End(argument) => (argument, None),
        };
        let Expr::Range(range) = argument else {
            return None;
        };
        if !range.attrs.is_empty() {
            return None;
        }
        if kind == Call::Fastrand && range.start.is_none() && range.end.is_none() {
            let mut edits = vec![
                self.edit(callee, "::adamantium::random::u64")?,
                self.edit(&range.limits, "")?,
            ];
            if let Some(comma) = comma {
                edits.push(self.edit(comma, "")?);
            }
            return Some(coalesce(edits));
        }
        let start = range.start.as_deref()?;
        let end = range.end.as_deref()?;
        let Expr::Lit(start_lit) = start else {
            return None;
        };
        let Lit::Int(zero) = &start_lit.lit else {
            return None;
        };
        if !start_lit.attrs.is_empty()
            || zero.base10_parse::<u64>().ok()? != 0
            || !matches!(range.limits, syn::RangeLimits::Closed(_))
            || !(kind == Call::Fastrand
                || typed
                || zero.suffix() == "u64"
                || matches!(end, Expr::Lit(lit) if matches!(&lit.lit, Lit::Int(int) if int.suffix() == "u64")))
        {
            return None;
        }
        Some(coalesce(vec![
            self.edit(callee, "::adamantium::random::up_to")?,
            self.edit(start, "")?,
            self.edit(&range.limits, "")?,
        ]))
    }
}

impl<'ast> Visit<'ast> for Planner<'_> {
    fn visit_block(&mut self, block: &'ast syn::Block) {
        if let Some(text) = self.text.of(block).and_then(|range| self.text.get(range)) {
            for pair in block.stmts.windows(2) {
                if let Some((wait, now)) = self.local_deadline(pair, text) {
                    self.deadlines.insert(wait, now);
                }
            }
        }
        syn::visit::visit_block(self, block);
    }

    fn visit_expr_await(&mut self, awaited: &'ast ExprAwait) {
        if let Expr::Call(call) = &*awaited.base {
            let deadline = self.until(awaited).and_then(|call| {
                self.deadline(call.args.first()?)
                    .or_else(|| self.deadlines.remove(&self.text.of(awaited)?.start))
            });
            let rewrite = match self.resolved(&call.func) {
                Some(Call::Sleep) => self.edit(&call.func, "::adamantium::time::sleep"),
                Some(Call::SleepUntil)
                    if deadline.is_some()
                        || !call.args.iter().any(|arg| self.contains_clock_read(arg)) =>
                {
                    self.edit(&call.func, "::adamantium::time::sleep_until")
                }
                _ => None,
            };
            if rewrite.is_some() || deadline.is_some() {
                self.edits
                    .push(rewrite.into_iter().chain(deadline).collect());
            }
        }
        syn::visit::visit_expr_await(self, awaited);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if node.method == "unwrap"
            && node.args.is_empty()
            && let Expr::Await(awaited) = &*node.receiver
            && let Expr::Call(call) = &*awaited.base
            && let Expr::Path(path) = &*call.func
            && self.resolved(&call.func) == Some(Call::Blocking)
            && path
                .path
                .segments
                .iter()
                .all(|segment| matches!(segment.arguments, PathArguments::None))
            && let Some(args) = self.text.range(call.paren_token.span.join())
            && let Some(text) = self.text.get(args)
            && let Some(edit) = self.edit(node, &format!("::adamantium::blocking{text}.await"))
        {
            self.edits.push(vec![edit]);
            return;
        }
        syn::visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Some(kind @ (Call::Rand | Call::RandRange | Call::Fastrand)) =
            self.resolved(&call.func)
            && let Some(edits) = self.random(call, kind)
        {
            self.edits.push(edits);
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn unparen(mut expression: &Expr) -> &Expr {
    while let Expr::Paren(paren) = expression {
        if !paren.attrs.is_empty() {
            break;
        }
        expression = &paren.expr;
    }
    expression
}

fn adamantium_wait(callee: &Expr) -> bool {
    matches!(callee, Expr::Path(path) if path.qself.is_none()
        && path.attrs.is_empty()
        && plain_path(&path.path).is_some_and(|parts| parts == ["adamantium", "time", "sleep_until"]))
}

#[derive(Default)]
struct TypeNames {
    bound: BTreeSet<String>,
    opaque: bool,
    duration_import: bool,
}

impl TypeNames {
    fn unshadowed(&self, name: &str) -> bool {
        !self.opaque && !self.bound.contains(name)
    }

    fn primitive_u64(&self, ty: &Type) -> bool {
        let Type::Path(ty) = ty else {
            return false;
        };
        ty.qself.is_none()
            && (ty.path.is_ident("u64") && self.unshadowed("u64")
                || plain_path(&ty.path).is_some_and(|segments| {
                    matches!(segments.as_slice(), [krate, module, name]
                        if matches!(krate.as_str(), "core" | "std") && module == "primitive" && name == "u64"
                            && self.unshadowed(krate))
                }))
    }

    fn duration(&self, expr: &Expr) -> bool {
        let (path, called) = match unparen(expr) {
            Expr::Call(call) => match &*call.func {
                Expr::Path(path) => (path, true),
                _ => return false,
            },
            Expr::Path(path) => (path, false),
            _ => return false,
        };
        let Some(parts) = plain_path(&path.path) else {
            return false;
        };
        let Some((member, owner)) = parts.split_last() else {
            return false;
        };
        let standard_member = if called {
            matches!(
                member.as_str(),
                "new"
                    | "from_secs"
                    | "from_millis"
                    | "from_micros"
                    | "from_nanos"
                    | "from_secs_f32"
                    | "from_secs_f64"
            )
        } else {
            matches!(member.as_str(), "ZERO" | "MAX")
        };
        if path.qself.is_some() || !path.attrs.is_empty() || !standard_member {
            return false;
        }
        let absolute = path.path.leading_colon.is_some();
        match owner {
            [name] => {
                !absolute
                    && name == "Duration"
                    && self.duration_import
                    && self.unshadowed("Duration")
                    && self.unshadowed("std")
                    && self.unshadowed("core")
            }
            [krate, module, name] => {
                matches!(krate.as_str(), "std" | "core")
                    && module == "time"
                    && name == "Duration"
                    && self.unshadowed(krate)
            }
            _ => false,
        }
    }

    fn imports(&mut self, tree: &UseTree, prefix: &mut Vec<String>) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(bare(&path.ident));
                self.imports(&path.tree, prefix);
                prefix.pop();
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.imports(tree, prefix);
                }
            }
            UseTree::Name(name)
                if name.ident == "Duration"
                    && matches!(prefix.as_slice(),
                [krate, module] if matches!(krate.as_str(), "core" | "std") && module == "time") =>
            {
                self.duration_import = true;
            }
            UseTree::Name(name) if name.ident == "self" => {
                self.bound.extend(prefix.last().cloned());
            }
            UseTree::Name(name) => {
                self.bound.insert(bare(&name.ident));
            }
            UseTree::Rename(rename) => {
                self.bound.insert(bare(&rename.rename));
            }
            UseTree::Glob(_) => {
                self.opaque = true;
            }
        }
    }
}

fn plain_path(path: &syn::Path) -> Option<Vec<String>> {
    path.segments
        .iter()
        .map(|segment| {
            matches!(segment.arguments, PathArguments::None).then(|| bare(&segment.ident))
        })
        .collect()
}

impl<'ast> Visit<'ast> for TypeNames {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let name = match item {
            syn::Item::Enum(item) => Some(&item.ident),
            syn::Item::ExternCrate(item) => {
                Some(item.rename.as_ref().map_or(&item.ident, |(_, name)| name))
            }
            syn::Item::Mod(item) => Some(&item.ident),
            syn::Item::Struct(item) => Some(&item.ident),
            syn::Item::Trait(item) => Some(&item.ident),
            syn::Item::TraitAlias(item) => Some(&item.ident),
            syn::Item::Type(item) => Some(&item.ident),
            syn::Item::Union(item) => Some(&item.ident),
            syn::Item::Macro(item) if item.ident.is_none() => {
                self.opaque = true;
                None
            }
            _ => None,
        };
        self.bound.extend(name.map(bare));
        syn::visit::visit_item(self, item);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.imports(&item.tree, &mut Vec::new());
    }

    fn visit_type_param(&mut self, param: &'ast syn::TypeParam) {
        self.bound.insert(bare(&param.ident));
        syn::visit::visit_type_param(self, param);
    }
}
