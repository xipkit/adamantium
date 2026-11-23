use std::{
    collections::{BTreeSet, HashSet},
    mem,
    path::PathBuf,
};

use proc_macro2::{Delimiter, Spacing, Span, TokenStream, TokenTree};
use syn::{
    Arm, Attribute, Block, Expr, ExprCall, ExprClosure, ExprForLoop, ExprIf, ExprMethodCall,
    ExprPath, ExprUnsafe, ExprWhile, Field, FieldValue, FnArg, GenericParam, Generics, ImplItem,
    ImplItemFn, Item, ItemFn, ItemForeignMod, ItemImpl, ItemMacro, ItemMod, ItemTrait, Macro, Meta,
    Pat, PatIdent, Safety, Signature, Stmt, Token, TraitItem, TraitItemFn, Type, Variant,
    ext::IdentExt,
    punctuated::Punctuated,
    visit::{self, Visit},
};

use super::{
    super::{cfg::Truth, disallowed::canonical, rules::is_punct},
    Deferred, DefinitionSite, Scanner, Site, Unscanned,
    attrs::{UNSAFE_ATTRIBUTES, expr_attrs, is_unsafe_attribute, item_attrs},
    flatten::{is_include, reads_input},
    macros::{Textual, call_site},
    modules::Location,
    resolve::Resolver,
    scope::{Namespace, Resolved, Scope},
};

const GLOBAL_ASM: [&str; 3] = ["std", "arch", "global_asm"];

pub(super) struct FileVisitor<'s, 'a> {
    scanner: &'s mut Scanner<'a>,
    file: String,
    module: Vec<String>,
    mod_dir: PathBuf,
    path_dir: PathBuf,
    blocks: Vec<Scope>,
    attributes: HashSet<Site>,
    hidden: HashSet<String>,
}

impl<'s, 'a> FileVisitor<'s, 'a> {
    pub(super) fn new(
        scanner: &'s mut Scanner<'a>,
        file: String,
        module: Vec<String>,
        mod_dir: PathBuf,
        path_dir: PathBuf,
        hidden: HashSet<String>,
    ) -> Self {
        Self {
            scanner,
            file,
            module,
            mod_dir,
            path_dir,
            blocks: Vec::new(),
            attributes: HashSet::new(),
            hidden,
        }
    }

    fn enabled(&self, attrs: &[Attribute]) -> bool {
        self.scanner.krate.cfg.enabled(attrs) != Truth::False
    }

    fn site(&self, span: Span) -> Site {
        let start = span.start();
        Site {
            file: self.scanner.file_of(span, &self.file),
            line: start.line,
            column: start.column + 1,
        }
    }

    fn here(&self, span: Span) -> bool {
        !self.scanner.foreign.contains(&span.file())
    }

    fn writes(&self, unsafety: Option<&Token![unsafe]>) -> bool {
        unsafety.is_some_and(|token| self.here(token.span))
    }

    fn writes_unsafe(&self, safety: &Safety) -> bool {
        match safety {
            Safety::Unsafe(token) => self.writes(Some(token)),
            _ => false,
        }
    }

    fn unscanned(&mut self, span: Span, reason: String) {
        let site = self.site(span);
        self.scanner.unscanned(Unscanned {
            file: site.file,
            line: Some(site.line),
            reason,
        });
    }

    fn unsafe_attributes(&self, meta: Meta) -> Vec<Meta> {
        self.scanner
            .krate
            .cfg
            .expanded(meta)
            .into_iter()
            .map(|(_, meta)| meta)
            .filter(is_unsafe_attribute)
            .collect()
    }

    fn unexpanded(&mut self, mac: &Macro) {
        if let Some(segment) = mac.path.segments.last() {
            self.scanner
                .unexpanded
                .insert(segment.ident.unraw().to_string());
        }
    }

