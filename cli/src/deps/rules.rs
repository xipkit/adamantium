//! Expanding a call of a `macro_rules!` without compiling the crate: the
//! call's input is matched against the macro's rules in order, and the
//! first rule that matches is transcribed with what its metavariables bound.
//!
//! rustc parses fragments with its own parser; this parses them with syn,
//! so it matches only the kinds whose syn parsers take the tokens rustc's
//! do: `tt`, `ident`, `lifetime`, `literal`, `vis`, `meta`, `path`, `ty`,
//! `block` and `item`. A call that reaches any other kind, a fragment syn
//! can't parse, more than one way to match a rule, or no rule that matches
//! is one this can't decide, so it isn't expanded. A crate that builds has
//! none of the last three, so they mean this doesn't read the rule as rustc
//! does.

use std::collections::HashMap;

use proc_macro2::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};
use syn::{
    Block, Item, Meta, Path, Token, Type, Visibility,
    parse::{Parse, ParseStream, Parser},
};

/// The rules of a `macro_rules!`.
#[derive(Debug)]
pub struct Rules(Vec<Rule>);

#[derive(Debug)]
struct Rule {
    matcher: Vec<Matcher>,
    transcriber: Vec<Transcribed>,
}

/// A part of a rule's matcher.
#[derive(Debug)]
enum Matcher {
    /// A token the input must have there.
    Token(Token),
    Group(Delimiter, Vec<Matcher>),
    /// `$name:kind`.
    Fragment {
        name: String,
        kind: Kind,
    },
    Repetition(Repetition<Matcher>),
}

/// A part of a rule's transcriber.
#[derive(Debug)]
enum Transcribed {
    /// A token other than a group, copied as it is.
    Token(TokenTree),
    Group(Group, Vec<Transcribed>),
    /// `$name`, which is copied as it is if no metavariable has the name.
    Variable {
        dollar: TokenTree,
        name: Ident,
    },
    /// `$crate`. The macro is the crate's own, so it is `crate`.
    Crate(Span),
    Repetition(Repetition<Transcribed>),
}

/// `$( ... ) separator? operator`.
#[derive(Debug)]
struct Repetition<T> {
    body: Vec<T>,
    separator: Option<Vec<TokenTree>>,
    operator: Operator,
    /// The metavariables the body declares or uses, at any depth.
    names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    /// `*`
    Any,
    /// `+`
    AtLeastOne,
    /// `?`
    AtMostOne,
}

/// A fragment specifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Tt,
    Ident,
    Lifetime,
    Literal,
    Vis,
    Meta,
    Path,
    Ty,
    Block,
    Item,
    /// A kind rustc knows whose syn parser takes other tokens than rustc's,
    /// such as `expr`, whose tokens differ by edition.
    Unmatched,
}

impl Kind {
    fn new(name: &str) -> Option<Self> {
        Some(match name {
            "tt" => Self::Tt,
            "ident" => Self::Ident,
            "lifetime" => Self::Lifetime,
            "literal" => Self::Literal,
            "vis" => Self::Vis,
            "meta" => Self::Meta,
            "path" => Self::Path,
            "ty" => Self::Ty,
            "block" => Self::Block,
            "item" => Self::Item,
            "expr" | "expr_2021" | "stmt" | "pat" | "pat_param" => Self::Unmatched,
            _ => return None,
        })
    }

    /// Whether rustc passes a fragment of this kind on as its tokens. It
    /// passes any other kind on as one opaque token, which a later matcher
    /// can't look into, as tokens in an invisible group.
    fn is_transparent(self) -> bool {
        matches!(self, Self::Tt | Self::Ident | Self::Lifetime)
    }
}

/// A token as rustc sees it: an identifier, a literal, a lifetime or an
/// operator, which may be more than one [`proc_macro2::Punct`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Literal(String),
    Lifetime(String),
    Punct(String),
}

/// The operators rustc lexes as one token, which proc_macro2 gives as joint
/// [`proc_macro2::Punct`]s.
const OPERATORS: [&str; 25] = [
    "<<=", ">>=", "...", "..=", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=", "%=",
    "^=", "&=", "|=", "<<", ">>", "..", "::", "->", "=>", "<-",
];

