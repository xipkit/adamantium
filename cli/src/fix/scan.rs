//! What a field rewrite needs to know about the whole package: which files
//! name the field, which functions mention which names, and which code might
//! run later or inside an actor's callback.
//!
//! The scan reads syntax only, so every answer errs toward refusing: a name
//! counts wherever it appears, whatever it refers to.

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::Result;
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::{
    Attribute, Expr, Item, Macro, Member, Visibility, ext::IdentExt, punctuated::Punctuated,
    visit::Visit,
};

mod poll;

/// A function, method or associated function defined in the package.
struct Function {
    asynchronous: bool,
    wrapped: bool,
    /// Declared in a trait or a trait implementation, so it may run through
    /// an operator, a `Drop`, a format or dynamic dispatch, not only by name.
    trait_method: bool,
    /// Declared `pub`, so another crate might call it.
    public: bool,
    /// Every name its body mentions.
    mentions: BTreeSet<String>,
}

/// One package's source, as a field rewrite needs it.
#[derive(Default)]
pub(super) struct Crate {
    poll: poll::Poll,
    /// Opaque code could manufacture a caller without spelling its name.
    opaque_poll: bool,
    /// The files that name each struct field, in an access, a literal, a
    /// pattern, a declaration or a macro.
    members: HashMap<String, BTreeSet<PathBuf>>,
    /// How many named fields of structs, unions and variants have each name.
    fields: HashMap<String, usize>,
    /// Why fix can't see all of the package's source, if it can't.
    unreadable: Option<&'static str>,
    functions: HashMap<String, Vec<Function>>,
    /// Names mentioned in code that may run later or elsewhere: closures,
    /// async blocks, macros fix can't read and items under attribute macros.
    deferred: BTreeSet<String>,
    /// Names used as values rather than called: the last segment of a
    /// longer path, as `bump` in `Self::bump`, and names in attribute
    /// strings, as serde's `default = "fresh"` names `fresh`.
    values: BTreeSet<String>,
    /// Single names used as values. A method can't be named this way, only
    /// a function or a variable.
    bare_values: BTreeSet<String>,
    /// The names each name is imported under with `use ... as`, so a call
    /// under the new name still reaches the original.
    renames: HashMap<String, BTreeSet<String>>,
    /// Whether an actor callback already in the package calls anything.
    callbacks: bool,
    /// Whether the package has a library, whose public functions another
    /// crate can call.
    library: bool,
    /// How many times each name appears in the package's source, in code,
    /// in macros and in string literals other than documentation.
    words: HashMap<String, usize>,
}

impl Crate {
    /// Scans every Rust file under `package`, except build output.
    pub(super) fn scan(package: &Path) -> Result<Self> {
        let manifest: toml_edit::DocumentMut =
            fs::read_to_string(package.join("Cargo.toml"))?.parse()?;
        let mut krate = Self {
            poll: poll::Poll::new(package),
            library: manifest.contains_key("lib") || package.join("src/lib.rs").is_file(),
            ..Self::default()
        };
        krate.walk(package, &mut BTreeSet::new());
        Ok(krate)
    }

