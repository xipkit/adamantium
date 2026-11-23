use std::{
    collections::hash_map::Entry,
    fs, mem,
    path::{Path, PathBuf},
};

use proc_macro2::{Ident, Span, TokenStream};
use syn::{
    Attribute, Expr, ExprLit, File, Item, ItemMod, Lit, Meta, Token,
    ext::IdentExt,
    punctuated::Punctuated,
    visit::{self, Visit},
    visit_mut::VisitMut,
};

use super::{
    super::cfg::Truth,
    Loaded, Place, Scanner, Unscanned,
    flatten::{Expand, cfg_call, cfg_none, cfg_predicates, conditional, reads_input},
    macros::{Textual, call_site},
    scope::Scope,
};

pub(super) struct ModuleFile {
    pub(super) path: PathBuf,
    pub(super) mod_rs: bool,
    condition: Option<TokenStream>,
}

pub(super) enum Location {
    Inline { dir: PathBuf },
    Files(Vec<ModuleFile>),
    Unscannable(String),
}

struct FirstSpan(Option<Span>);

impl<'ast> Visit<'ast> for FirstSpan {
    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if self.0.is_none() {
            visit::visit_attribute(self, attr);
        }
    }

    fn visit_item(&mut self, item: &'ast Item) {
        if self.0.is_none() {
            visit::visit_item(self, item);
        }
    }

    fn visit_ident(&mut self, ident: &'ast Ident) {
        self.0.get_or_insert(ident.span());
    }
}