    fn calls_global_asm(&self, path: &[String], leading: bool, hidden: &HashSet<String>) -> bool {
        let bare = !leading && path.len() == 1;
        if bare && path.first().is_some_and(|name| hidden.contains(name)) {
            return false;
        }
        let named = path.last().is_some_and(|name| name == "global_asm");
        let mut resolver = Resolver::new(self.scanner, &self.blocks);
        let resolved = resolver.path(&self.module, Namespace::Macros, path, leading);
        let globs = || {
            self.blocks
                .iter()
                .chain(resolver.module(&self.module))
                .any(|scope| scope.globs)
        };
        resolved
            .alternatives()
            .iter()
            .any(|resolved| match resolved {
                Resolved::Path { krate: None, path } => canonical(path.clone()) == GLOBAL_ASM,
                Resolved::Unknown => named && (!bare || globs()),
                Resolved::Path { .. } | Resolved::Local | Resolved::Either(_) => false,
            })
    }

    fn item_call(&mut self, mac: &Macro, hidden: &HashSet<String>) {
        if self.here(mac.bang_token.span)
            && self.calls_global_asm(
                &segments(&mac.path),
                mac.path.leading_colon.is_some(),
                hidden,
            )
        {
            self.scanner.scan.unsafe_code.global_asm += 1;
        }
    }

    fn unresolved(&mut self, name: &str) {
        *self
            .scanner
            .scan
            .unresolved
            .entry(name.to_owned())
            .or_default() += 1;
    }

    fn resolve(&self, expr: &ExprPath) -> Resolved {
        let mut resolver = Resolver::new(self.scanner, &self.blocks);
        let names = segments(&expr.path);
        let resolved = match &expr.qself {
            Some(qself) if qself.position == 0 => {
                let base = match &*qself.ty {
                    Type::Slice(_) => Resolved::Path {
                        krate: None,
                        path: vec!["slice".to_owned()],
                    },
                    Type::Path(ty) if ty.qself.is_none() => {
                        resolver.syn_path(&self.module, Namespace::Types, &ty.path)
                    }
                    _ => Resolved::Unknown,
                };
                base.join(&names)
            }
            _ => resolver.syn_path(&self.module, Namespace::Values, &expr.path),
        };
        resolved.map_paths(|krate, path| Resolved::Path {
            krate,
            path: canonical(path),
        })
    }

    fn start(expr: &ExprPath) -> Span {
        if let Some(qself) = &expr.qself {
            return qself.lt_token.span;
        }
        if let Some(colon) = &expr.path.leading_colon {
            let [first, _] = colon.spans;
            return first;
        }
        expr.path
            .segments
            .first()
            .map_or_else(Span::call_site, |segment| segment.ident.span())
    }

    pub(super) fn tokens(&mut self, stream: TokenStream) {
        let trees: Vec<TokenTree> = stream.into_iter().collect();
        for (index, tree) in trees.iter().enumerate() {
            let previous = index.checked_sub(1).and_then(|index| trees.get(index));
            let before = index.checked_sub(2).and_then(|index| trees.get(index));
            match tree {
                TokenTree::Ident(ident) if ident == "unsafe" && self.here(ident.span()) => {
                    self.scanner.scan.unsafe_code.in_macros += 1;
                }
                TokenTree::Punct(punct) if punct.as_char() == ';' => {
                    if let (Some(TokenTree::Ident(name)), Some(TokenTree::Ident(keyword))) =
                        (previous, before)
                        && keyword == "mod"
                    {
                        self.unscanned(
                            name.span(),
                            format!(
                                "module `{}` is declared in a macro body the scan can't read",
                                name.unraw()
                            ),
                        );
                    }
                }
                TokenTree::Group(group) => {
                    if let (Some(TokenTree::Punct(bang)), Some(TokenTree::Ident(name))) =
                        (previous, before)
                        && bang.as_char() == '!'
                        && name != "macro_rules"
                    {
                        self.scanner.unexpanded.insert(name.unraw().to_string());
                        if self.here(name.span())
                            && let Some((path, leading)) =
                                trees.get(..index.saturating_sub(1)).and_then(trailing_path)
                            && self.calls_global_asm(&path, leading, &self.hidden)
                        {
                            self.scanner.scan.unsafe_code.in_macros += 1;
                        }
                    }
                    if group.delimiter() == Delimiter::Bracket
                        && is_attribute(previous, before)
                        && self.here(group.span())
                    {
                        self.scanner.scan.unsafe_code.in_macros +=
                            self.bare_unsafe_attributes(group.stream());
                    }
                    if group.delimiter() == Delimiter::Parenthesis
                        && let Some(TokenTree::Ident(name)) = previous
                        && self.here(name.span())
                    {
                        let name = name.unraw().to_string();
                        let disallowed = self.scanner.disallowed;
                        let counts = match before {
                            Some(TokenTree::Ident(ident)) if ident == "fn" => false,
                            Some(TokenTree::Punct(punct)) if punct.as_char() == '.' => {
                                disallowed.has_method(&name)
                            }
                            _ => disallowed.has_name(&name),
                        };
                        if counts {
                            self.unresolved(&name);
                        }
                    }
                    self.tokens(group.stream());
                }
                _ => {}
            }
        }
    }