    /// Adds the Rust files under `dir`. A directory or file fix can't read
    /// leaves every field for review rather than failing the whole fix.
    fn walk(&mut self, dir: &Path, seen: &mut BTreeSet<PathBuf>) {
        // Symbolic links can form a cycle.
        let Ok(canonical) = dir.canonicalize() else {
            self.unreadable(UNREAD);
            return;
        };
        if !seen.insert(canonical) || dir.join("CACHEDIR.TAG").is_file() {
            return;
        }
        let Ok(entries) = fs::read_dir(dir).and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()
        }) else {
            self.unreadable(UNREAD);
            return;
        };
        for path in entries {
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != ".git") {
                    self.walk(&path, seen);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = path.canonicalize().and_then(|file| {
                    let source = fs::read_to_string(&file)?;
                    Ok((file, source))
                });
                match source {
                    Ok((file, source)) => self.add(&file, &source),
                    Err(_) => self.unreadable(UNREAD),
                }
            }
        }
    }

    /// Adds one file's source.
    pub(super) fn add(&mut self, file: &Path, source: &str) {
        let Ok(tree) = syn::parse_file(source) else {
            self.unreadable(UNPARSED);
            return;
        };
        if includes(tree.to_token_stream()) {
            self.unreadable("its package reads source through `include!` or `#[path]`");
        }
        words(tree.to_token_stream(), &mut self.words);
        let mut macros = BTreeSet::new();
        for item in &tree.items {
            collect_macro_names(item, &mut macros);
        }
        self.opaque_poll |= self.poll.add(file, &tree, &macros);
        let mut scan = Scan {
            krate: self,
            file,
            macros: &macros,
            trait_depth: 0,
            locals: Vec::new(),
        };
        scan.visit_file(&tree);
    }

    fn unreadable(&mut self, reason: &'static str) {
        self.unreadable.get_or_insert(reason);
    }

    /// Why fix can't see all of the package's source, if it can't.
    pub(super) fn unreadable_reason(&self) -> Option<&'static str> {
        self.unreadable
    }

    /// Whether a file other than `file` names the field `name`.
    pub(super) fn names_member_elsewhere(&self, name: &str, file: &Path) -> bool {
        self.members
            .get(name)
            .is_some_and(|files| files.iter().any(|other| other != file))
    }

    /// How many named fields in the package are called `name`.
    pub(super) fn field_declarations(&self, name: &str) -> usize {
        self.fields.get(name).copied().unwrap_or(0)
    }

    /// How many times `name` appears anywhere in the package's source.
    pub(super) fn occurrences(&self, name: &str) -> usize {
        self.words.get(name).copied().unwrap_or(0)
    }

    /// `functions` and every function that mentions one of them, directly or
    /// through another: every function that may run one of them. A name an
    /// import gives one of them counts as reached too, so it can be called,
    /// or used as a value, under that name.
    pub(super) fn callers(&self, functions: &BTreeSet<String>) -> BTreeSet<String> {
        let mut reached = functions.clone();
        loop {
            let aliases = reached
                .iter()
                .filter_map(|name| self.renames.get(name))
                .flatten()
                .filter(|alias| !reached.contains(*alias))
                .cloned();
            let more: Vec<String> = self
                .functions
                .iter()
                .filter(|(name, definitions)| {
                    !reached.contains(*name)
                        && definitions
                            .iter()
                            .any(|function| !function.mentions.is_disjoint(&reached))
                })
                .map(|(name, _)| name.clone())
                .chain(aliases)
                .collect();
            if more.is_empty() {
                return reached;
            }
            reached.extend(more);
        }
    }

    /// Whether this exact function is the audited binary entry candidate.
    pub(super) fn poll_root(&self, file: &Path, sig: &syn::Signature) -> bool {
        self.poll.root(file, sig)
    }

    /// Proves every async caller, plus functions containing awaited blocks.
    pub(super) fn poll_hazard(
        &self,
        functions: &BTreeSet<String>,
        blocks: &BTreeSet<String>,
    ) -> Option<String> {
        for name in functions.union(blocks) {
            if (blocks.contains(name)
                || self
                    .functions
                    .get(name)
                    .is_some_and(|defs| defs.iter().any(|f| f.asynchronous)))
                && let Err(reason) = self.poll.prove(self, name)
            {
                return Some(reason);
            }
        }
        None
    }

    /// Why one of `functions` might run inside an actor's callback or out of
    /// fix's sight, if one might.
    pub(super) fn hazard(&self, functions: &BTreeSet<String>) -> Option<Hazard> {
        let definitions = || {
            functions
                .iter()
                .filter_map(|name| self.functions.get(name))
                .flatten()
        };
        if definitions().any(|function| function.trait_method) {
            return Some(Hazard::Trait);
        }
        if self.library && definitions().any(|function| function.public) {
            return Some(Hazard::Public);
        }
        // A callback can only reach a field through a function that locks it.
        if self.callbacks && !functions.is_empty() {
            return Some(Hazard::Callbacks);
        }
        if !functions.is_disjoint(&self.deferred)
            || !functions.is_disjoint(&self.values)
            || !functions.is_disjoint(&self.bare_values)
        {
            return Some(Hazard::Escapes);
        }
        None
    }
}

/// Why code that locks a field might run where fix can't follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hazard {
    Trait,
    Public,
    /// A closure, async block, macro or function value might run it later.
    Escapes,
    /// An actor callback already in the package calls something.
    Callbacks,
}

