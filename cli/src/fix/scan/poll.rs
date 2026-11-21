use super::*;

#[derive(Default)]
pub(super) struct Poll {
    awaits: HashMap<String, Vec<Await>>,
    entry: Option<PathBuf>,
    root: Option<(PathBuf, usize)>,
}

struct Await {
    caller: Option<String>,
    boundary: Option<&'static str>,
}

impl Poll {
    pub(super) fn new(package: &Path) -> Self {
        Self {
            entry: crate::deps::metadata::tokio_entry(package),
            ..Self::default()
        }
    }

    pub(super) fn add(&mut self, file: &Path, tree: &syn::File, macros: &BTreeSet<String>) -> bool {
        if self.entry.as_deref() == Some(file) && tree.attrs.is_empty() {
            struct Shadow(bool);
            impl<'ast> Visit<'ast> for Shadow {
                fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {
                    self.0 = true;
                }
                fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
                    let mut names = BTreeSet::new();
                    idents(item.to_token_stream(), &mut names);
                    self.0 |=
                        names.contains("tokio") || item.to_token_stream().to_string().contains('*');
                }
                fn visit_item_extern_crate(&mut self, _: &'ast syn::ItemExternCrate) {
                    self.0 = true;
                }
                fn visit_ident(&mut self, ident: &'ast syn::Ident) {
                    self.0 |= ident == "tokio";
                }
                fn visit_path(&mut self, _: &'ast syn::Path) {}
            }
            let mut shadow = Shadow(false);
            shadow.visit_file(tree);
            if !shadow.0 {
                for item in &tree.items {
                    if let Item::Fn(f) = item
                        && f.sig.ident == "main"
                        && f.sig.asyncness.is_some()
                        && f.sig.inputs.is_empty()
                        && f.sig.generics.params.is_empty()
                        && matches!(f.vis, Visibility::Inherited)
                        && matches!(f.attrs.as_slice(), [attr] if main_attribute(attr))
                    {
                        self.root = Some((file.to_owned(), f.sig.ident.span().start().line));
                    }
                }
            }
        }
        struct Attributes<'a> {
            allowed: Option<&'a Attribute>,
            macros: &'a BTreeSet<String>,
            opaque: bool,
        }
        impl<'ast> Visit<'ast> for Attributes<'_> {
            fn visit_macro(&mut self, mac: &'ast Macro) {
                match expression_arguments(mac, self.macros) {
                    Some(args) => {
                        for expr in &args {
                            self.visit_expr(expr);
                        }
                    }
                    None => self.opaque = true,
                }
            }
            fn visit_attribute(&mut self, attr: &'ast Attribute) {
                if !self
                    .allowed
                    .is_some_and(|allowed| std::ptr::eq(allowed, attr))
                {
                    self.opaque |= macro_attribute(std::slice::from_ref(attr));
                }
            }
        }
        let allowed = tree.items.iter().find_map(|item| match item {
            Item::Fn(f) if self.root(file, &f.sig) => f.attrs.first(),
            _ => None,
        });
        let mut attrs = Attributes {
            allowed,
            macros,
            opaque: false,
        };
        attrs.visit_file(tree);
        Edges {
            poll: self,
            caller: None,
            boundary: None,
        }
        .visit_file(tree);
        attrs.opaque
    }

    pub(super) fn root(&self, file: &Path, sig: &syn::Signature) -> bool {
        self.root.as_ref().is_some_and(|(path, line)| {
            path == file && *line == sig.ident.span().start().line && sig.ident == "main"
        })
    }

    pub(super) fn prove(&self, krate: &Crate, name: &str) -> Result<(), String> {
        self.chain(krate, name, &mut BTreeSet::new())
    }

    fn chain(
        &self,
        krate: &Crate,
        name: &str,
        visiting: &mut BTreeSet<String>,
    ) -> Result<(), String> {
        let fail = |why| {
            Err(format!(
                "future boundary `{name}` {why}; the rewrite remains manual"
            ))
        };
        if krate.opaque_poll {
            return fail(
                "is in a package with an opaque macro that could generate an unseen caller",
            );
        }
        let Some(definitions) = krate.functions.get(name) else {
            return fail("has no unique definition");
        };
        let [function] = definitions.as_slice() else {
            return fail("has ambiguous definitions");
        };
        if function.wrapped {
            return fail("has an unproved attribute macro wrapper");
        }
        if !function.asynchronous || function.trait_method || function.public {
            return fail("is not a private async function with statically visible callers");
        }
        if name == "main" && self.root.is_some() {
            return if krate.occurrences("main") == 2 {
                Ok(())
            } else {
                fail("names the binary entry somewhere else (a call, value or macro)")
            };
        }
        if name == "main" {
            return fail("is not a verified Tokio binary entry");
        }
        if !visiting.insert(name.to_owned()) {
            return fail("has a recursive polling chain");
        }
        let uses = self.awaits.get(name).map(Vec::as_slice).unwrap_or_default();
        if uses.is_empty() || krate.occurrences(name) != uses.len() + 1 {
            return fail(
                "is not accounted for solely by direct awaits (it may be stored, returned, boxed or passed on)",
            );
        }
        for use_ in uses {
            if let Some(boundary) = use_.boundary {
                return fail(boundary);
            }
            let Some(caller) = &use_.caller else {
                return fail("has no enclosing async function");
            };
            self.chain(krate, caller, visiting)?;
        }
        visiting.remove(name);
        Ok(())
    }
}

