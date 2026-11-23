use std::{mem, slice};

use proc_macro2::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};
use syn::{
    AttrStyle, Attribute, File, Item, ItemMod, Macro, MacroDelimiter, Meta, MetaList, Token,
    braced,
    parse::ParseStream,
    punctuated::Punctuated,
    token,
    visit_mut::{self, VisitMut},
};

use super::attrs::prepend_attrs;

pub(super) struct Expand;

impl Expand {
    pub(super) fn fragment(&mut self, items: &mut Vec<Item>) {
        self.items(items);
        for item in items {
            self.visit_item_mut(item);
        }
    }

    fn items(&mut self, items: &mut Vec<Item>) {
        let mut expanded = Vec::with_capacity(items.len());
        for item in mem::take(items) {
            let Item::Macro(item) = item else {
                expanded.push(item);
                continue;
            };
            let input = match item.ident {
                Some(_) => None,
                None => input_items(&item.mac),
            };
            match input {
                Some(mut inner) => {
                    self.items(&mut inner);
                    let outer = item.attrs.clone();
                    expanded.push(Item::Macro(item));
                    for mut inner in inner {
                        prepend_attrs(&mut inner, &outer);
                        expanded.push(inner);
                    }
                }
                None => expanded.push(Item::Macro(item)),
            }
        }
        *items = expanded;
    }
}

fn input_items(mac: &Macro) -> Option<Vec<Item>> {
    if is_include(mac) || mac.tokens.is_empty() {
        return None;
    }
    let parsed = if is_cfg_if(mac) {
        mac.parse_body_with(parse_cfg_if)
    } else if is_cfg_select(mac) {
        mac.parse_body_with(parse_cfg_select)
    } else {
        mac.parse_body_with(parse_items)
    };
    parsed.ok()
}

pub(super) fn reads_input(mac: &Macro) -> bool {
    input_items(mac).is_some()
}

impl VisitMut for Expand {
    fn visit_file_mut(&mut self, file: &mut File) {
        self.items(&mut file.items);
        visit_mut::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, item: &mut ItemMod) {
        if let Some((_, items)) = &mut item.content {
            self.items(items);
        }
        visit_mut::visit_item_mod_mut(self, item);
    }
}

pub(super) fn conditional(attrs: &[Attribute]) -> Vec<(Vec<TokenStream>, Meta)> {
    let mut metas = Vec::new();
    for attr in attrs {
        flatten(&mut Vec::new(), attr.meta.clone(), &mut metas);
    }
    metas
}

pub(super) fn flatten(
    conditions: &mut Vec<TokenStream>,
    meta: Meta,
    metas: &mut Vec<(Vec<TokenStream>, Meta)>,
) {
    let Meta::List(list) = &meta else {
        metas.push((conditions.clone(), meta));
        return;
    };
    if !list.path.is_ident("cfg_attr") {
        metas.push((conditions.clone(), meta));
        return;
    }
    let parsed = list.parse_args_with(|input: ParseStream| {
        let mut condition = TokenStream::new();
        while !input.peek(Token![,]) {
            condition.extend([input.parse::<TokenTree>()?]);
        }
        input.parse::<Token![,]>()?;
        let inner = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        Ok((condition, inner))
    });
    let Ok((condition, inner)) = parsed else {
        return;
    };
    conditions.push(condition);
    for meta in inner {
        flatten(conditions, meta, metas);
    }
    conditions.pop();
}

pub(super) fn cfg_predicates(attrs: &[Attribute]) -> Vec<TokenStream> {
    let span = Span::call_site();
    conditional(attrs)
        .into_iter()
        .filter_map(|(conditions, meta)| {
            let Meta::List(list) = meta else {
                return None;
            };
            if !list.path.is_ident("cfg") {
                return None;
            }
            if conditions.is_empty() {
                return Some(list.tokens);
            }
            let unless = cfg_call("not", [cfg_call("all", conditions, span)], span);
            Some(cfg_call("any", [unless, list.tokens], span))
        })
        .collect()
}

pub(super) fn parse_items(input: ParseStream) -> syn::Result<Vec<Item>> {
    let mut items = Vec::new();
    while !input.is_empty() {
        items.push(input.parse()?);
    }
    Ok(items)
}