/// The token at `at`, which is not a group, and how many trees it takes.
fn token(trees: &[TokenTree], at: usize) -> Option<(Token, usize)> {
    match trees.get(at)? {
        TokenTree::Group(_) => None,
        TokenTree::Ident(ident) => Some((Token::Ident(ident.to_string()), 1)),
        TokenTree::Literal(literal) => Some((Token::Literal(literal.to_string()), 1)),
        TokenTree::Punct(punct) => {
            if punct.as_char() == '\''
                && punct.spacing() == Spacing::Joint
                && let Some(TokenTree::Ident(name)) = trees.get(at + 1)
            {
                return Some((Token::Lifetime(name.to_string()), 2));
            }
            let mut joined = String::new();
            for tree in trees.iter().skip(at).take(3) {
                let TokenTree::Punct(punct) = tree else {
                    break;
                };
                joined.push(punct.as_char());
                if punct.spacing() == Spacing::Alone {
                    break;
                }
            }
            while joined.len() > 1 && !OPERATORS.contains(&joined.as_str()) {
                joined.pop();
            }
            let len = joined.len();
            Some((Token::Punct(joined), len))
        }
    }
}

/// How many trees the token or group at `at` takes, as one `tt`.
fn tree_len(trees: &[TokenTree], at: usize) -> usize {
    token(trees, at).map_or(1, |(_, len)| len)
}

fn is_punct(tree: Option<&TokenTree>, ch: char) -> bool {
    matches!(tree, Some(TokenTree::Punct(punct)) if punct.as_char() == ch)
}

impl Rules {
    /// Reads the body of a `macro_rules!`: its rules, separated by `;`.
    /// `None` if it has a form this doesn't read, or that rustc rejects.
    pub fn parse(body: TokenStream) -> Option<Self> {
        let pairs = parse_rules.parse2(body).ok()?;
        if pairs.is_empty() {
            return None;
        }
        let rules = pairs
            .into_iter()
            .map(|(matcher, transcriber)| {
                Some(Rule {
                    matcher: parse_matcher(&trees(matcher.stream()))?,
                    transcriber: parse_transcriber(&trees(transcriber.stream()))?,
                })
            })
            .collect::<Option<_>>()?;
        Some(Self(rules))
    }

    /// The tokens a call with `input` expands to, or `None` if this can't
    /// decide what they are.
    pub fn expand(&self, input: TokenStream) -> Option<TokenStream> {
        let input = trees(input);
        let mut matching = Matching {
            steps: STEPS,
            ..Matching::default()
        };
        for rule in &self.0 {
            let start = State {
                at: 0,
                bound: Bindings::new(),
            };
            let mut ends = matching.all(&rule.matcher, &input, vec![start]).ok()?;
            ends.retain(|state| state.at == input.len());
            match &ends[..] {
                [] => continue,
                [end] => {
                    let mut out = Vec::new();
                    matching.transcribe(&rule.transcriber, &end.bound, &mut out)?;
                    return Some(out.into_iter().collect());
                }
                _ => return None,
            }
        }
        None
    }
}

fn trees(stream: TokenStream) -> Vec<TokenTree> {
    stream.into_iter().collect()
}

fn parse_rules(input: ParseStream) -> syn::Result<Vec<(Group, Group)>> {
    let mut rules = Vec::new();
    while !input.is_empty() {
        let matcher: Group = input.parse()?;
        input.parse::<Token![=>]>()?;
        let transcriber: Group = input.parse()?;
        rules.push((matcher, transcriber));
        if !input.is_empty() {
            input.parse::<Token![;]>()?;
        }
    }
    Ok(rules)
}