    fn bare_unsafe_attributes(&self, attribute: TokenStream) -> usize {
        let mut trees = attribute.into_iter();
        let Some(TokenTree::Ident(name)) = trees.next() else {
            return 0;
        };
        if UNSAFE_ATTRIBUTES
            .iter()
            .any(|unsafe_name| name == unsafe_name)
        {
            return 1;
        }
        if name != "cfg_attr" {
            return 0;
        }
        let (Some(TokenTree::Group(group)), None) = (trees.next(), trees.next()) else {
            return 0;
        };
        let mut parts = split_commas(group.stream()).into_iter();
        let Some(predicate) = parts.next() else {
            return 0;
        };
        if self.scanner.krate.cfg.holds(predicate) == Truth::False {
            return 0;
        }
        parts.map(|part| self.bare_unsafe_attributes(part)).sum()
    }

    fn module_item(&mut self, item: &ItemMod) {
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
        let mut module = self.module.clone();
        module.push(item.ident.unraw().to_string());
        let location = self.scanner.locate(item, &self.mod_dir, &self.path_dir);
        match (&item.content, location) {
            (Some((_, items)), location) => {
                let dir = match location {
                    Location::Inline { dir } => dir,
                    _ => self.mod_dir.join(item.ident.unraw().to_string()),
                };
                let outer = mem::replace(&mut self.module, module);
                let mod_dir = mem::replace(&mut self.mod_dir, dir.clone());
                let path_dir = mem::replace(&mut self.path_dir, dir);
                let blocks = mem::take(&mut self.blocks);
                for item in items {
                    self.visit_item(item);
                }
                self.module = outer;
                self.mod_dir = mod_dir;
                self.path_dir = path_dir;
                self.blocks = blocks;
            }
            (None, _) if self.blocks.is_empty() => {}
            (None, Location::Files(files)) => {
                let textual = &mut Textual::default();
                for file in files {
                    let declared = self.site(item.ident.span()).file;
                    let from = (declared.as_str(), item.ident.span());
                    self.scanner
                        .load(&file.path, module.clone(), file.mod_rs, Some(from), textual);
                }
            }
            (None, Location::Unscannable(reason)) => self.unscanned(item.ident.span(), reason),
            (None, Location::Inline { .. }) => {}
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        let items = stmts.iter().filter_map(|stmt| match stmt {
            Stmt::Item(item) => Some(item),
            _ => None,
        });
        let mut scope = Scope::default();
        self.scanner.bind(&mut scope, items, None, false);
        self.blocks.push(scope);
        for stmt in stmts {
            self.visit_stmt(stmt);
            if let Stmt::Local(local) = stmt
                && self.enabled(&local.attrs)
                && let Some(block) = self.blocks.last_mut()
            {
                for name in pattern_names([&local.pat]) {
                    block.shadow(Namespace::Values, name);
                }
            }
        }
        self.blocks.pop();
    }

    fn condition(&mut self, condition: &Expr) {
        match condition {
            Expr::Let(expr) if self.enabled(&expr.attrs) => {
                self.visit_expr(&expr.expr);
                self.visit_pat(&expr.pat);
                if let Some(block) = self.blocks.last_mut() {
                    for name in pattern_names([&*expr.pat]) {
                        block.shadow(Namespace::Values, name);
                    }
                }
            }
            Expr::Binary(expr) if self.enabled(&expr.attrs) => {
                self.condition(&expr.left);
                self.condition(&expr.right);
            }
            Expr::Paren(expr) if self.enabled(&expr.attrs) => self.condition(&expr.expr),
            expr => self.visit_expr(expr),
        }
    }

    fn with_generics(&mut self, generics: &Generics, visit: impl FnOnce(&mut Self)) {
        let mut scope = Scope::default();
        for param in &generics.params {
            match param {
                GenericParam::Type(param) => {
                    scope.shadow(Namespace::Types, param.ident.unraw().to_string());
                }
                GenericParam::Const(param) => {
                    scope.shadow(Namespace::Values, param.ident.unraw().to_string());
                }
                _ => {}
            }
        }
        self.blocks.push(scope);
        visit(self);
        self.blocks.pop();
    }

    fn with_patterns<'p>(
        &mut self,
        patterns: impl IntoIterator<Item = &'p Pat>,
        visit: impl FnOnce(&mut Self),
    ) {
        let mut scope = Scope::default();
        for name in pattern_names(patterns) {
            scope.shadow(Namespace::Values, name);
        }
        self.blocks.push(scope);
        visit(self);
        self.blocks.pop();
    }
}