fn main_attribute(attr: &Attribute) -> bool {
    let names: Vec<_> = attr
        .path()
        .segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect();
    if names != ["tokio", "main"] {
        return false;
    }
    match &attr.meta {
        syn::Meta::Path(_) => true,
        syn::Meta::List(list) if list.tokens.is_empty() => true,
        syn::Meta::List(_) => {
            let Ok(options) = attr.parse_args_with(
                Punctuated::<syn::MetaNameValue, syn::Token![,]>::parse_terminated,
            ) else {
                return false;
            };
            let Some(option) = options.first().filter(|_| options.len() == 1) else {
                return false;
            };
            option.path.is_ident("flavor")
                && matches!(&option.value, Expr::Lit(lit)
                if matches!(&lit.lit, syn::Lit::Str(value) if matches!(value.value().as_str(), "current_thread" | "multi_thread")))
        }
        _ => false,
    }
}

struct Edges<'p> {
    poll: &'p mut Poll,
    caller: Option<String>,
    boundary: Option<&'static str>,
}

impl Edges<'_> {
    fn function(&mut self, sig: &syn::Signature, body: &syn::Block) {
        let caller = self.caller.replace(sig.ident.unraw().to_string());
        let boundary = self.boundary.take();
        self.visit_block(body);
        self.caller = caller;
        self.boundary = boundary;
    }
}