fn parse_matcher(trees: &[TokenTree]) -> Option<Vec<Matcher>> {
    let mut matcher = Vec::new();
    let mut at = 0;
    while at < trees.len() {
        let tree = trees.get(at)?;
        if is_punct(Some(tree), '$') {
            match trees.get(at + 1)? {
                TokenTree::Ident(name) if name != "crate" => {
                    let (true, Some(TokenTree::Ident(kind))) =
                        (is_punct(trees.get(at + 2), ':'), trees.get(at + 3))
                    else {
                        return None;
                    };
                    matcher.push(Matcher::Fragment {
                        name: name.to_string(),
                        kind: Kind::new(&kind.to_string())?,
                    });
                    at += 4;
                }
                TokenTree::Group(group) if group.delimiter() == Delimiter::Parenthesis => {
                    let body = parse_matcher(&self::trees(group.stream()))?;
                    let (separator, operator, len) = parse_operator(trees, at + 2)?;
                    let mut names = Vec::new();
                    declared(&body, &mut names);
                    matcher.push(Matcher::Repetition(Repetition {
                        body,
                        separator,
                        operator,
                        names,
                    }));
                    at += 2 + len;
                }
                _ => return None,
            }
            continue;
        }
        match tree {
            // An invisible group is a fragment another macro passed on.
            TokenTree::Group(group) if group.delimiter() == Delimiter::None => return None,
            TokenTree::Group(group) => {
                let inner = parse_matcher(&self::trees(group.stream()))?;
                matcher.push(Matcher::Group(group.delimiter(), inner));
                at += 1;
            }
            _ => {
                let (token, len) = token(trees, at)?;
                matcher.push(Matcher::Token(token));
                at += len;
            }
        }
    }
    Some(matcher)
}

/// Reads the separator and operator after a repetition's group, at `at`,
/// and how many trees they take.
fn parse_operator(
    trees: &[TokenTree],
    at: usize,
) -> Option<(Option<Vec<TokenTree>>, Operator, usize)> {
    let operator = |at| match token(trees, at)? {
        (Token::Punct(op), 1) if op == "*" => Some(Operator::Any),
        (Token::Punct(op), 1) if op == "+" => Some(Operator::AtLeastOne),
        (Token::Punct(op), 1) if op == "?" => Some(Operator::AtMostOne),
        _ => None,
    };
    if let Some(operator) = operator(at) {
        return Some((None, operator, 1));
    }
    let (_, len) = token(trees, at)?;
    let separator = trees.get(at..at + len)?.to_vec();
    match operator(at + len)? {
        Operator::AtMostOne => None,
        operator => Some((Some(separator), operator, len + 1)),
    }
}

fn declared(matcher: &[Matcher], names: &mut Vec<String>) {
    for part in matcher {
        match part {
            Matcher::Token(_) => {}
            Matcher::Group(_, inner) => declared(inner, names),
            Matcher::Fragment { name, .. } => names.push(name.clone()),
            Matcher::Repetition(repetition) => names.extend(repetition.names.iter().cloned()),
        }
    }
}

fn parse_transcriber(trees: &[TokenTree]) -> Option<Vec<Transcribed>> {
    let mut transcriber = Vec::new();
    let mut at = 0;
    while let Some(tree) = trees.get(at) {
        if is_punct(Some(tree), '$') {
            match trees.get(at + 1)? {
                TokenTree::Ident(name) if name == "crate" => {
                    transcriber.push(Transcribed::Crate(name.span()));
                    at += 2;
                }
                TokenTree::Ident(name) => {
                    transcriber.push(Transcribed::Variable {
                        dollar: tree.clone(),
                        name: name.clone(),
                    });
                    at += 2;
                }
                TokenTree::Group(group) if group.delimiter() == Delimiter::Parenthesis => {
                    let body = parse_transcriber(&self::trees(group.stream()))?;
                    let (mut separator, operator, len) = parse_operator(trees, at + 2)?;
                    if let Some(last) = separator
                        .as_mut()
                        .and_then(|separator| separator.last_mut())
                    {
                        *last = alone(last);
                    }
                    let mut names = Vec::new();
                    used(&body, &mut names);
                    transcriber.push(Transcribed::Repetition(Repetition {
                        body,
                        separator,
                        operator,
                        names,
                    }));
                    at += 2 + len;
                }
                // `${...}` and `$$` are unstable metavariable expressions.
                _ => return None,
            }
            continue;
        }
        match tree {
            TokenTree::Group(group) => {
                let inner = parse_transcriber(&self::trees(group.stream()))?;
                transcriber.push(Transcribed::Group(group.clone(), inner));
            }
            // rustc doesn't join an operator to what a metavariable after it
            // stands for, which proc_macro2 would, as its `$` is a `Punct`.
            tree if is_punct(trees.get(at + 1), '$') => {
                transcriber.push(Transcribed::Token(alone(tree)));
            }
            tree => transcriber.push(Transcribed::Token(tree.clone())),
        }
        at += 1;
    }
    Some(transcriber)
}

