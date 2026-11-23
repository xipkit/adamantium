use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
    path::Path,
    str::FromStr,
};

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::{Item, ItemUse, Lit, Pat, UseTree, ext::IdentExt, spanned::Spanned, visit::Visit};

use super::Edit;

fn has_out_of_line_module(tree: &syn::File) -> bool {
    struct Modules(bool);
    impl<'ast> Visit<'ast> for Modules {
        fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
            self.0 |= module.content.is_none();
            syn::visit::visit_item_mod(self, module);
        }
    }
    let mut modules = Modules(false);
    modules.visit_file(tree);
    modules.0
}

pub(super) fn coalesce(mut edits: Vec<Edit>) -> Vec<Edit> {
    edits.sort();
    let mut joined: Vec<Edit> = Vec::with_capacity(edits.len());
    for edit in edits {
        match joined.last_mut() {
            Some(last) if last.end == edit.start => {
                last.end = edit.end;
                last.replacement.push_str(&edit.replacement);
            }
            _ => joined.push(edit),
        }
    }
    joined
}

pub(super) struct Text<'a> {
    pub(super) source: &'a str,
    prefix: usize,
}

impl<'a> Text<'a> {
    pub(super) fn new(source: &'a str, tree: &syn::File) -> Self {
        Self {
            source,
            prefix: usize::from(source.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
                + tree.shebang.as_ref().map_or(0, String::len),
        }
    }

    pub(super) fn range(&self, span: Span) -> Option<Range<usize>> {
        let range = span.byte_range();
        let range = (range.start + self.prefix)..(range.end + self.prefix);
        self.source.get(range.clone()).map(|_| range)
    }

    pub(super) fn of<T: Spanned + ?Sized>(&self, node: &T) -> Option<Range<usize>> {
        self.range(node.span())
    }

    /// The source text of `node`.
    pub(super) fn slice<T: Spanned + ?Sized>(&self, node: &T) -> Option<&'a str> {
        self.source.get(self.of(node)?)
    }

    /// Whether `expr` starts with an attribute.
    pub(super) fn attributed(&self, expr: &syn::Expr) -> bool {
        self.slice(expr).is_some_and(|text| text.starts_with('#'))
    }

    pub(super) fn get(&self, range: Range<usize>) -> Option<&str> {
        self.source.get(range)
    }

    pub(super) fn line(&self, span: Span) -> usize {
        span.start().line
    }
}

pub(super) fn format_captures(format: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            continue;
        }
        let mut spec = String::new();
        for c in chars.by_ref() {
            if c == '}' {
                break;
            }
            spec.push(c);
        }
        let (argument, options) = spec.split_once(':').unwrap_or((spec.as_str(), ""));
        if is_identifier(argument.trim()) {
            names.push(argument.trim().to_owned());
        }
        for part in options.split('$').rev().skip(1) {
            let name: String = part
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if is_identifier(&name) {
                names.push(name);
            }
        }
    }
    names
}

fn is_identifier(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && name != "_"
}

/// Whether `op` is a compound assignment such as `+=`.
pub(super) fn is_assignment(op: &syn::BinOp) -> bool {
    matches!(
        op,
        syn::BinOp::AddAssign(_)
            | syn::BinOp::SubAssign(_)
            | syn::BinOp::MulAssign(_)
            | syn::BinOp::DivAssign(_)
            | syn::BinOp::RemAssign(_)
            | syn::BinOp::BitXorAssign(_)
            | syn::BinOp::BitAndAssign(_)
            | syn::BinOp::BitOrAssign(_)
            | syn::BinOp::ShlAssign(_)
            | syn::BinOp::ShrAssign(_)
    )
}

pub(super) fn bare(ident: &proc_macro2::Ident) -> String {
    ident.unraw().to_string()
}

/// The value of `literal` when it is a string literal.
pub(super) fn string_literal(literal: &proc_macro2::Literal) -> Option<String> {
    match syn::parse2::<Lit>(TokenTree::Literal(literal.clone()).into()) {
        Ok(Lit::Str(string)) => Some(string.value()),
        _ => None,
    }
}

pub(super) fn names_in(text: &str, name: &str) -> Option<usize> {
    fn count(tokens: TokenStream, name: &str) -> usize {
        tokens
            .into_iter()
            .map(|token| match token {
                TokenTree::Group(group) => count(group.stream(), name),
                TokenTree::Ident(ident) => usize::from(bare(&ident) == name),
                TokenTree::Literal(literal) => string_literal(&literal).map_or(0, |string| {
                    format_captures(&string)
                        .iter()
                        .filter(|capture| *capture == name)
                        .count()
                }),
                TokenTree::Punct(_) => 0,
            })
            .sum()
    }
    TokenStream::from_str(text)
        .ok()
        .map(|tokens| count(tokens, name))
}

pub(super) fn prune_imports(
    file: &Path,
    source: &str,
    edits: &[Edit],
    paths: &[&[&str]],
) -> Vec<Edit> {
    let Ok(tree) = syn::parse_file(source) else {
        return Vec::new();
    };
    if has_out_of_line_module(&tree) {
        return Vec::new();
    }
    let text = Text::new(source, &tree);
    coalesce(Imports::prune(&text, file, &tree, edits, paths))
}