pub(super) fn trailing_path(mut trees: &[TokenTree]) -> Option<(Vec<String>, bool)> {
    let mut path = Vec::new();
    let leading = loop {
        let [rest @ .., TokenTree::Ident(ident)] = trees else {
            if path.is_empty() {
                return None;
            }
            break true;
        };
        path.push(ident.unraw().to_string());
        match rest {
            [rest @ .., TokenTree::Punct(first), TokenTree::Punct(second)]
                if first.as_char() == ':'
                    && first.spacing() == Spacing::Joint
                    && second.as_char() == ':' =>
            {
                trees = rest;
            }
            _ => break false,
        }
    };
    path.reverse();
    Some((path, leading))
}

fn is_attribute(previous: Option<&TokenTree>, before: Option<&TokenTree>) -> bool {
    is_punct(previous, '#') || (is_punct(previous, '!') && is_punct(before, '#'))
}

fn split_commas(stream: TokenStream) -> Vec<TokenStream> {
    let mut parts = vec![TokenStream::new()];
    for tree in stream {
        if is_punct(Some(&tree), ',') {
            parts.push(TokenStream::new());
        } else if let Some(part) = parts.last_mut() {
            part.extend([tree]);
        }
    }
    parts.retain(|part| !part.is_empty());
    parts
}

fn pattern_names<'p>(patterns: impl IntoIterator<Item = &'p Pat>) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'ast> Visit<'ast> for Names {
        fn visit_pat_ident(&mut self, pat: &'ast PatIdent) {
            self.0.push(pat.ident.unraw().to_string());
            visit::visit_pat_ident(self, pat);
        }
    }
    let mut names = Names(Vec::new());
    for pattern in patterns {
        names.visit_pat(pattern);
    }
    names.0
}

fn parameters(sig: &Signature) -> impl Iterator<Item = &Pat> {
    sig.inputs.iter().filter_map(|input| match input {
        FnArg::Typed(typed) => Some(&*typed.pat),
        FnArg::Receiver(_) => None,
    })
}

pub(super) fn segments(path: &syn::Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.unraw().to_string())
        .collect()
}

