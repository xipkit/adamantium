use std::{ops::Range, path::Path};

use syn::{Expr, Lit, Local, Pat, PathArguments, visit::Visit};

use super::{
    BORROW_SEEN, CAPACITY, CHANNEL, Kind, REPLY, TYPED, UNBOUND, UNLOCATED, WATCH, WATCH_UNWATCHED,
    analysis::{Analysis, Role},
    find::Found,
};
use crate::fix::{
    Edit,
    source::{Text, bare},
};

fn halves(pat: &Pat) -> Result<[&syn::PatIdent; 2], &'static str> {
    let Pat::Tuple(tuple) = pat else {
        return Err(if matches!(pat, Pat::Type(_)) {
            TYPED
        } else {
            UNBOUND
        });
    };
    let names: Vec<&syn::PatIdent> = tuple
        .elems
        .iter()
        .filter_map(|pat| match pat {
            Pat::Ident(ident)
                if ident.by_ref.is_none() && ident.subpat.is_none() && ident.attrs.is_empty() =>
            {
                Some(ident)
            }
            _ => None,
        })
        .collect();
    match names.as_slice() {
        [sender, receiver] if tuple.elems.len() == 2 => Ok([*sender, *receiver]),
        _ => Err(UNBOUND),
    }
}

/// The edits that turn the channel `found` and its uses into their Adamantium
/// forms, or the first reason they can't be made.
pub(super) fn rewrite(
    file: &Path,
    text: &Text<'_>,
    found: &Found<'_>,
) -> Result<Vec<Edit>, &'static str> {
    let call = match found.local.init.as_ref() {
        Some(init) if init.diverge.is_none() => match &*init.expr {
            Expr::Call(call) => call,
            _ => return Err(UNBOUND),
        },
        _ => return Err(UNBOUND),
    };
    let halves = halves(&found.local.pat)?;
    let arguments = match found.kind {
        Kind::Oneshot => 0,
        Kind::Watch | Kind::Mpsc => 1,
    };
    let turbofish = turbofish(text, &call.func).ok_or(UNBOUND)?;
    if call.args.len() != arguments {
        return Err(UNBOUND);
    }
    if found.kind == Kind::Mpsc && !call.args.first().is_some_and(positive_capacity) {
        return Err(CAPACITY);
    }
    let mut analysis = Analysis {
        text,
        file,
        kind: found.kind,
        vars: Vec::new(),
        scopes: Vec::new(),
        edits: Vec::new(),
        refusal: None,
        captured: 0,
        nested: 0,
        sends: false,
        sends_nested: false,
        receiver_moved: false,
        waits: false,
        borrows: false,
    };
    let mut scope = Vec::new();
    for (role, half) in [Role::Sender, Role::Receiver].into_iter().zip(halves) {
        let var = analysis.var(role, half);
        scope.push((bare(&half.ident), Some(var)));
    }
    analysis.scopes.push(scope);
    for stmt in found.rest {
        analysis.visit_stmt(stmt);
    }
    if analysis.borrows && analysis.waits {
        analysis.refuse(BORROW_SEEN);
    }
    if found.kind == Kind::Watch
        && analysis.sends
        && (analysis.sends_nested || analysis.receiver_moved)
    {
        analysis.refuse(WATCH_UNWATCHED);
    }
    if let Some(reason) = analysis.refusal {
        return Err(reason);
    }
    let constructor = match found.kind {
        Kind::Oneshot => text
            .of(&call.func)
            .map(|range| vec![analysis.edit(range, format!("{REPLY}{turbofish}::pair"))]),
        Kind::Watch => watch_statement(text, found.local, &analysis, &turbofish, halves)
            .map(|(range, replacement)| vec![analysis.edit(range, replacement)]),
        Kind::Mpsc => text
            .of(call)
            .zip(call.args.first().and_then(|capacity| text.slice(capacity)))
            .map(|(range, capacity)| {
                vec![analysis.edit(
                    range,
                    format!("{CHANNEL}{turbofish}::bounded({capacity}).split()"),
                )]
            }),
    };
    let mut edits = constructor.ok_or(UNLOCATED)?;
    edits.append(&mut analysis.edits);
    for (index, var) in analysis.vars.iter().enumerate() {
        if found.kind == Kind::Watch && index < 2 {
            continue;
        }
        match (found.kind, var.role, &var.mutable) {
            (Kind::Oneshot | Kind::Mpsc, _, Some(mutable)) => {
                edits.push(analysis.edit(mutable.clone(), String::new()));
            }
            (Kind::Watch, Role::Receiver, None) if var.needs_mut => {
                edits.push(analysis.edit(var.ident.start..var.ident.start, "mut ".to_owned()));
            }
            _ => {}
        }
    }
    Ok(edits)
}

fn positive_capacity(capacity: &Expr) -> bool {
    let Expr::Lit(literal) = capacity else {
        return false;
    };
    let Lit::Int(int) = &literal.lit else {
        return false;
    };
    literal.attrs.is_empty()
        && matches!(int.suffix(), "" | "usize")
        && int
            .base10_parse::<u32>()
            .is_ok_and(|capacity| (1..=MAX_CAPACITY).contains(&capacity))
}

const MAX_CAPACITY: u32 = u32::MAX >> 3;

fn turbofish(text: &Text<'_>, func: &Expr) -> Option<String> {
    let Expr::Path(path) = func else {
        return None;
    };
    if path.qself.is_some() || !path.attrs.is_empty() {
        return None;
    }
    let (last, init) = path
        .path
        .segments
        .iter()
        .collect::<Vec<_>>()
        .split_last()
        .map(|(last, init)| {
            (
                *last,
                init.iter().all(|segment| segment.arguments.is_none()),
            )
        })?;
    if !init {
        return None;
    }
    match &last.arguments {
        PathArguments::None => Some(String::new()),
        PathArguments::AngleBracketed(arguments) if arguments.colon2_token.is_some() => {
            let range = text.of(arguments)?;
            text.get(range).map(str::to_owned)
        }
        _ => None,
    }
}

fn watch_statement(
    text: &Text<'_>,
    local: &Local,
    analysis: &Analysis<'_>,
    turbofish: &str,
    halves: [&syn::PatIdent; 2],
) -> Option<(Range<usize>, String)> {
    let range = text.of(local)?;
    let Some(Expr::Call(call)) = local.init.as_ref().map(|init| &*init.expr) else {
        return None;
    };
    let value = text.slice(call.args.first()?)?;
    let source = text.source;
    let line_start = source.get(..range.start)?.rfind('\n').map_or(0, |n| n + 1);
    let indent = source.get(line_start..range.start)?;
    let separator = if !indent.trim().is_empty() {
        " ".to_owned()
    } else if source.get(..line_start)?.ends_with("\r\n") {
        format!("\r\n{indent}")
    } else {
        format!("\n{indent}")
    };
    let [sender, receiver] = halves;
    let name = |ident: &syn::PatIdent| text.slice(&ident.ident);
    let sender_name = name(sender)?;
    let receiver_name = name(receiver)?;
    let sender_mut = if sender.mutability.is_some() {
        "mut "
    } else {
        ""
    };
    let receiver_mut =
        if receiver.mutability.is_some() || analysis.vars.get(1).is_some_and(|var| var.needs_mut) {
            "mut "
        } else {
            ""
        };
    Some((
        range,
        format!(
            "let {sender_mut}{sender_name} = {WATCH}{turbofish}::new({value});{separator}let {receiver_mut}{receiver_name} = {sender_name}.subscribe();"
        ),
    ))
}