/// What pruning leaves of a use tree.
enum Pruned {
    Removed,
    Unchanged,
    Rewritten(String),
}

struct Imports<'t> {
    text: &'t Text<'t>,
    paths: &'t [&'t [&'t str]],
    idents: HashMap<String, Vec<Range<usize>>>,
}

impl<'t> Imports<'t> {
    fn prune(
        text: &'t Text<'t>,
        file: &Path,
        tree: &syn::File,
        edits: &[Edit],
        paths: &'t [&'t [&'t str]],
    ) -> Vec<Edit> {
        let mut uses = Uses(Vec::new());
        uses.visit_file(tree);
        let mut imports = Imports {
            text,
            paths,
            idents: HashMap::new(),
        };
        if let Some(body) = text.source.get(text.prefix..)
            && let Ok(tokens) = TokenStream::from_str(body)
        {
            let paths: Vec<Range<usize>> =
                uses.0.iter().filter_map(|item| text.of(*item)).collect();
            imports.collect(tokens, &paths);
        }
        for item in &uses.0 {
            if item.leading_colon.is_none() {
                imports.roots(&item.tree);
            }
        }
        let mut pruned = Vec::new();
        let mut lines = Vec::new();
        for item in uses.0 {
            if !matches!(item.vis, syn::Visibility::Inherited) {
                continue;
            }
            let mut removed = BTreeSet::new();
            imports.unused(&item.tree, &mut Vec::new(), edits, &mut removed);
            if removed.is_empty() {
                continue;
            }
            let edit = match imports.rewrite(&item.tree, &removed) {
                Pruned::Rewritten(tree) => text.of(&item.tree).map(|range| (range, tree)),
                Pruned::Unchanged => None,
                Pruned::Removed => imports.line(item).map(|range| (range, String::new())),
            };
            match edit {
                Some((range, replacement)) if replacement.is_empty() => lines.push(range),
                Some((range, replacement)) => pruned.push((range, replacement)),
                None => {}
            }
        }
        lines.sort_by_key(|range| range.start);
        let mut runs: Vec<Range<usize>> = Vec::new();
        for range in lines {
            match runs.last_mut() {
                Some(run) if run.end == range.start => run.end = range.end,
                _ => runs.push(range),
            }
        }
        for run in runs {
            let run = imports.without_blank_line(run);
            pruned.push((run, String::new()));
        }
        pruned
            .into_iter()
            .map(|(range, replacement)| Edit::new(file, range, replacement))
            .collect()
    }

    fn collect(&mut self, tokens: TokenStream, uses: &[Range<usize>]) {
        let mut colons = 0;
        let mut module = false;
        for token in tokens {
            match token {
                TokenTree::Group(group) => {
                    module = false;
                    self.collect(group.stream(), uses);
                }
                TokenTree::Ident(ident) => {
                    let reachable = colons < 2 || module;
                    module = ident == "self" || ident == "super" || ident == "crate";
                    if reachable
                        && let Some(range) = self.text.range(ident.span())
                        && !uses
                            .iter()
                            .any(|item| item.start <= range.start && range.end <= item.end)
                    {
                        self.idents.entry(bare(&ident)).or_default().push(range);
                    }
                }
                TokenTree::Punct(ref punct) if punct.as_char() == ':' => {
                    colons += 1;
                    continue;
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => module = false,
            }
            colons = 0;
        }
    }

    fn roots(&mut self, tree: &UseTree) {
        let ident = match tree {
            UseTree::Path(path) => &path.ident,
            UseTree::Name(name) => &name.ident,
            UseTree::Rename(rename) => &rename.ident,
            UseTree::Glob(_) => return,
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.roots(tree);
                }
                return;
            }
        };
        if let Some(range) = self.text.range(ident.span()) {
            self.idents.entry(bare(ident)).or_default().push(range);
        }
    }