impl<'ast> Visit<'ast> for FileVisitor<'_, '_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !self.enabled(item_attrs(item)) {
            return;
        }
        match item {
            Item::Mod(item) => self.module_item(item),
            Item::Macro(item) => self.visit_item_macro(item),
            _ => visit::visit_item(self, item),
        }
    }

    fn visit_item_macro(&mut self, item: &'ast ItemMacro) {
        if item.ident.is_none() && is_include(&item.mac) {
            self.unscanned(
                item.mac.bang_token.span,
                "include! of another file is not followed".to_owned(),
            );
            return;
        }
        let hidden = self
            .scanner
            .hidden
            .get(&call_site(&item.mac))
            .cloned()
            .unwrap_or_default();
        if item.ident.is_none() {
            self.item_call(&item.mac, &hidden);
            if reads_input(&item.mac) {
                return;
            }
        }
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
        match &item.ident {
            Some(name) => {
                let file = self.scanner.file_of(name.span(), &self.file);
                let at = DefinitionSite::new(file, name.span());
                if self.scanner.expanded.contains(&at) {
                    let deferred = Deferred {
                        name: name.unraw().to_string(),
                        module: self.module.clone(),
                        mod_dir: self.mod_dir.clone(),
                        path_dir: self.path_dir.clone(),
                        tokens: item.mac.tokens.clone(),
                        hidden,
                    };
                    self.scanner.deferred.insert(at, deferred);
                    return;
                }
            }
            None => self.unexpanded(&item.mac),
        }
        let outer = mem::replace(&mut self.hidden, hidden);
        self.tokens(item.mac.tokens.clone());
        self.hidden = outer;
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if self.writes_unsafe(&item.sig.safety) {
            self.scanner.scan.unsafe_code.functions += 1;
        }
        self.with_generics(&item.sig.generics, |this| {
            this.with_patterns(parameters(&item.sig), |this| {
                visit::visit_item_fn(this, item);
            });
        });
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if self.writes(item.unsafety.as_ref()) {
            self.scanner.scan.unsafe_code.impls += 1;
        }
        self.with_generics(&item.generics, |this| visit::visit_item_impl(this, item));
    }

    fn visit_item_trait(&mut self, item: &'ast ItemTrait) {
        if self.writes(item.unsafety.as_ref()) {
            self.scanner.scan.unsafe_code.traits += 1;
        }
        self.with_generics(&item.generics, |this| visit::visit_item_trait(this, item));
    }

    fn visit_item_foreign_mod(&mut self, item: &'ast ItemForeignMod) {
        if self.writes(item.unsafety.as_ref()) {
            self.scanner.scan.unsafe_code.extern_blocks += 1;
        }
        visit::visit_item_foreign_mod(self, item);
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        let attrs = match item {
            ImplItem::Const(item) => &item.attrs[..],
            ImplItem::Fn(item) => &item.attrs,
            ImplItem::Type(item) => &item.attrs,
            ImplItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if self.enabled(attrs) {
            visit::visit_impl_item(self, item);
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if self.writes_unsafe(&item.sig.safety) {
            self.scanner.scan.unsafe_code.functions += 1;
        }
        self.with_generics(&item.sig.generics, |this| {
            this.with_patterns(parameters(&item.sig), |this| {
                visit::visit_impl_item_fn(this, item);
            });
        });
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        let attrs = match item {
            TraitItem::Const(item) => &item.attrs[..],
            TraitItem::Fn(item) => &item.attrs,
            TraitItem::Type(item) => &item.attrs,
            TraitItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if self.enabled(attrs) {
            visit::visit_trait_item(self, item);
        }
    }

    fn visit_trait_item_fn(&mut self, item: &'ast TraitItemFn) {
        if self.writes_unsafe(&item.sig.safety) {
            self.scanner.scan.unsafe_code.functions += 1;
        }
        self.with_generics(&item.sig.generics, |this| {
            this.with_patterns(parameters(&item.sig), |this| {
                visit::visit_trait_item_fn(this, item);
            });
        });
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if self.here(attr.pound_token.span)
            && self.attributes.insert(self.site(attr.pound_token.span))
        {
            self.scanner.scan.unsafe_code.attributes +=
                self.unsafe_attributes(attr.meta.clone()).len();
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_block(&mut self, block: &'ast Block) {
        self.stmts(&block.stmts);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let attrs = match stmt {
            Stmt::Local(local) => &local.attrs[..],
            Stmt::Macro(mac) => &mac.attrs,
            Stmt::Expr(..) | Stmt::Item(_) => &[],
        };
        if self.enabled(attrs) {
            visit::visit_stmt(self, stmt);
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if self.enabled(expr_attrs(expr)) {
            visit::visit_expr(self, expr);
        }
    }

    fn visit_field_value(&mut self, field: &'ast FieldValue) {
        if self.enabled(&field.attrs) {
            visit::visit_field_value(self, field);
        }
    }

    fn visit_variant(&mut self, variant: &'ast Variant) {
        if self.enabled(&variant.attrs) {
            visit::visit_variant(self, variant);
        }
    }

    fn visit_field(&mut self, field: &'ast Field) {
        if self.enabled(&field.attrs) {
            visit::visit_field(self, field);
        }
    }

    fn visit_arm(&mut self, arm: &'ast Arm) {
        if self.enabled(&arm.attrs) {
            self.with_patterns([&arm.pat], |this| visit::visit_arm(this, arm));
        }
    }

    fn visit_expr_closure(&mut self, expr: &'ast ExprClosure) {
        self.with_patterns(&expr.inputs, |this| visit::visit_expr_closure(this, expr));
    }

    fn visit_expr_for_loop(&mut self, expr: &'ast ExprForLoop) {
        for attr in &expr.attrs {
            self.visit_attribute(attr);
        }
        self.visit_expr(&expr.expr);
        self.with_patterns([&*expr.pat], |this| this.visit_block(&expr.body));
    }

    fn visit_expr_if(&mut self, expr: &'ast ExprIf) {
        for attr in &expr.attrs {
            self.visit_attribute(attr);
        }
        self.blocks.push(Scope::default());
        self.condition(&expr.cond);
        self.visit_block(&expr.then_branch);
        self.blocks.pop();
        if let Some((_, otherwise)) = &expr.else_branch {
            self.visit_expr(otherwise);
        }
    }

    fn visit_expr_while(&mut self, expr: &'ast ExprWhile) {
        for attr in &expr.attrs {
            self.visit_attribute(attr);
        }
        self.blocks.push(Scope::default());
        self.condition(&expr.cond);
        self.visit_block(&expr.body);
        self.blocks.pop();
    }

    fn visit_expr_unsafe(&mut self, expr: &'ast ExprUnsafe) {
        if self.here(expr.unsafe_token.span) {
            self.scanner.scan.unsafe_code.blocks += 1;
        }
        visit::visit_expr_unsafe(self, expr);
    }

    fn visit_expr_path(&mut self, expr: &'ast ExprPath) {
        let entries: BTreeSet<usize> = self
            .resolve(expr)
            .alternatives()
            .iter()
            .filter_map(|resolved| match resolved {
                Resolved::Path { path, .. } => self.scanner.entry(path),
                _ => None,
            })
            .collect();
        let start = Self::start(expr);
        if self.here(start) {
            for entry in entries {
                let site = self.site(start);
                self.scanner.scan.calls.entry(entry).or_default().push(site);
            }
        }
        visit::visit_expr_path(self, expr);
    }

    fn visit_expr_call(&mut self, expr: &'ast ExprCall) {
        if let Expr::Path(func) = &*expr.func
            && let Some(name) = func
                .path
                .segments
                .last()
                .map(|s| s.ident.unraw().to_string())
            && self.scanner.disallowed.has_name(&name)
            && self.here(Self::start(func))
        {
            let disallowed = self.scanner.disallowed;
            let resolved = self.resolve(func);
            let unresolved = resolved
                .alternatives()
                .iter()
                .any(|resolved| match resolved {
                    Resolved::Path { path, .. } => {
                        self.scanner.entry(path).is_none()
                            && path
                                .first()
                                .is_none_or(|krate| !disallowed.names_crate(krate))
                    }
                    Resolved::Local => false,
                    Resolved::Unknown | Resolved::Either(_) => true,
                });
            if unresolved {
                self.unresolved(&name);
            }
        }
        visit::visit_expr_call(self, expr);
    }

    fn visit_expr_method_call(&mut self, expr: &'ast ExprMethodCall) {
        let name = expr.method.unraw().to_string();
        if self.scanner.disallowed.has_method(&name) && self.here(expr.method.span()) {
            self.unresolved(&name);
        }
        visit::visit_expr_method_call(self, expr);
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        if is_include(mac) {
            self.unscanned(
                mac.bang_token.span,
                "include! of another file is not followed".to_owned(),
            );
            return;
        }
        self.unexpanded(mac);
        if let Ok(exprs) = mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated) {
            for expr in &exprs {
                self.visit_expr(expr);
            }
        } else if let Ok(stmts) = mac.parse_body_with(Block::parse_within) {
            self.stmts(&stmts);
        } else {
            self.tokens(mac.tokens.clone());
        }
    }
}