impl<'a> Scanner<'a> {
    pub(super) fn load(
        &mut self,
        path: &Path,
        module: Vec<String>,
        mod_rs: bool,
        from: Option<(&str, Span)>,
        textual: &mut Textual,
    ) -> Vec<Attribute> {
        let rel = self.relative(path);
        let Some(mut file) = self.parse_source(path, &rel, from) else {
            return Vec::new();
        };
        let mut first = FirstSpan(None);
        first.visit_file(&file);
        if let FirstSpan(Some(span)) = first {
            self.sources.insert(span.file(), rel.clone());
        }
        if self.krate.cfg.enabled(&file.attrs) == Truth::False {
            return Vec::new();
        }
        Expand.visit_file_mut(&mut file);
        if module.len() == 1 {
            self.scan.forbids_unsafe |= self.forbids_unsafe(&file.attrs);
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let mod_dir = if mod_rs {
            dir.clone()
        } else {
            dir.join(path.file_stem().unwrap_or_default())
        };
        let outer = textual.conditions.len();
        textual.conditions.extend(cfg_predicates(&file.attrs));
        if module.len() == 1 {
            self.import_macros(&file.items, textual);
        }
        let mut scope = Scope::default();
        let place = Place {
            module: &module,
            mod_dir: &mod_dir,
            path_dir: &dir,
            rel: &rel,
        };
        self.index(&mut file.items, place, &mut scope, textual);
        textual.conditions.truncate(outer);
        self.add_scope(&module, scope);
        let attrs = file.attrs.clone();
        self.files.push(Loaded {
            attrs: file.attrs,
            items: file.items,
            rel,
            module,
            mod_dir,
            path_dir: dir,
        });
        attrs
    }

    /// Reads and parses a module's file the first time it is loaded. A file
    /// that can't be read or parsed is recorded as unscanned.
    fn parse_source(&mut self, path: &Path, rel: &str, from: Option<(&str, Span)>) -> Option<File> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                let reason = format!("reading {rel}: {error}");
                self.unscanned(match from {
                    Some((file, span)) => Unscanned {
                        file: file.to_owned(),
                        line: Some(span.start().line),
                        reason,
                    },
                    None => Unscanned {
                        file: rel.to_owned(),
                        line: None,
                        reason,
                    },
                });
                return None;
            }
        };
        if !self.visited.insert(path.to_path_buf()) {
            return None;
        }
        match syn::parse_file(&text) {
            Ok(file) => Some(file),
            Err(error) => {
                self.unscanned(Unscanned {
                    file: rel.to_owned(),
                    line: Some(error.span().start().line),
                    reason: format!("syn could not parse it: {error}"),
                });
                None
            }
        }
    }

    pub(super) fn index(
        &mut self,
        items: &mut Vec<Item>,
        place: Place,
        scope: &mut Scope,
        textual: &mut Textual,
    ) {
        self.bind(scope, items.iter(), Some(place.module), textual.uncertain);
        let mut indexed = Vec::with_capacity(items.len());
        for mut item in mem::take(items) {
            match &mut item {
                Item::Mod(item) => {
                    self.around.push((place.module.to_vec(), mem::take(scope)));
                    self.child(item, place, textual);
                    if let Some((_, around)) = self.around.pop() {
                        *scope = around;
                    }
                }
                Item::Use(item) => self.use_macros(item, place, scope, textual),
                Item::Macro(call) => {
                    match &call.ident {
                        Some(name) => self.define(call, name, place, scope, textual),
                        None => {
                            if !reads_input(&call.mac) && self.expand(call, place, scope, textual) {
                                continue;
                            }
                        }
                    }
                    let hidden = textual.hidden();
                    match self.hidden.entry(call_site(&call.mac)) {
                        Entry::Occupied(mut known) => {
                            known.get_mut().retain(|name| hidden.contains(name));
                        }
                        Entry::Vacant(place) => {
                            place.insert(hidden);
                        }
                    }
                }
                _ => {}
            }
            indexed.push(item);
        }
        *items = indexed;
    }

    fn add_scope(&mut self, module: &[String], scope: Scope) {
        match self.modules.get_mut(module) {
            Some(existing) => existing.merge(scope),
            None => {
                self.modules.insert(module.to_vec(), scope);
            }
        }
    }

    fn child(&mut self, item: &mut ItemMod, place: Place, textual: &mut Textual) {
        if self.krate.cfg.enabled(&item.attrs) == Truth::False {
            return;
        }
        let mut child = place.module.to_vec();
        child.push(item.ident.unraw().to_string());
        let macros = textual.macros.len();
        let conditions = textual.conditions.len();
        textual.conditions.extend(cfg_predicates(&item.attrs));
        match self.locate(item, place.mod_dir, place.path_dir) {
            Location::Inline { dir } => {
                if let Some((_, items)) = &mut item.content {
                    let mut scope = Scope::default();
                    let inner = Place {
                        module: &child,
                        mod_dir: &dir,
                        path_dir: &dir,
                        rel: place.rel,
                    };
                    self.index(items, inner, &mut scope, textual);
                    self.add_scope(&child, scope);
                }
                self.keep_macros(&item.attrs, macros, textual);
            }
            Location::Files(files) => {
                let declared = self.file_of(item.ident.span(), place.rel);
                for file in files {
                    let from = Some((declared.as_str(), item.ident.span()));
                    let outer = textual.conditions.len();
                    let macros = textual.macros.len();
                    textual.conditions.extend(file.condition);
                    let mut attrs = item.attrs.clone();
                    attrs.extend(self.load(&file.path, child.clone(), file.mod_rs, from, textual));
                    textual.conditions.truncate(outer);
                    self.keep_macros(&attrs, macros, textual);
                }
            }
            Location::Unscannable(reason) => self.unscanned(Unscanned {
                file: self.file_of(item.ident.span(), place.rel),
                line: Some(item.ident.span().start().line),
                reason,
            }),
        }
        textual.conditions.truncate(conditions);
    }

    fn keep_macros(&self, attrs: &[Attribute], start: usize, textual: &mut Textual) {
        match self.macro_use(attrs) {
            Truth::True => {}
            Truth::Unknown => {
                for definition in textual.macros.iter_mut().skip(start) {
                    definition.rules = None;
                }
            }
            Truth::False => textual.macros.truncate(start),
        }
    }

    fn macro_use(&self, attrs: &[Attribute]) -> Truth {
        self.applies(
            attrs,
            |meta| matches!(meta, Meta::Path(path) if path.is_ident("macro_use")),
        )
    }

    /// Whether some attribute that `is` of a kind applies, once `cfg_attr`
    /// is expanded.
    pub(super) fn applies(&self, attrs: &[Attribute], is: impl Fn(&Meta) -> bool) -> Truth {
        self.krate
            .cfg
            .effective(attrs)
            .iter()
            .filter(|(_, meta)| is(meta))
            .fold(Truth::False, |truth, (applies, _)| truth.or(*applies))
    }

    /// Each place a module's `#[path]` attributes may put it, with the cfg
    /// that selects it: `None` for the default location. Places whose cfg is
    /// false are left out.
    fn path_places(&self, item: &ItemMod) -> Vec<(Option<String>, TokenStream)> {
        let span = item.ident.span();
        let mut places = Vec::new();
        let mut later = Vec::new();
        for (conditions, meta) in conditional(&item.attrs).into_iter().rev() {
            let Meta::NameValue(value) = &meta else {
                continue;
            };
            let Expr::Lit(ExprLit {
                lit: Lit::Str(lit), ..
            }) = &value.value
            else {
                continue;
            };
            if !value.path.is_ident("path") {
                continue;
            }
            let applies = cfg_call("all", conditions, span);
            let none_later = cfg_none(later.clone(), span);
            places.push((
                Some(lit.value()),
                cfg_call("all", [applies.clone(), none_later], span),
            ));
            later.push(applies);
        }
        places.push((None, cfg_none(later, span)));
        places.retain(|(_, selects)| self.krate.cfg.holds(selects.clone()) != Truth::False);
        places
    }

    pub(super) fn locate(&self, item: &ItemMod, mod_dir: &Path, path_dir: &Path) -> Location {
        let name = item.ident.unraw().to_string();
        let places = self.path_places(item);
        let default = || {
            [
                (mod_dir.join(format!("{name}.rs")), false),
                (mod_dir.join(&name).join("mod.rs"), true),
            ]
            .into_iter()
            .find(|(path, _)| path.is_file())
        };
        let file = |(path, mod_rs), condition| ModuleFile {
            path,
            mod_rs,
            condition,
        };
        match (&item.content, &places[..]) {
            (Some(_), [(path, _)]) => Location::Inline {
                dir: match path {
                    Some(path) => path_dir.join(path),
                    None => mod_dir.join(&name),
                },
            },
            (Some(_), _) => Location::Unscannable(format!(
                "module `{name}` has a #[path] that depends on an unknown cfg"
            )),
            (None, [(Some(path), _)]) => {
                Location::Files(vec![file((path_dir.join(path), true), None)])
            }
            (None, [(None, _)]) => default().map_or_else(
                || {
                    Location::Unscannable(format!(
                        "module `{name}` has no file: neither {name}.rs nor {name}/mod.rs"
                    ))
                },
                |found| Location::Files(vec![file(found, None)]),
            ),
            (None, places) => {
                let files: Vec<_> = places
                    .iter()
                    .filter_map(|(path, selects)| {
                        let found = match path {
                            Some(path) => {
                                let path = path_dir.join(path);
                                path.is_file().then_some((path, true))
                            }
                            None => default(),
                        };
                        found.map(|found| file(found, Some(selects.clone())))
                    })
                    .collect();
                if files.is_empty() {
                    Location::Unscannable(format!(
                        "module `{name}` has no file at any place its #[path], which depends on an unknown cfg, may put it"
                    ))
                } else {
                    Location::Files(files)
                }
            }
        }
    }

    fn forbids_unsafe(&self, attrs: &[Attribute]) -> bool {
        self.krate
            .cfg
            .effective(attrs)
            .iter()
            .any(|(applies, meta)| {
                *applies == Truth::True
                    && meta.path().is_ident("forbid")
                    && matches!(meta, Meta::List(list) if list
                    .parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
                    .is_ok_and(|lints| lints.iter().any(|lint| lint.is_ident("unsafe_code"))))
            })
    }
}