    fn unused(
        &self,
        tree: &UseTree,
        prefix: &mut Vec<String>,
        edits: &[Edit],
        removed: &mut BTreeSet<usize>,
    ) {
        let (ident, name) = match tree {
            UseTree::Path(path) => {
                prefix.push(bare(&path.ident));
                self.unused(&path.tree, prefix, edits, removed);
                prefix.pop();
                return;
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.unused(tree, prefix, edits, removed);
                }
                return;
            }
            UseTree::Name(name) => (&name.ident, &name.ident),
            UseTree::Rename(rename) => (&rename.ident, &rename.rename),
            UseTree::Glob(_) => return,
        };
        let mut path = prefix.clone();
        let name = if ident == "self" {
            match (tree, prefix.last()) {
                (UseTree::Name(_), Some(module)) => module.clone(),
                _ => bare(name),
            }
        } else {
            path.push(bare(ident));
            bare(name)
        };
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        if !self.paths.contains(&path.as_slice()) {
            return;
        }
        let Some(leaf) = self.text.of(tree) else {
            return;
        };
        let elsewhere = self.idents.get(&name).into_iter().flatten().any(|range| {
            !(leaf.start <= range.start && range.end <= leaf.end)
                && !edits
                    .iter()
                    .any(|edit| edit.start <= range.start && range.end <= edit.end)
        }) || edits
            .iter()
            .any(|edit| names_in(&edit.replacement, &name) != Some(0));
        if !elsewhere {
            removed.insert(leaf.start);
        }
    }

    fn rewrite(&self, tree: &UseTree, removed: &BTreeSet<usize>) -> Pruned {
        match tree {
            UseTree::Path(path) => match self.rewrite(&path.tree, removed) {
                Pruned::Rewritten(rest) => Pruned::Rewritten(format!("{}::{rest}", path.ident)),
                whole => whole,
            },
            // A leaf whose text can't be found counts as removed.
            UseTree::Name(_) | UseTree::Rename(_) => match self.text.of(tree) {
                Some(leaf) if !removed.contains(&leaf.start) => Pruned::Unchanged,
                _ => Pruned::Removed,
            },
            UseTree::Glob(_) => Pruned::Unchanged,
            UseTree::Group(group) => {
                let mut kept = Vec::new();
                let mut changed = false;
                for tree in &group.items {
                    match self.rewrite(tree, removed) {
                        Pruned::Removed => changed = true,
                        Pruned::Unchanged => match self.text.slice(tree) {
                            Some(text) => kept.push(text.to_owned()),
                            None => return Pruned::Removed,
                        },
                        Pruned::Rewritten(text) => {
                            changed = true;
                            kept.push(text);
                        }
                    }
                }
                match kept.as_slice() {
                    _ if !changed => Pruned::Unchanged,
                    [] => Pruned::Removed,
                    [only] if !only.starts_with("self") => Pruned::Rewritten(only.clone()),
                    kept => Pruned::Rewritten(format!("{{{}}}", kept.join(", "))),
                }
            }
        }
    }

    fn line(&self, item: &ItemUse) -> Option<Range<usize>> {
        let range = self.text.of(item)?;
        let source = self.text.source;
        let line_start = source.get(..range.start)?.rfind('\n').map_or(0, |n| n + 1);
        let start = if source.get(line_start..range.start)?.trim().is_empty() {
            line_start
        } else {
            range.start
        };
        if start != line_start {
            return Some(range);
        }
        Some(start..range.end + line_break(source.get(range.end..)?))
    }

    fn without_blank_line(&self, lines: Range<usize>) -> Range<usize> {
        let source = self.text.source;
        let whole = lines.start == 0
            || source
                .get(..lines.start)
                .is_some_and(|above| above.ends_with('\n'));
        let above = source.get(..lines.start).unwrap_or_default();
        let blank_above =
            above.len() <= self.text.prefix || above.ends_with("\n\n") || above.ends_with("\n\r\n");
        let blank_below = source.get(lines.end..).map_or(0, line_break);
        if whole && blank_above && blank_below > 0 {
            lines.start..lines.end + blank_below
        } else {
            lines
        }
    }
}

fn line_break(text: &str) -> usize {
    if text.starts_with("\r\n") {
        2
    } else {
        usize::from(text.starts_with('\n'))
    }
}

struct Uses<'ast>(Vec<&'ast ItemUse>);

impl<'ast> Visit<'ast> for Uses<'ast> {
    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        self.0.push(item);
    }
}

pub(super) fn known_macro(name: &str) -> Option<usize> {
    match name {
        "format" | "print" | "println" | "eprint" | "eprintln" | "panic" => Some(0),
        "assert" | "debug_assert" | "write" | "writeln" => Some(1),
        "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" => Some(2),
        _ => None,
    }
}

pub(super) fn item_attributes(item: &Item) -> &[syn::Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

pub(super) fn pattern_names(pat: &Pat, names: &mut Vec<String>) {
    match pat {
        Pat::Ident(ident) => {
            names.push(bare(&ident.ident));
            if let Some((_, subpat)) = &ident.subpat {
                pattern_names(subpat, names);
            }
        }
        Pat::Guard(guard) => pattern_names(&guard.pat, names),
        Pat::Or(or) => {
            if let Some(first) = or.cases.first() {
                pattern_names(first, names);
            }
        }
        Pat::Paren(paren) => pattern_names(&paren.pat, names),
        Pat::Reference(reference) => pattern_names(&reference.pat, names),
        Pat::Slice(slice) => slice.elems.iter().for_each(|pat| pattern_names(pat, names)),
        Pat::Struct(fields) => fields
            .fields
            .iter()
            .for_each(|field| pattern_names(&field.pat, names)),
        Pat::Tuple(tuple) => tuple.elems.iter().for_each(|pat| pattern_names(pat, names)),
        Pat::TupleStruct(tuple) => tuple.elems.iter().for_each(|pat| pattern_names(pat, names)),
        Pat::Type(typed) => pattern_names(&typed.pat, names),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_captures_name_variables() {
        assert_eq!(
            format_captures("{a} {{b}} {c:?} {:>width$} {d:.prec$} {0}"),
            ["a", "c", "width", "d", "prec"]
        );
    }
}