/// A copy of the tree that a `Punct` after it isn't joined to.
fn alone(tree: &TokenTree) -> TokenTree {
    match tree {
        TokenTree::Punct(punct) => {
            let mut copy = Punct::new(punct.as_char(), Spacing::Alone);
            copy.set_span(punct.span());
            TokenTree::Punct(copy)
        }
        tree => tree.clone(),
    }
}

fn used(transcriber: &[Transcribed], names: &mut Vec<String>) {
    for part in transcriber {
        match part {
            Transcribed::Token(_) | Transcribed::Crate(_) => {}
            Transcribed::Group(_, inner) => used(inner, names),
            Transcribed::Variable { name, .. } => names.push(name.to_string()),
            Transcribed::Repetition(repetition) => names.extend(repetition.names.iter().cloned()),
        }
    }
}

/// What metavariables bound, by name.
type Bindings = HashMap<String, Bound>;

/// What a metavariable bound, in [`Matching`].
#[derive(Debug, Clone, Copy)]
enum Bound {
    /// The index of a fragment.
    Fragment(usize),
    /// What the repetition it is declared in bound in each of its passes:
    /// the index of its last pass, `None` if it made none.
    Repeated(Option<usize>),
}

/// A pass a repetition made: what it bound, and the pass before it, which
/// the ways to make one more pass share.
#[derive(Debug)]
struct Pass {
    bound: Bindings,
    before: Option<usize>,
    count: usize,
}

/// One way to have matched part of a rule: where in the input it is, and
/// what it bound so far.
#[derive(Debug, Clone)]
struct State {
    at: usize,
    bound: Bindings,
}

/// The input can't be matched as rustc would match it.
#[derive(Debug)]
struct Undecided;

/// How many steps matching one call may take, so that a rule whose
/// repetitions can each match in many ways can't keep it going.
const STEPS: usize = 100_000;

/// The fragments and passes matching one call bound, which [`Bound`]s
/// refer to by index, and the steps it may still take.
#[derive(Debug, Default)]
struct Matching {
    fragments: Vec<(Kind, Vec<TokenTree>)>,
    passes: Vec<Pass>,
    steps: usize,
}

impl Matching {
    fn spend(&mut self) -> Result<(), Undecided> {
        self.steps = self.steps.checked_sub(1).ok_or(Undecided)?;
        Ok(())
    }

    /// Every state `matcher` can take each of `states` to in `input`, as
    /// rustc follows every way to match a rule at once.
    fn all(
        &mut self,
        matcher: &[Matcher],
        input: &[TokenTree],
        mut states: Vec<State>,
    ) -> Result<Vec<State>, Undecided> {
        for part in matcher {
            let mut next = Vec::new();
            for state in states {
                self.spend()?;
                next.extend(self.part(part, input, state)?);
            }
            states = next;
        }
        Ok(states)
    }

    fn part(
        &mut self,
        part: &Matcher,
        input: &[TokenTree],
        mut state: State,
    ) -> Result<Vec<State>, Undecided> {
        let at = state.at;
        Ok(match part {
            Matcher::Token(expected) => match token(input, at) {
                Some((found, len)) if found == *expected => {
                    state.at += len;
                    vec![state]
                }
                _ => Vec::new(),
            },
            Matcher::Group(delimiter, inner) => match input.get(at) {
                Some(TokenTree::Group(group)) if group.delimiter() == *delimiter => {
                    let content = trees(group.stream());
                    state.at = 0;
                    self.all(inner, &content, vec![state])?
                        .into_iter()
                        .filter(|end| end.at == content.len())
                        .map(|end| State {
                            at: at + 1,
                            bound: end.bound,
                        })
                        .collect()
                }
                _ => Vec::new(),
            },
            Matcher::Fragment { name, kind } => match fragment(*kind, input, at)? {
                Some(len) => {
                    let tokens = input.get(at..at + len).ok_or(Undecided)?.to_vec();
                    self.fragments.push((*kind, tokens));
                    let bound = Bound::Fragment(self.fragments.len() - 1);
                    state.bound.insert(name.clone(), bound);
                    state.at += len;
                    vec![state]
                }
                None => Vec::new(),
            },
            Matcher::Repetition(repetition) => self.repeat(repetition, input, state)?,
        })
    }

