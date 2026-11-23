use std::{borrow::Cow, collections::HashSet, mem, slice};

use proc_macro2::{Ident, Span, TokenStream};
use syn::{
    Attribute, Item, ItemMacro, Macro, Meta, Token, UseTree, ext::IdentExt, parse::Parser,
    punctuated::Punctuated,
};

use super::{
    super::{cfg::Truth, rules::Rules},
    DefinitionSite, Export, Loaded, PathCall, Place, Scanner, SpanStart, Unscanned,
    attrs::prepend_attrs,
    flatten::{Expand, cfg_attribute, cfg_call, cfg_none, cfg_predicates, parse_items},
    resolve::{MAX_DEPTH, Resolver},
    scope::{Binding, Namespace, Resolved, Scope},
};

#[derive(Clone)]
pub(super) struct MacroRules {
    name: String,
    pub(super) rules: Option<Rules>,
    rel: String,
    span: Span,
    condition: Vec<TokenStream>,
    certain: bool,
    defined: Truth,
    instance: usize,
}

pub(super) struct Imported {
    name: String,
    library: usize,
    export: usize,
    condition: Vec<TokenStream>,
    certain: bool,
    defined: Truth,
    import: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Local,
    MacroUse(usize),
    Path,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Found {
    Export {
        library: usize,
        export: usize,
    },
    Own {
        path: Vec<String>,
        definition: usize,
    },
}

struct Candidate<'t> {
    rules: &'t Rules,
    rel: &'t str,
    span: Span,
    condition: Cow<'t, [TokenStream]>,
    defined: Truth,
    origin: Origin,
    instance: Option<usize>,
}

impl Candidate<'_> {
    fn either(&mut self, other: Self) {
        let text = |condition: &[TokenStream]| {
            condition
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        if text(&self.condition) != text(&other.condition) {
            let span = self.span;
            let either = cfg_call(
                "any",
                [
                    cfg_call("all", self.condition.to_vec(), span),
                    cfg_call("all", other.condition.to_vec(), span),
                ],
                span,
            );
            self.condition = Cow::Owned(vec![either]);
        }
        self.defined = self.defined.or(other.defined);
    }
}

#[derive(Default)]
pub(super) struct Textual {
    pub(super) macros: Vec<MacroRules>,
    pub(super) conditions: Vec<TokenStream>,
    depth: usize,
    pub(super) uncertain: bool,
}

impl Textual {
    fn condition_with(&self, attrs: &[Attribute]) -> Vec<TokenStream> {
        let mut condition = self.conditions.clone();
        condition.extend(cfg_predicates(attrs));
        condition
    }

    fn hides(&self, name: &str) -> bool {
        self.macros
            .iter()
            .any(|definition| definition.name == name && definition.certain)
    }

    pub(super) fn hidden(&self) -> HashSet<String> {
        self.macros
            .iter()
            .filter(|definition| definition.certain)
            .map(|definition| definition.name.clone())
            .collect()
    }
}

const MAX_EXPANSIONS: usize = 128;

const MAX_EXPANDED_CALLS: usize = 10_000;

pub(super) fn call_site(mac: &Macro) -> SpanStart {
    SpanStart::of(mac.bang_token.span)
}

fn is_bare(path: &syn::Path) -> bool {
    path.leading_colon.is_none() && path.segments.len() == 1
}

fn bare_names(tree: &UseTree, names: &mut Vec<(String, String)>) {
    match tree {
        UseTree::Name(name) if name.ident != "self" => {
            let ident = name.ident.unraw().to_string();
            names.push((ident.clone(), ident));
        }
        UseTree::Rename(rename) if rename.ident != "self" && rename.rename != "_" => {
            names.push((
                rename.ident.unraw().to_string(),
                rename.rename.unraw().to_string(),
            ));
        }
        UseTree::Group(group) => {
            for tree in &group.items {
                bare_names(tree, names);
            }
        }
        _ => {}
    }
}