const UNPARSED: &str = "a Rust file in its package doesn't parse";
const UNREAD: &str = "fix couldn't read every Rust file in its package";

/// Attributes that are not macros and don't generate code from their item.
const INERT: &[&str] = &[
    "allow",
    "cfg",
    "cold",
    "deny",
    "deprecated",
    "doc",
    "expect",
    "forbid",
    "ignore",
    "inline",
    "must_use",
    "should_panic",
    "test",
    "track_caller",
    "warn",
];

/// Whether `attrs` hold an attribute that might be a macro reading its
/// item: anything but the compiler's inert attributes and a `derive` of
/// `Clone` and `Default`, which copy an actor as they copy an `Arc`.
pub(super) fn macro_attribute(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("derive") {
            let Ok(derived) =
                attr.parse_args_with(Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
            else {
                return true;
            };
            return derived.iter().any(|path| {
                !path
                    .segments
                    .last()
                    .is_some_and(|last| last.ident == "Clone" || last.ident == "Default")
            });
        }
        !attr
            .path()
            .get_ident()
            .is_some_and(|ident| INERT.contains(&ident.to_string().as_str()))
    })
}

/// The standard macros whose arguments parse as expressions.
fn reads_as_expressions(name: &str) -> bool {
    super::state::known_macro(name).is_some() || name == "vec"
}

/// Expression arguments of an unshadowed standard macro, if readable.
fn expression_arguments(
    mac: &Macro,
    shadowed: &BTreeSet<String>,
) -> Option<Punctuated<Expr, syn::Token![,]>> {
    let name = mac.path.get_ident()?.to_string();
    if mac.path.leading_colon.is_some() || shadowed.contains(&name) || !reads_as_expressions(&name)
    {
        return None;
    }
    mac.parse_body_with(Punctuated::<Expr, syn::Token![,]>::parse_terminated)
        .ok()
}

/// Every name in `tokens`, without `r#`.
pub(super) fn idents(tokens: TokenStream, names: &mut BTreeSet<String>) {
    for token in tokens {
        match token {
            TokenTree::Group(group) => idents(group.stream(), names),
            TokenTree::Ident(ident) => {
                names.insert(ident.unraw().to_string());
            }
            TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
}

/// Counts every name in `tokens`, without `r#`, and every word in their
/// string literals, which an attribute such as serde's `default = "fresh"`
/// or a format string may use as a name. Documentation is prose, not code.
fn words(tokens: TokenStream, counts: &mut HashMap<String, usize>) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    let punct = |index: Option<usize>, c: char| matches!(index.and_then(|index| tokens.get(index)), Some(TokenTree::Punct(punct)) if punct.as_char() == c);
    for (index, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) => {
                // `#[doc = "..."]` or `#![doc = "..."]`.
                let attribute = punct(index.checked_sub(1), '#')
                    || (punct(index.checked_sub(1), '!') && punct(index.checked_sub(2), '#'));
                let documentation = attribute
                    && group.delimiter() == Delimiter::Bracket
                    && matches!(group.stream().into_iter().next(), Some(TokenTree::Ident(name)) if name == "doc");
                if !documentation {
                    words(group.stream(), counts);
                }
            }
            TokenTree::Ident(ident) => {
                *counts.entry(ident.unraw().to_string()).or_default() += 1;
            }
            TokenTree::Literal(literal) => {
                if let Ok(syn::Lit::Str(string)) =
                    syn::parse2::<syn::Lit>(TokenTree::Literal(literal.clone()).into())
                {
                    for word in string
                        .value()
                        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .filter(|word| !word.is_empty())
                    {
                        *counts.entry(word.to_owned()).or_default() += 1;
                    }
                }
            }
            TokenTree::Punct(_) => {}
        }
    }
}

/// Whether `tokens` invoke `include!` or carry a `#[path]` attribute, which
/// read source fix doesn't scan.
fn includes(tokens: TokenStream) -> bool {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    tokens.iter().zip(tokens.iter().skip(1)).any(|pair| match pair {
        (TokenTree::Ident(ident), TokenTree::Punct(bang)) => {
            ident == "include" && bang.as_char() == '!'
        }
        (TokenTree::Punct(hash), TokenTree::Group(group)) => {
            hash.as_char() == '#'
                && group.delimiter() == Delimiter::Bracket
                && matches!(group.stream().into_iter().next(), Some(TokenTree::Ident(name)) if name == "path")
        }
        _ => false,
    }) || tokens.iter().any(|token| match token {
        TokenTree::Group(group) => includes(group.stream()),
        _ => false,
    })
}