    /// The states after each number of passes a repetition can make from
    /// `state`, with what its metavariables bound in each pass.
    fn repeat(
        &mut self,
        repetition: &Repetition<Matcher>,
        input: &[TokenTree],
        state: State,
    ) -> Result<Vec<State>, Undecided> {
        let mut pending = vec![(state.at, None)];
        let mut ends = Vec::new();
        while let Some((at, last)) = pending.pop() {
            self.spend()?;
            let made = self.count(last);
            if made >= usize::from(repetition.operator == Operator::AtLeastOne) {
                let mut end = state.clone();
                end.at = at;
                for name in &repetition.names {
                    end.bound.insert(name.clone(), Bound::Repeated(last));
                }
                ends.push(end);
            }
            if repetition.operator == Operator::AtMostOne && made > 0 {
                continue;
            }
            let mut start = at;
            if let (Some(separator), true) = (&repetition.separator, made > 0) {
                let expected = token(separator, 0).map(|(token, _)| token);
                match token(input, at) {
                    Some((found, len)) if Some(&found) == expected.as_ref() => start += len,
                    _ => continue,
                }
            }
            // A pass starts with no bindings of its own.
            let pass = State {
                at: start,
                bound: Bindings::new(),
            };
            for end in self.all(&repetition.body, input, vec![pass])? {
                // rustc rejects a repetition that can match nothing.
                if end.at == at {
                    return Err(Undecided);
                }
                self.passes.push(Pass {
                    bound: end.bound,
                    before: last,
                    count: made + 1,
                });
                pending.push((end.at, Some(self.passes.len() - 1)));
            }
        }
        Ok(ends)
    }

    fn count(&self, last: Option<usize>) -> usize {
        last.and_then(|last| self.passes.get(last))
            .map_or(0, |pass| pass.count)
    }

    /// What each pass up to `last` bound, from the first.
    fn each_pass(&self, mut last: Option<usize>) -> Option<Vec<&Bindings>> {
        let mut each = Vec::new();
        while let Some(index) = last {
            let pass = self.passes.get(index)?;
            each.push(&pass.bound);
            last = pass.before;
        }
        each.reverse();
        Some(each)
    }

    /// Writes `transcriber` to `out` with what the metavariables bound in
    /// the passes of the repetitions around it.
    fn transcribe(
        &self,
        transcriber: &[Transcribed],
        bound: &Bindings,
        out: &mut Vec<TokenTree>,
    ) -> Option<()> {
        for part in transcriber {
            match part {
                Transcribed::Token(tree) => out.push(tree.clone()),
                Transcribed::Group(group, inner) => {
                    let mut content = Vec::new();
                    self.transcribe(inner, bound, &mut content)?;
                    let mut copy = Group::new(group.delimiter(), content.into_iter().collect());
                    copy.set_span(group.span());
                    out.push(TokenTree::Group(copy));
                }
                Transcribed::Crate(span) => out.push(TokenTree::Ident(Ident::new("crate", *span))),
                Transcribed::Variable { dollar, name } => match bound.get(&name.to_string()) {
                    None => out.extend([dollar.clone(), TokenTree::Ident(name.clone())]),
                    Some(Bound::Fragment(index)) => {
                        let (kind, tokens) = self.fragments.get(*index)?;
                        if kind.is_transparent() {
                            out.extend(tokens.iter().cloned());
                        } else {
                            let stream = tokens.iter().cloned().collect();
                            out.push(TokenTree::Group(Group::new(Delimiter::None, stream)));
                        }
                    }
                    // rustc rejects a metavariable used at less depth than it
                    // repeats at.
                    Some(Bound::Repeated(_)) => return None,
                },
                Transcribed::Repetition(repetition) => {
                    self.transcribe_repetition(repetition, bound, out)?;
                }
            }
        }
        Some(())
    }