impl<'ast> Visit<'ast> for Edges<'_> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.function(&f.sig, &f.block);
    }
    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.function(&f.sig, &f.block);
    }
    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        if let Some(block) = &f.default {
            self.function(&f.sig, block);
        }
    }
    fn visit_expr_await(&mut self, expr: &'ast syn::ExprAwait) {
        if let Expr::Async(block) = &*expr.base
            && expr.attrs.is_empty()
            && block.attrs.is_empty()
        {
            self.visit_block(&block.block);
            return;
        }
        let name = match &*expr.base {
            Expr::Call(call) if call.attrs.is_empty() => match &*call.func {
                Expr::Path(path) if path.qself.is_none() => path
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.unraw().to_string()),
                _ => None,
            },
            Expr::MethodCall(call) if call.attrs.is_empty() => {
                Some(call.method.unraw().to_string())
            }
            _ => None,
        };
        if expr.attrs.is_empty()
            && let Some(name) = name
        {
            self.poll.awaits.entry(name).or_default().push(Await {
                caller: self.caller.clone(),
                boundary: self.boundary,
            });
        }
        syn::visit::visit_expr_await(self, expr);
    }
    fn visit_expr_closure(&mut self, expr: &'ast syn::ExprClosure) {
        let old = self
            .boundary
            .replace("is awaited inside a closure whose polling context is unproved");
        syn::visit::visit_expr_closure(self, expr);
        self.boundary = old;
    }
    fn visit_expr_async(&mut self, expr: &'ast syn::ExprAsync) {
        let old = self
            .boundary
            .replace("is inside an async block that is not immediately awaited");
        syn::visit::visit_expr_async(self, expr);
        self.boundary = old;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str =
        "#[tokio::main(flavor = \"current_thread\")] async fn main() { outer().await; }";
    const CHAIN: &str = "async fn outer() { async { leaf().await }.await; } async fn leaf() {}";

    fn proof(source: &str) -> Result<(), String> {
        let file = Path::new("main.rs");
        let mut krate = Crate::default();
        krate.poll.entry = Some(file.to_owned());
        krate.add(file, source);
        krate.poll.prove(&krate, "leaf")
    }

    #[test]
    fn every_enclosing_future_and_every_use_needs_a_proof() {
        assert!(proof(&format!("{CHAIN} {MAIN}")).is_ok());
        for source in [
            format!("{CHAIN} {MAIN} fn other() {{ let _f = leaf(); }}"),
            format!("{CHAIN} {MAIN} fn other() {{ let _f = outer; }}"),
            format!("{CHAIN} {MAIN} fn other() {{ main(); }}"),
            format!("{CHAIN} {MAIN} use outer as renamed;"),
            format!(
                "async fn leaf() {{}} async fn outer() {{ #[wrapper] let _value = leaf().await; }} {MAIN}"
            ),
            format!(
                "async fn leaf() {{}} async fn outer() {{ #[wrapper] {{ leaf().await; }} }} {MAIN}"
            ),
            format!("{CHAIN} {MAIN} mod unrelated {{ async fn leaf() {{}} }}"),
            format!(
                "async fn leaf() {{}} async fn outer() {{ let _ = async {{ leaf().await }}; }} {MAIN}"
            ),
            format!(
                "async fn leaf() {{}} async fn outer() {{ (|| async {{ leaf().await }})().await; }} {MAIN}"
            ),
            format!("async fn leaf() {{}} async fn outer() {{ let f = leaf(); f.await; }} {MAIN}"),
            format!("async fn leaf() {{}} async fn outer() {{ Box::pin(leaf()).await; }} {MAIN}"),
            format!("async fn leaf() {{}} async fn outer() {{ unknown(leaf()).await; }} {MAIN}"),
            format!("async fn leaf() {{}} async fn outer() {{ invoke!(leaf).await; }} {MAIN}"),
            format!(
                "{CHAIN} {}",
                MAIN.replace("outer().await", "let _f = async { outer().await }")
            ),
            format!(
                "{CHAIN} {}",
                MAIN.replace("outer().await", "block_on(async { outer().await })")
            ),
        ] {
            assert!(proof(&source).is_err(), "{source}");
        }
    }

    #[test]
    fn only_the_unwrapped_unambiguous_binary_entry_is_a_root() {
        for attr in [
            "#[tokio::main]",
            "#[::tokio::main()]",
            "#[tokio::main(flavor = \"multi_thread\")]",
            "#[tokio::main(flavor = \"current_thread\",)]",
        ] {
            assert!(
                proof(&format!(
                    "{CHAIN} {attr} async fn main() {{ outer().await; }}"
                ))
                .is_ok()
            );
        }
        for attr in [
            "",
            "#[fake::main]",
            "#[tokio::test]",
            "#[wrapper] #[tokio::main]",
            "#[tokio::main(crate = \"tokio\")]",
            "#[tokio::main(start_paused = true)]",
            "#[tokio::main(flavor = \"local\")]",
            "#[cfg(any())] #[tokio::main]",
        ] {
            let source = format!("{CHAIN} {attr} async fn main() {{ outer().await; }}");
            assert!(proof(&source).is_err(), "{source}");
        }
        for extra in [
            "mod tokio {}",
            "use other as tokio;",
            "use other::*;",
            "extern crate other as tokio;",
            "struct tokio;",
            "type tokio = ();",
            "mod inner { #[tokio::main] async fn main() {} }",
            "#![wrapper]",
        ] {
            let source = format!("{extra} {CHAIN} {MAIN}");
            assert!(proof(&source).is_err(), "{source}");
        }
        assert!(
            proof(&format!(
                "{CHAIN} {}",
                MAIN.replace("fn main", "fn application")
            ))
            .is_err()
        );
    }

    #[test]
    fn private_or_send_is_not_a_polling_boundary() {
        for outer in [
            "pub async fn outer() { leaf().await; }",
            "#[wrapper] async fn outer() { leaf().await; }",
            "trait Run { async fn outer() { leaf().await; } }",
            "fn outer() -> impl std::future::Future<Output = ()> + Send { async { leaf().await; } }",
        ] {
            let source = format!("async fn leaf() {{}} {outer} {MAIN}");
            assert!(proof(&source).is_err(), "{source}");
        }
    }
}