/// Whether `tokens` could call `with` or `read` with an argument: an actor
/// callback in code fix can't read.
fn calls_back(tokens: TokenStream) -> bool {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    tokens
        .iter()
        .zip(tokens.iter().skip(1))
        .any(|pair| match pair {
            (TokenTree::Ident(ident), TokenTree::Group(group)) => {
                (ident == "with" || ident == "read")
                    && group.delimiter() == Delimiter::Parenthesis
                    && !group.stream().is_empty()
            }
            _ => false,
        })
        || tokens.iter().any(|token| match token {
            TokenTree::Group(group) => calls_back(group.stream()),
            _ => false,
        })
}

fn collect_macro_names(item: &Item, names: &mut BTreeSet<String>) {
    match item {
        Item::Macro(mac) => {
            if let Some(ident) = &mac.ident {
                names.insert(ident.unraw().to_string());
            }
        }
        // An import can bring in a macro of a standard macro's name.
        Item::Use(_) => {
            let mut tokens = BTreeSet::new();
            idents(item.to_token_stream(), &mut tokens);
            names.extend(tokens);
        }
        Item::Mod(module) => {
            for item in module.content.iter().flat_map(|(_, items)| items) {
                collect_macro_names(item, names);
            }
        }
        _ => {}
    }
}

/// Whether a callback passed to `with` or `read` might call something. A
/// closure that only reads fields and applies operators can't, unless an
/// operator is a trait implementation, which `Hazard::Trait` covers.
fn callback_calls(arg: &Expr) -> bool {
    struct Calls(bool);
    impl<'ast> Visit<'ast> for Calls {
        fn visit_expr(&mut self, expr: &'ast Expr) {
            self.0 |= matches!(expr, Expr::Call(_) | Expr::MethodCall(_) | Expr::Macro(_));
            syn::visit::visit_expr(self, expr);
        }
        fn visit_macro(&mut self, _: &'ast Macro) {
            self.0 = true;
        }
    }
    match arg {
        Expr::Lit(_) | Expr::Reference(_) => false,
        Expr::Closure(closure) => {
            let mut calls = Calls(false);
            calls.visit_expr(&closure.body);
            calls.0
        }
        _ => true,
    }
}

struct Scan<'c> {
    krate: &'c mut Crate,
    file: &'c Path,
    /// Macros the file defines or imports, which may shadow standard ones.
    macros: &'c BTreeSet<String>,
    /// Inside a trait or a trait implementation.
    trait_depth: usize,
    /// The local variables in scope, innermost last. A scope that starts an
    /// item hides those of the scopes around it.
    locals: Vec<(BTreeSet<String>, bool)>,
}