    /// Writes a repetition once for each pass the repetitions in the matcher
    /// that declare its metavariables made. Those must have made as many,
    /// and a metavariable declared at less depth is the same in each pass.
    fn transcribe_repetition(
        &self,
        repetition: &Repetition<Transcribed>,
        bound: &Bindings,
        out: &mut Vec<TokenTree>,
    ) -> Option<()> {
        let mut repeated = Vec::new();
        for name in &repetition.names {
            if let Some(Bound::Repeated(last)) = bound.get(name) {
                repeated.push((name, self.each_pass(*last)?));
            }
        }
        // rustc rejects a repetition with no metavariable that repeats at
        // its depth.
        let passes = repeated.first().map(|(_, each)| each.len())?;
        if repeated.iter().any(|(_, each)| each.len() != passes) {
            return None;
        }
        for index in 0..passes {
            if let (Some(separator), true) = (&repetition.separator, index > 0) {
                out.extend(separator.iter().cloned());
            }
            let mut pass = bound.clone();
            for (name, each) in &repeated {
                let value = *each.get(index)?.get(*name)?;
                pass.insert((*name).clone(), value);
            }
            self.transcribe(&repetition.body, &pass, out)?;
        }
        Some(())
    }
}

/// How many trees a fragment of `kind` at `at` takes: `None` if rustc
/// wouldn't try to parse one there, and undecided if it would but this
/// can't parse it as rustc does.
fn fragment(kind: Kind, input: &[TokenTree], at: usize) -> Result<Option<usize>, Undecided> {
    let Some(first) = input.get(at) else {
        return Ok(None);
    };
    let punct = match first {
        TokenTree::Punct(punct) => Some(punct.as_char()),
        _ => None,
    };
    let group = match first {
        TokenTree::Group(group) => Some(group.delimiter()),
        _ => None,
    };
    let opaque = group == Some(Delimiter::None);
    let ident = matches!(first, TokenTree::Ident(_));
    let lifetime = matches!(token(input, at), Some((Token::Lifetime(_), _)));
    let path_start =
        ident || opaque || matches!(token(input, at), Some((Token::Punct(op), _)) if op == "::");
    // The tokens rustc's `can_begin_type` takes, as far as they matter here.
    let type_start = ident
        || matches!(
            group,
            Some(Delimiter::Parenthesis | Delimiter::Bracket | Delimiter::None)
        )
        || matches!(punct, Some('!' | '*' | '&' | '?' | '<' | ':' | '\''));
    Ok(match kind {
        Kind::Tt => Some(tree_len(input, at)),
        Kind::Ident => match first {
            TokenTree::Ident(name) if name != "_" => Some(1),
            _ => None,
        },
        Kind::Lifetime => lifetime.then_some(2),
        Kind::Literal => match first {
            TokenTree::Literal(_) => Some(1),
            TokenTree::Ident(name) if name == "true" || name == "false" => Some(1),
            TokenTree::Punct(minus) if minus.as_char() == '-' => match input.get(at + 1) {
                Some(TokenTree::Literal(_)) => Some(2),
                _ => return Err(Undecided),
            },
            TokenTree::Group(_) if opaque => return Err(Undecided),
            _ => None,
        },
        Kind::Vis if type_start || punct == Some(',') => Some(parse::<Visibility>(input, at)?),
        Kind::Meta if path_start => Some(parse::<Meta>(input, at)?),
        Kind::Path if path_start => Some(parse::<Path>(input, at)?),
        Kind::Ty if type_start => Some(parse::<Type>(input, at)?),
        Kind::Block if opaque || group == Some(Delimiter::Brace) => {
            Some(parse::<Block>(input, at)?)
        }
        Kind::Item => Some(parse::<Item>(input, at)?),
        Kind::Unmatched => return Err(Undecided),
        Kind::Vis | Kind::Meta | Kind::Path | Kind::Ty | Kind::Block => None,
    })
}

/// How many trees from `at` syn parses as a `T`.
fn parse<T: Parse>(input: &[TokenTree], at: usize) -> Result<usize, Undecided> {
    let rest = input.get(at..).ok_or(Undecided)?;
    let left = (|stream: ParseStream| {
        stream.parse::<T>()?;
        stream.parse::<TokenStream>()
    })
    .parse2(rest.iter().cloned().collect())
    .map_err(|_| Undecided)?;
    let left = trees(left);
    let taken = rest.len().checked_sub(left.len()).ok_or(Undecided)?;
    // syn reads into an invisible group as if it weren't there, where rustc
    // takes it whole, so a parse may stop inside one and leave the rest of
    // it loose.
    let whole = rest
        .get(taken..)
        .is_some_and(|after| after.iter().zip(&left).all(|(a, b)| same_tree(a, b)));
    if whole { Ok(taken) } else { Err(Undecided) }
}