impl<'a> Scanner<'a> {
    pub(super) fn call_macros(
        &mut self,
        module: &[String],
        path: &syn::Path,
        indexing: Option<&Scope>,
    ) -> (Option<Vec<Found>>, bool) {
        let mut resolver = Resolver::new(self, &[]);
        if let Some(scope) = indexing {
            resolver = resolver.indexing(module, scope);
        }
        let resolved = if is_bare(path) {
            let bound = path.segments.first().and_then(|segment| {
                let name = segment.ident.unraw().to_string();
                resolver.module(module)?.bound(Namespace::Macros, &name)
            });
            let Some(bound) = bound else {
                return (Some(Vec::new()), true);
            };
            let certain = bound.certain;
            let resolved = resolver.binding(module, 0, Namespace::Macros, &bound.binding, 0);
            (resolved, certain)
        } else {
            (resolver.syn_path(module, Namespace::Macros, path), true)
        };
        let (resolved, certain) = resolved;
        (self.path_macros(&resolved), certain)
    }

    fn name_macro(&mut self, path: Vec<String>, definition: MacroRules) {
        let definitions = self.macro_paths.entry(path).or_default();
        let same = definitions
            .iter_mut()
            .find(|known| known.instance == definition.instance);
        match same {
            Some(known) => *known = definition,
            None => definitions.push(definition),
        }
    }

    fn instance(&mut self, rel: &str, span: Span) -> usize {
        let place = DefinitionSite::new(rel.to_owned(), span);
        let before = self.read_at.entry(place.clone()).or_default();
        let key = (place, *before);
        *before += 1;
        let next = self.instances.len();
        *self.instances.entry(key).or_insert(next)
    }

    pub(super) fn define(
        &mut self,
        item: &ItemMacro,
        name: &Ident,
        place: Place,
        scope: &mut Scope,
        textual: &mut Textual,
    ) {
        let condition = textual.condition_with(&item.attrs);
        let span = Span::call_site();
        let holds = self
            .krate
            .cfg
            .holds(cfg_call("all", condition.clone(), span));
        if holds == Truth::False {
            return;
        }
        let rel = self.file_of(name.span(), place.rel);
        let rules = Rules::parse(item.mac.tokens.clone());
        let instance = self.instance(&rel, name.span());
        let definition = MacroRules {
            name: name.unraw().to_string(),
            rules,
            rel,
            span: name.span(),
            condition,
            certain: holds == Truth::True && !textual.uncertain,
            defined: if textual.uncertain {
                Truth::Unknown
            } else {
                Truth::True
            },
            instance,
        };
        if self.exports.is_some() {
            self.crate_spans
                .extend(definition.rules.iter().flat_map(Rules::crate_spans));
        }
        self.export_macro(&item.attrs, &definition, holds, place.module, scope);
        textual.macros.push(definition);
    }

    /// Records a definition that `#[macro_export]` puts at the crate root.
    fn export_macro(
        &mut self,
        attrs: &[Attribute],
        definition: &MacroRules,
        holds: Truth,
        module: &[String],
        scope: &mut Scope,
    ) {
        let effective = self.krate.cfg.effective(attrs);
        let export = effective
            .iter()
            .filter(|(_, meta)| meta.path().is_ident("macro_export"));
        let exported = export
            .clone()
            .fold(Truth::False, |truth, (applies, _)| truth.or(*applies));
        if exported == Truth::False {
            return;
        }
        let local_inner_macros = export.clone().any(|(_, meta)| {
            matches!(meta, Meta::List(list) if list
                .parse_args_with(Punctuated::<Ident, Token![,]>::parse_terminated)
                .is_ok_and(|args| args.iter().any(|arg| arg == "local_inner_macros")))
        });
        let mut rules = definition.rules.clone();
        if local_inner_macros && let Some(rules) = &mut rules {
            rules.local_inner_macros();
            if self.exports.is_some() {
                self.crate_spans.extend(rules.crate_spans());
            }
        }
        let defined = definition.defined.and(exported);
        if let Some(exports) = &mut self.exports {
            exports.push(Export {
                name: definition.name.clone(),
                rules: rules.clone(),
                rel: definition.rel.clone(),
                span: definition.span,
                defined: holds.and(exported),
            });
        }
        let crate_root = [self.krate.name.to_owned()];
        let path = vec![self.krate.name.to_owned(), definition.name.clone()];
        let certain = definition.certain && exported == Truth::True;
        let root = if module.len() == 1 {
            Some(scope)
        } else {
            match self.around.iter_mut().find(|(module, _)| module.len() == 1) {
                Some((_, root)) => Some(root),
                None => self.modules.get_mut(crate_root.as_slice()),
            }
        };
        if let Some(root) = root {
            root.define(
                Namespace::Macros,
                definition.name.clone(),
                Binding::Item(None, path.clone()),
                certain,
            );
        }
        self.name_macro(
            path,
            MacroRules {
                rules,
                certain,
                defined,
                ..definition.clone()
            },
        );
    }