impl Scan<'_> {
    /// Runs `f` in a scope that declares `pat`'s names, if any.
    fn scoped<'p>(
        &mut self,
        pats: impl IntoIterator<Item = &'p syn::Pat>,
        item: bool,
        f: impl FnOnce(&mut Self),
    ) {
        let mut names = Vec::new();
        for pat in pats {
            super::state::pattern_names(pat, &mut names);
        }
        self.locals.push((names.into_iter().collect(), item));
        f(self);
        self.locals.pop();
    }

    /// Declares `pat`'s names in the innermost scope.
    fn bind(&mut self, pat: &syn::Pat) {
        let mut names = Vec::new();
        super::state::pattern_names(pat, &mut names);
        if let Some((scope, _)) = self.locals.last_mut() {
            scope.extend(names);
        }
    }

    fn is_local(&self, name: &str) -> bool {
        for (scope, item) in self.locals.iter().rev() {
            if scope.contains(name) {
                return true;
            }
            if *item {
                break;
            }
        }
        false
    }

    /// A function body, with its parameters in scope.
    fn body(&mut self, sig: &'_ syn::Signature, block: &'_ syn::Block) {
        let params: Vec<&syn::Pat> = sig
            .inputs
            .iter()
            .filter_map(|input| match input {
                syn::FnArg::Typed(typed) => Some(&*typed.pat),
                syn::FnArg::Receiver(_) => None,
            })
            .collect();
        self.scoped(params, true, |scan| scan.visit_block(block));
    }

    fn member(&mut self, name: String) {
        self.krate
            .members
            .entry(name)
            .or_default()
            .insert(self.file.to_path_buf());
    }

    /// Code that may run later or elsewhere, and whose fields fix can't see.
    fn defer(&mut self, tokens: TokenStream) {
        let mut names = BTreeSet::new();
        idents(tokens, &mut names);
        for name in &names {
            self.member(name.clone());
        }
        self.krate.deferred.extend(names);
    }

    fn function(
        &mut self,
        name: &syn::Ident,
        sig: &syn::Signature,
        vis: Option<&Visibility>,
        body: TokenStream,
        attrs: &[Attribute],
    ) {
        let mut mentions = BTreeSet::new();
        idents(body.clone(), &mut mentions);
        if macro_attribute(attrs) && !self.krate.poll_root(self.file, sig) {
            self.krate.opaque_poll = true;
            self.defer(body);
            self.krate.deferred.insert(name.unraw().to_string());
        }
        let wrapped = macro_attribute(attrs) && !self.krate.poll_root(self.file, sig);
        self.krate
            .functions
            .entry(name.unraw().to_string())
            .or_default()
            .push(Function {
                asynchronous: sig.asyncness.is_some(),
                wrapped,
                trait_method: self.trait_depth > 0,
                public: matches!(vis, Some(Visibility::Public(_))),
                mentions,
            });
    }

    /// Records a call to `with` or `read` that passes a callback.
    fn callback<'e>(&mut self, name: &str, args: impl IntoIterator<Item = &'e Expr>) {
        if (name == "with" || name == "read") && args.into_iter().any(callback_calls) {
            self.krate.callbacks = true;
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if macro_attribute(super::state::item_attributes(item))
            && !matches!(item, Item::Fn(f) if self.krate.poll_root(self.file, &f.sig))
        {
            self.krate.opaque_poll = true;
            self.defer(item.to_token_stream());
        }
        self.scoped([], true, |scan| syn::visit::visit_item(scan, item));
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.function(
            &item.sig.ident,
            &item.sig,
            Some(&item.vis),
            item.block.to_token_stream(),
            &item.attrs,
        );
        if !self.krate.poll_root(self.file, &item.sig) {
            for attr in &item.attrs {
                self.visit_attribute(attr);
            }
        }
        self.body(&item.sig, &item.block);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let in_trait = usize::from(item.trait_.is_some());
        self.trait_depth += in_trait;
        syn::visit::visit_item_impl(self, item);
        self.trait_depth -= in_trait;
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.trait_depth += 1;
        syn::visit::visit_item_trait(self, item);
        self.trait_depth -= 1;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.function(
            &item.sig.ident,
            &item.sig,
            Some(&item.vis),
            item.block.to_token_stream(),
            &item.attrs,
        );
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
        self.body(&item.sig, &item.block);
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if let Some(body) = &item.default {
            self.function(
                &item.sig.ident,
                &item.sig,
                None,
                body.to_token_stream(),
                &item.attrs,
            );
            for attr in &item.attrs {
                self.visit_attribute(attr);
            }
            self.body(&item.sig, body);
        }
    }

    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        self.defer(closure.body.to_token_stream());
        self.scoped(&closure.inputs, false, |scan| {
            scan.visit_expr(&closure.body)
        });
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.scoped([], false, |scan| syn::visit::visit_block(scan, block));
    }

    /// A `let` declares its names after its initializer.
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Some(init) = &local.init {
            self.visit_expr(&init.expr);
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
        }
        self.visit_pat(&local.pat);
        self.bind(&local.pat);
    }

    fn visit_expr_let(&mut self, binding: &'ast syn::ExprLet) {
        self.visit_expr(&binding.expr);
        self.visit_pat(&binding.pat);
        self.bind(&binding.pat);
    }

    /// The names an `if let` or `while let` binds are in scope in its body
    /// only.
    fn visit_expr_if(&mut self, branch: &'ast syn::ExprIf) {
        self.scoped([], false, |scan| {
            scan.visit_expr(&branch.cond);
            scan.visit_block(&branch.then_branch);
        });
        if let Some((_, other)) = &branch.else_branch {
            self.visit_expr(other);
        }
    }

    fn visit_expr_while(&mut self, lp: &'ast syn::ExprWhile) {
        self.scoped([], false, |scan| {
            scan.visit_expr(&lp.cond);
            scan.visit_block(&lp.body);
        });
    }

    fn visit_expr_for_loop(&mut self, lp: &'ast syn::ExprForLoop) {
        self.visit_expr(&lp.expr);
        self.scoped([&*lp.pat], false, |scan| scan.visit_block(&lp.body));
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        self.scoped([&arm.pat], false, |scan| {
            if let syn::Pat::Guard(guard) = &arm.pat {
                scan.visit_expr(&guard.guard);
            }
            scan.visit_expr(&arm.body);
        });
    }

    fn visit_expr_await(&mut self, expr: &'ast syn::ExprAwait) {
        if let Expr::Async(block) = &*expr.base
            && expr.attrs.is_empty()
            && block.attrs.is_empty()
        {
            self.visit_block(&block.block);
        } else {
            syn::visit::visit_expr_await(self, expr);
        }
    }

    fn visit_expr_async(&mut self, block: &'ast syn::ExprAsync) {
        self.defer(block.block.to_token_stream());
        syn::visit::visit_expr_async(self, block);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        match &*call.func {
            // A called path is not a value.
            Expr::Path(path) => {
                if let Some(last) = path.path.segments.last() {
                    self.callback(&last.ident.to_string(), call.args.iter().skip(1));
                }
                for segment in &path.path.segments {
                    self.visit_path_arguments(&segment.arguments);
                }
            }
            func => self.visit_expr(func),
        }
        for arg in &call.args {
            self.visit_expr(arg);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.callback(&call.method.to_string(), &call.args);
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if let Some(last) = path.path.segments.last() {
            let name = last.ident.unraw().to_string();
            if path.path.segments.len() > 1 || path.qself.is_some() {
                self.krate.values.insert(name);
            } else if !self.is_local(&name) {
                self.krate.bare_values.insert(name);
            }
        }
        syn::visit::visit_expr_path(self, path);
    }

    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        self.krate
            .renames
            .entry(rename.ident.unraw().to_string())
            .or_default()
            .insert(rename.rename.unraw().to_string());
    }

    fn visit_expr_field(&mut self, field: &'ast syn::ExprField) {
        if let Member::Named(name) = &field.member {
            self.member(name.unraw().to_string());
        }
        syn::visit::visit_expr_field(self, field);
    }

    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        if let Member::Named(name) = &field.member {
            self.member(name.unraw().to_string());
        }
        syn::visit::visit_field_value(self, field);
    }

    fn visit_field_pat(&mut self, field: &'ast syn::FieldPat) {
        if let Member::Named(name) = &field.member {
            self.member(name.unraw().to_string());
        }
        syn::visit::visit_field_pat(self, field);
    }

    fn visit_field(&mut self, field: &'ast syn::Field) {
        if let Some(name) = &field.ident {
            let name = name.unraw().to_string();
            *self.krate.fields.entry(name.clone()).or_default() += 1;
            self.member(name);
        }
        syn::visit::visit_field(self, field);
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if !attr.path().is_ident("doc") {
            strings(attr.to_token_stream(), &mut self.krate.values);
            strings(attr.to_token_stream(), &mut self.krate.bare_values);
        }
        syn::visit::visit_attribute(self, attr);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        if let Some(args) = expression_arguments(mac, self.macros) {
            for arg in &args {
                self.visit_expr(arg);
            }
            return;
        }
        self.krate.opaque_poll = true;
        if calls_back(mac.tokens.clone()) {
            self.krate.callbacks = true;
        }
        self.defer(mac.tokens.clone());
    }
}

/// The identifiers in the string literals of `tokens`, which an attribute
/// may resolve as paths.
fn strings(tokens: TokenStream, names: &mut BTreeSet<String>) {
    for token in tokens {
        match token {
            TokenTree::Group(group) => strings(group.stream(), names),
            TokenTree::Literal(literal) => {
                if let Ok(syn::Lit::Str(string)) =
                    syn::parse2::<syn::Lit>(TokenTree::Literal(literal).into())
                    && let Ok(tokens) = TokenStream::from_str(&string.value())
                {
                    idents(tokens, names);
                }
            }
            TokenTree::Ident(_) | TokenTree::Punct(_) => {}
        }
    }
}