fn parse_cfg_if(input: ParseStream) -> syn::Result<Vec<Item>> {
    let mut earlier = Vec::new();
    let mut items = Vec::new();
    let mut otherwise = None;
    loop {
        let (span, predicate) = match otherwise {
            None => {
                input.parse::<Token![if]>()?;
                let (span, condition) = cfg_if_condition(input)?;
                let none_earlier = cfg_none(earlier.clone(), span);
                earlier.push(condition.clone());
                (span, cfg_call("all", [condition, none_earlier], span))
            }
            Some(span) => (span, cfg_none(earlier.clone(), span)),
        };
        let body;
        braced!(body in input);
        let cfg = cfg_attribute(predicate, span);
        for mut item in parse_items(&body)? {
            prepend_attrs(&mut item, slice::from_ref(&cfg));
            items.push(item);
        }
        if otherwise.is_some() || input.is_empty() {
            return Ok(items);
        }
        let span = input.parse::<Token![else]>()?.span;
        if !input.peek(Token![if]) {
            otherwise = Some(span);
        }
    }
}

fn cfg_if_condition(input: ParseStream) -> syn::Result<(Span, TokenStream)> {
    let attrs = input.call(Attribute::parse_outer)?;
    let [attr] = &attrs[..] else {
        return Err(input.error("expected one #[cfg(...)]"));
    };
    match &attr.meta {
        Meta::List(list) if list.path.is_ident("cfg") && !list.tokens.is_empty() => {
            let span = attr.pound_token.span;
            Ok((span, list.tokens.clone()))
        }
        _ => Err(input.error("expected #[cfg(...)]")),
    }
}

fn parse_cfg_select(input: ParseStream) -> syn::Result<Vec<Item>> {
    let mut earlier = Vec::new();
    let mut items = Vec::new();
    loop {
        let span = input.span();
        let otherwise = input.peek(Token![_]);
        let none_earlier = cfg_none(earlier.clone(), span);
        let predicate = if otherwise {
            input.parse::<Token![_]>()?;
            none_earlier
        } else {
            let mut condition = TokenStream::new();
            while !input.peek(Token![=>]) {
                condition.extend([input.parse::<TokenTree>()?]);
            }
            if condition.is_empty() {
                return Err(input.error("expected a cfg predicate"));
            }
            earlier.push(condition.clone());
            cfg_call("all", [condition, none_earlier], span)
        };
        input.parse::<Token![=>]>()?;
        let body;
        braced!(body in input);
        let cfg = cfg_attribute(predicate, span);
        for mut item in parse_items(&body)? {
            prepend_attrs(&mut item, slice::from_ref(&cfg));
            items.push(item);
        }
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
        if otherwise || input.is_empty() {
            return Ok(items);
        }
    }
}

pub(super) fn cfg_call(
    name: &str,
    args: impl IntoIterator<Item = TokenStream>,
    span: Span,
) -> TokenStream {
    let mut list = TokenStream::new();
    for arg in args {
        if !list.is_empty() {
            list.extend([TokenTree::Punct(Punct::new(',', Spacing::Alone))]);
        }
        list.extend(arg);
    }
    let mut group = Group::new(Delimiter::Parenthesis, list);
    group.set_span(span);
    TokenStream::from_iter([
        TokenTree::Ident(Ident::new(name, span)),
        TokenTree::Group(group),
    ])
}

/// The predicate that holds when none of `conditions` does.
pub(super) fn cfg_none(conditions: Vec<TokenStream>, span: Span) -> TokenStream {
    cfg_call("not", [cfg_call("any", conditions, span)], span)
}

pub(super) fn cfg_attribute(predicate: TokenStream, span: Span) -> Attribute {
    Attribute {
        pound_token: Token![#](span),
        style: AttrStyle::Outer,
        bracket_token: token::Bracket(span),
        meta: Meta::List(MetaList {
            path: Ident::new("cfg", span).into(),
            delimiter: MacroDelimiter::Paren(token::Paren(span)),
            tokens: predicate,
        }),
    }
}

fn is_cfg_if(mac: &Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "cfg_if")
}

fn is_cfg_select(mac: &Macro) -> bool {
    let mut names = mac.path.segments.iter().map(|segment| &segment.ident);
    match (names.next(), names.next(), names.next()) {
        (Some(name), None, None) => name == "cfg_select",
        (Some(root), Some(name), None) => (root == "std" || root == "core") && name == "cfg_select",
        _ => false,
    }
}

pub(super) fn is_include(mac: &Macro) -> bool {
    mac.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "include")
}