    pub(super) fn use_macros(
        &mut self,
        item: &syn::ItemUse,
        place: Place,
        scope: &mut Scope,
        textual: &Textual,
    ) {
        if self.krate.edition_2015 || item.leading_colon.is_some() {
            return;
        }
        let certain = match self.krate.cfg.enabled(&item.attrs) {
            Truth::True => !textual.uncertain,
            Truth::Unknown => false,
            Truth::False => return,
        };
        let condition = textual.condition_with(&item.attrs);
        let mut names = Vec::new();
        bare_names(&item.tree, &mut names);
        for (name, binds) in names {
            let mut definitions = Vec::new();
            let mut hidden = false;
            for definition in textual.macros.iter().rev() {
                if definition.name != name {
                    continue;
                }
                definitions.push(definition);
                if definition.certain {
                    hidden = true;
                    break;
                }
            }
            if definitions.is_empty() {
                continue;
            }
            let mut path = place.module.to_vec();
            path.push(binds.clone());
            for definition in definitions {
                let mut both = definition.condition.clone();
                both.extend(condition.iter().cloned());
                self.name_macro(
                    path.clone(),
                    MacroRules {
                        condition: both,
                        certain: definition.certain && certain,
                        ..definition.clone()
                    },
                );
            }
            let used = Binding::Use {
                krate: None,
                path: vec![name],
                leading: false,
            };
            scope.bring_in(&binds, &used, &Binding::Item(None, path), !hidden);
        }
    }

    pub(super) fn import_macros(&mut self, items: &[Item], textual: &Textual) {
        for (import, item) in items.iter().enumerate() {
            let Item::ExternCrate(item) = item else {
                continue;
            };
            let condition = textual.condition_with(&item.attrs);
            let holds = self
                .krate
                .cfg
                .holds(cfg_call("all", condition.clone(), Span::call_site()));
            let macro_use = self
                .krate
                .cfg
                .effective(&item.attrs)
                .iter()
                .any(|(applies, meta)| {
                    *applies != Truth::False && meta.path().is_ident("macro_use")
                });
            if holds == Truth::False || !macro_use {
                continue;
            }
            let extern_name = item.ident.unraw().to_string();
            let Some(&library) = self.krate.dependencies.get(&extern_name) else {
                continue;
            };
            let Some(exports) = self
                .library(library)
                .and_then(|library| library.exports.as_ref())
            else {
                continue;
            };
            let exports: Vec<_> = exports
                .iter()
                .map(|export| (export.name.clone(), export.defined))
                .collect();
            for (index, (name, exported)) in exports.into_iter().enumerate() {
                let imported = self.imported(&item.attrs, &name);
                if imported == Truth::False || exported == Truth::False {
                    continue;
                }
                let defined = exported.and(imported);
                self.prelude.push(Imported {
                    name,
                    library,
                    export: index,
                    condition: condition.clone(),
                    certain: holds == Truth::True && defined == Truth::True,
                    defined,
                    import,
                });
            }
        }
    }