fn same_tree(a: &TokenTree, b: &TokenTree) -> bool {
    let place = |tree: &TokenTree| tree.span().byte_range();
    let same = match (a, b) {
        (TokenTree::Group(a), TokenTree::Group(b)) => a.delimiter() == b.delimiter(),
        (TokenTree::Ident(a), TokenTree::Ident(b)) => a == b,
        (TokenTree::Punct(a), TokenTree::Punct(b)) => a.as_char() == b.as_char(),
        (TokenTree::Literal(a), TokenTree::Literal(b)) => a.to_string() == b.to_string(),
        _ => false,
    };
    same && place(a) == place(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(body: &str) -> Rules {
        Rules::parse(body.parse().unwrap()).unwrap()
    }

    /// The expansion of `input` with the rules `body`, flattened to text.
    fn expand(body: &str, input: &str) -> Option<String> {
        rules(body)
            .expand(input.parse().unwrap())
            .map(|tokens| flatten(tokens).to_string())
    }

    /// The tokens with each invisible group replaced by its tokens.
    fn flatten(tokens: TokenStream) -> TokenStream {
        tokens
            .into_iter()
            .flat_map(|tree| match tree {
                TokenTree::Group(group) if group.delimiter() == Delimiter::None => {
                    trees(flatten(group.stream()))
                }
                TokenTree::Group(group) => {
                    let mut copy = Group::new(group.delimiter(), flatten(group.stream()));
                    copy.set_span(group.span());
                    vec![TokenTree::Group(copy)]
                }
                tree => vec![tree],
            })
            .collect()
    }

    fn text(source: &str) -> String {
        source.parse::<TokenStream>().unwrap().to_string()
    }

    /// Tokio's `feature!`, which puts a `cfg` on each item it is given.
    #[test]
    fn items_after_an_inner_attribute_are_each_given_its_cfg() {
        let feature = r"(
            #![$meta:meta]
            $($item:item)*
        ) => {
            $(
                #[cfg($meta)]
                #[cfg_attr(docsrs, doc(cfg($meta)))]
                $item
            )*
        }";
        let expanded = expand(
            feature,
            "#![unix] mod symlink; pub use self::symlink::symlink; #[cfg(x)] pub(crate) mod a { fn b() {} }",
        );
        let expected = text(
            "#[cfg(unix)] #[cfg_attr(docsrs, doc(cfg(unix)))] mod symlink;
             #[cfg(unix)] #[cfg_attr(docsrs, doc(cfg(unix)))] pub use self::symlink::symlink;
             #[cfg(unix)] #[cfg_attr(docsrs, doc(cfg(unix)))] #[cfg(x)] pub(crate) mod a { fn b() {} }",
        );
        assert_eq!(expanded, Some(expected));
    }

    /// The first rule that matches is the one transcribed, chosen here by a
    /// literal token, as psm's `psm_stack_manipulation!` chooses by `yes`.
    #[test]
    fn the_first_rule_that_matches_is_transcribed() {
        let pick = r"
            (yes { $($yes:tt)* } no { $($no:tt)* }) => { $($yes)* };
            (no { $($no:tt)* }) => { $($no)* };
            ($($other:tt)*) => { mod other; };
        ";
        assert_eq!(
            expand(pick, "yes { mod a; fn b() {} } no { mod c; }"),
            Some(text("mod a; fn b() {}"))
        );
        assert_eq!(expand(pick, "no { mod c; }"), Some(text("mod c;")));
        assert_eq!(expand(pick, "maybe"), Some(text("mod other;")));
    }

    #[test]
    fn repetitions_bind_each_pass_and_transcribe_separators() {
        let list = r"($($name:ident = $value:literal),+ $(,)?) => { $(const $name: i32 = $value;)+ [$($value),*] }";
        assert_eq!(
            expand(list, "a = 1, b = -2,"),
            Some(text("const a: i32 = 1; const b: i32 = -2; [1, -2]"))
        );
        assert_eq!(expand(list, ""), None);
        let nested =
            r"($($outer:ident [$($inner:tt)*])*) => { $($(mod $inner;)* fn $outer() {})* }";
        assert_eq!(
            expand(nested, "f [a b] g [] h [c]"),
            Some(text("mod a; mod b; fn f() {} fn g() {} mod c; fn h() {}"))
        );
    }

    /// `$crate` is the crate itself; a `$` before a name nothing bound is
    /// copied; `vis`, `ty`, `path`, `block` and `lifetime` take what rustc's
    /// parsers take.
    #[test]
    fn fragments_take_what_rustc_takes() {
        let all = r"($v:vis struct $name:ident<$l:lifetime>($t:ty); $p:path => $b:block) => {
            $v struct $name<$l>($t); fn f() -> $crate::$p $b macro_rules! m { ($x) => {} }
        }";
        assert_eq!(
            expand(
                all,
                "pub(crate) struct S<'a>(&'a Vec<u8>); a::B<C> => { 1 }"
            ),
            Some(text(
                "pub(crate) struct S< 'a>(&'a Vec<u8>); fn f() -> crate::a::B<C> { 1 } macro_rules! m { ($x) => {} }"
            ))
        );
        assert_eq!(
            expand(all, "struct S<'a>(u8); P => { 1 }"),
            Some(text(
                "struct S< 'a>(u8); fn f() -> crate::P { 1 } macro_rules! m { ($x) => {} }"
            ))
        );
    }

    /// An operator is one token, as rustc lexes it: `=>` is not `=` `>`, and
    /// a `tt` takes all of `<<=`.
    #[test]
    fn operators_are_matched_whole() {
        let arrow = r"(= >) => { mod split; }; (=>) => { mod joined; };";
        assert_eq!(expand(arrow, "=>"), Some(text("mod joined;")));
        assert_eq!(expand(arrow, "= >"), Some(text("mod split;")));
        let count = r"($a:tt $b:tt) => { mod two; }; ($a:tt) => { mod one; };";
        assert_eq!(expand(count, "<<="), Some(text("mod one;")));
        assert_eq!(expand(count, "'a x"), Some(text("mod two;")));
    }

    /// The passes of a repetition share what the passes before them bound,
    /// so a long input takes time in proportion to its length.
    #[test]
    fn long_repetitions_are_matched() {
        let input = "x ".repeat(20_000);
        let body = r"($($t:tt)*) => { mod done; }";
        assert_eq!(expand(body, &input), Some(text("mod done;")));
    }

    /// What this can't decide isn't expanded.
    #[test]
    fn what_cannot_be_decided_is_not_expanded() {
        for (body, input) in [
            // A kind syn parses otherwise than rustc.
            (r"($e:expr) => { mod a; }", "1 + 1"),
            // A fragment syn can't parse where rustc would parse one.
            (r"($i:item) => { mod a; }", "#![inner]"),
            // No rule matches.
            (r"(a) => { mod a; }", "b"),
            // Two ways to match.
            (r"($(a)? $(a)?) => { mod a; }", "a"),
            // Repetitions whose counts differ.
            (r"($($a:ident)* ; $($b:ident)*) => { $($a $b)* }", "x y ; z"),
            // A metavariable used at less depth than it repeats at.
            (r"($($a:ident)*) => { $a }", "x"),
        ] {
            assert_eq!(expand(body, input), None, "{body} with {input}");
        }
        for body in [
            r"($e) => {}",
            r"($e:unknown) => {}",
            r"() => { ${count(x)} }",
            r"() => {} junk",
            "",
        ] {
            assert!(Rules::parse(body.parse().unwrap()).is_none(), "{body}");
        }
    }

    /// A fragment passed on is one token, which a literal token in another
    /// macro's matcher doesn't match, as in rustc.
    #[test]
    fn fragments_passed_on_are_opaque() {
        let forward = rules(r"($v:vis) => { $v }");
        let forwarded = forward.expand("pub".parse().unwrap()).unwrap();
        let callee = r"(pub) => { mod literal; }; ($v:vis) => { mod fragment; };";
        assert_eq!(
            rules(callee)
                .expand(forwarded)
                .map(|tokens| tokens.to_string()),
            Some(text("mod fragment;"))
        );
    }
}