    fn imported(&self, attrs: &[Attribute], name: &str) -> Truth {
        self.applies(attrs, |meta| match meta {
            Meta::Path(path) => path.is_ident("macro_use"),
            Meta::List(list) => {
                list.path.is_ident("macro_use")
                    && list
                        .parse_args_with(Punctuated::<Ident, Token![,]>::parse_terminated)
                        .is_ok_and(|names| names.iter().any(|named| named.unraw() == name))
            }
            Meta::NameValue(_) => false,
        })
    }

    fn library(&mut self, id: usize) -> Option<&Scanner<'a>> {
        if !self.libraries.contains_key(&id) {
            let krate = self.crates.get(id)?;
            let mut reader = if krate.cfg.proc_macro {
                let mut reader = Scanner::new(self.crates, id, krate, self.disallowed);
                reader.exports = Some(Vec::new());
                reader
            } else {
                Scanner::read(self.crates, id, krate, self.disallowed, true)
            };
            reader.files.clear();
            for (source, rel) in mem::take(&mut reader.sources) {
                self.foreign.insert(source.clone());
                self.sources
                    .insert(source, self.relative(&krate.root.join(rel)));
            }
            for export in reader.exports.iter_mut().flatten() {
                export.rel = self.relative(&krate.root.join(&export.rel));
            }
            for (key, dollar_id) in mem::take(&mut reader.dollar_crates) {
                self.dollar_crates.entry(key).or_insert(dollar_id);
            }
            for span in mem::take(&mut reader.crate_spans) {
                self.dollar_crates.entry(SpanStart::of(span)).or_insert(id);
            }
            self.libraries.insert(id, reader);
        }
        self.libraries.get(&id)
    }

    fn export(&self, library: usize, export: usize) -> Option<&Export> {
        self.libraries.get(&library)?.exports.as_ref()?.get(export)
    }

    fn path_macros(&mut self, resolved: &Resolved) -> Option<Vec<Found>> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        for alternative in resolved.alternatives() {
            let Resolved::Path { krate, path } = alternative else {
                return None;
            };
            match krate {
                Some(id) if *id != self.id => {
                    self.collect_foreign_macros(*id, path.clone(), 0, &mut seen, &mut found)?;
                }
                _ if path.first().map(String::as_str) == Some(self.krate.name) => {
                    let count = self.macro_paths.get(path)?.len();
                    for definition in 0..count {
                        let own = Found::Own {
                            path: path.clone(),
                            definition,
                        };
                        if !found.contains(&own) {
                            found.push(own);
                        }
                    }
                }
                _ => return None,
            }
        }
        Some(found)
    }

    fn collect_foreign_macros(
        &mut self,
        id: usize,
        path: Vec<String>,
        depth: usize,
        seen: &mut HashSet<(usize, Vec<String>)>,
        found: &mut Vec<Found>,
    ) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        if !seen.insert((id, path.clone())) {
            return Some(());
        }
        let reader = self.library(id)?;
        let targets = Resolver::new(reader, &[]).step(path, Namespace::Macros, 0);
        for target in targets.into_alternatives() {
            let Resolved::Path {
                krate: target_krate,
                path: target_path,
            } = target
            else {
                return None;
            };
            if let Some(next_id) = target_krate
                && next_id != id
            {
                self.collect_foreign_macros(next_id, target_path, depth + 1, seen, found)?;
                continue;
            }
            let (name, module) = target_path.split_last()?;
            let reader = self.libraries.get(&id)?;
            if module.first().map(String::as_str) != Some(reader.krate.name) {
                return None;
            }
            let scope = reader.modules.get(module)?;
            match scope.get(Namespace::Macros, name) {
                None if !scope.globs => continue,
                Some(_) if module.len() == 1 => {}
                _ => return None,
            }
            let exports = reader.exports.as_ref()?;
            let mut any = false;
            for (export, _) in exports
                .iter()
                .enumerate()
                .filter(|(_, export)| export.name == *name)
            {
                any = true;
                let export = Found::Export {
                    library: id,
                    export,
                };
                if !found.contains(&export) {
                    found.push(export);
                }
            }
            if !any {
                return None;
            }
        }
        Some(())
    }

    fn candidates<'t>(
        &'t self,
        textual: &'t Textual,
        scope: &Scope,
        name: &str,
        imported: Option<(&[Found], bool)>,
    ) -> Option<Vec<Candidate<'t>>> {
        let mut candidates = Vec::new();
        for definition in textual.macros.iter().rev() {
            if definition.name != name {
                continue;
            }
            candidates.push(Candidate {
                rules: definition.rules.as_ref()?,
                rel: &definition.rel,
                span: definition.span,
                condition: Cow::Borrowed(&definition.condition),
                defined: definition.defined,
                origin: Origin::Local,
                instance: Some(definition.instance),
            });
            if definition.certain {
                return Some(candidates);
            }
        }
        let used = imported.map_or(&[][..], |(found, _)| found);
        if let Some((found, certain)) = imported {
            let found: Vec<_> = self
                .found_candidates(found, certain)?
                .into_iter()
                .filter(|candidate| {
                    candidate.instance.is_none()
                        || !candidates
                            .iter()
                            .any(|local| local.instance == candidate.instance)
                })
                .collect();
            candidates.extend(found);
            if certain {
                return Some(candidates);
            }
        }
        let mut prelude = self
            .prelude
            .iter()
            .filter(|imported| imported.name == name)
            .peekable();
        if prelude.peek().is_some() {
            if scope.globs {
                return None;
            }
            let mut hidden = None;
            for imported in prelude.rev() {
                if hidden.is_some_and(|import| imported.import != import) {
                    break;
                }
                let read = used.iter().any(|found| {
                    matches!(found, Found::Export { library, export }
                        if *library == imported.library && *export == imported.export)
                });
                if !read {
                    let export = self.export(imported.library, imported.export)?;
                    candidates.push(Candidate {
                        rules: export.rules.as_ref()?,
                        rel: &export.rel,
                        span: export.span,
                        condition: Cow::Borrowed(&imported.condition),
                        defined: imported.defined,
                        origin: Origin::MacroUse(imported.import),
                        instance: None,
                    });
                }
                if imported.certain {
                    hidden = Some(imported.import);
                }
            }
        }
        (!candidates.is_empty()).then_some(candidates)
    }

    fn found_candidates(&self, found: &[Found], certain: bool) -> Option<Vec<Candidate<'_>>> {
        let mut places = HashSet::new();
        let mut definitions = Vec::new();
        for found in found {
            let definition = match found {
                Found::Export { library, export } => {
                    let export = self.export(*library, *export)?;
                    places.insert((Some(*library), vec![export.name.clone()]));
                    Candidate {
                        rules: export.rules.as_ref()?,
                        rel: &export.rel,
                        span: export.span,
                        condition: Cow::Borrowed(&[]),
                        defined: export.defined,
                        origin: Origin::Path,
                        instance: None,
                    }
                }
                Found::Own { path, definition } => {
                    let definition = self.macro_paths.get(path)?.get(*definition)?;
                    places.insert((None, path.clone()));
                    Candidate {
                        rules: definition.rules.as_ref()?,
                        rel: &definition.rel,
                        span: definition.span,
                        condition: Cow::Borrowed(&definition.condition),
                        defined: definition.defined,
                        origin: Origin::Path,
                        instance: Some(definition.instance),
                    }
                }
            };
            let same = definitions.iter_mut().find(|known: &&mut Candidate| {
                definition.instance.is_some() && known.instance == definition.instance
            });
            match same {
                Some(known) => known.either(definition),
                None => definitions.push(definition),
            }
        }
        let one = certain && places.len() == 1;
        let candidates = definitions
            .into_iter()
            .rev()
            .map(|candidate| Candidate {
                defined: if one {
                    candidate.defined
                } else {
                    Truth::Unknown
                },
                ..candidate
            })
            .collect();
        Some(candidates)
    }

    pub(super) fn expand(
        &mut self,
        call: &ItemMacro,
        place: Place,
        scope: &mut Scope,
        textual: &mut Textual,
    ) -> bool {
        let path = &call.mac.path;
        let Some(name) = path.segments.last().map(|segment| &segment.ident) else {
            return false;
        };
        if self.krate.cfg.enabled(&call.attrs) == Truth::False {
            return false;
        }
        let bare = is_bare(path);
        let unraw = name.unraw().to_string();
        let found = if bare && textual.hides(&unraw) {
            None
        } else {
            let (found, certain) = self.call_macros(place.module, path, Some(scope));
            self.path_calls.push(PathCall {
                module: place.module.to_vec(),
                path: path.clone(),
                found: found.clone(),
            });
            let Some(found) = found else {
                return false;
            };
            if found.is_empty() {
                if !bare {
                    return false;
                }
                None
            } else {
                Some((found, certain))
            }
        };
        let found = found
            .as_ref()
            .map(|(found, certain)| (&found[..], *certain));
        let candidates = if bare {
            self.candidates(textual, scope, &unraw, found)
        } else {
            found.and_then(|(found, certain)| self.found_candidates(found, certain))
        };
        let Some(candidates) = candidates else {
            return false;
        };
        let file = self.file_of(name.span(), place.rel);
        let start = name.span().start();
        let limit = if textual.depth == MAX_EXPANSIONS {
            Some(format!(
                "`{name}!` expands to more than {MAX_EXPANSIONS} nested macro calls"
            ))
        } else if self.expansions == MAX_EXPANDED_CALLS {
            Some(format!(
                "the crate's macros expand to more than {MAX_EXPANDED_CALLS} calls"
            ))
        } else {
            None
        };
        if let Some(reason) = limit {
            self.unscanned(Unscanned {
                file,
                line: Some(start.line),
                reason,
            });
            return false;
        }
        let call_condition = cfg_predicates(&call.attrs);
        let mut later = Vec::new();
        let mut group: Option<(Origin, Option<TokenStream>)> = None;
        let mut expansions = Vec::new();
        for candidate in candidates {
            let span = candidate.span;
            if candidate.origin == Origin::Local
                || group
                    .as_ref()
                    .is_some_and(|(current, _)| *current != candidate.origin)
            {
                later.extend(group.take().and_then(|(_, hides)| hides));
            }
            let mut condition = call_condition.clone();
            condition.extend(candidate.condition.iter().cloned());
            if !later.is_empty() {
                condition.push(cfg_none(later.clone(), span));
            }
            let hides = (candidate.defined == Truth::True)
                .then(|| cfg_call("all", candidate.condition.to_vec(), span));
            match candidate.origin {
                Origin::Local => later.extend(hides),
                origin => {
                    let (_, item) = group.get_or_insert((origin, None));
                    if hides.is_some() {
                        *item = hides;
                    }
                }
            }
            let defined = candidate.defined;
            let predicate = cfg_call("all", condition.clone(), span);
            if self.krate.cfg.holds(predicate.clone()).and(defined) == Truth::False {
                continue;
            }
            let Some(mut items) = candidate
                .rules
                .expand(call.mac.tokens.clone())
                .and_then(|tokens| parse_items.parse2(tokens).ok())
            else {
                return false;
            };
            Expand.fragment(&mut items);
            if !condition.is_empty() {
                let cfg = cfg_attribute(predicate, span);
                for item in &mut items {
                    prepend_attrs(item, slice::from_ref(&cfg));
                }
            }
            let at = DefinitionSite::new(candidate.rel.to_owned(), span);
            expansions.push((items, at, candidate.instance.is_none(), defined));
        }
        self.expansions += 1;
        for (mut items, at, foreign, defined) in expansions {
            let rel = at.file.clone();
            if !foreign {
                self.expanded.insert(at);
            }
            let inner = Place { rel: &rel, ..place };
            textual.depth += 1;
            let uncertain = textual.uncertain;
            textual.uncertain |= defined != Truth::True;
            self.index(&mut items, inner, scope, textual);
            textual.uncertain = uncertain;
            textual.depth -= 1;
            self.files.push(Loaded {
                attrs: Vec::new(),
                items,
                rel,
                module: place.module.to_vec(),
                mod_dir: place.mod_dir.to_path_buf(),
                path_dir: place.path_dir.to_path_buf(),
            });
        }
        true
    }
}
