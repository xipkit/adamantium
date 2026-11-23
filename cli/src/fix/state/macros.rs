use std::collections::HashMap;

use syn::{Expr, Macro, punctuated::Punctuated, visit::Visit};

use super::resolve::Names;
use crate::fix::source::{Text, known_macro};

pub(super) struct MacroArgs {
    pub(super) args: Vec<Expr>,
    pub(super) format: usize,
    pub(super) destination: bool,
    pub(super) condition: bool,
    pub(super) conditional: usize,
}

impl MacroArgs {
    pub(super) fn named<'e>(&self, position: usize, arg: &'e Expr) -> Option<&'e Expr> {
        match arg {
            Expr::Assign(named)
                if position > self.format
                    && matches!(&*named.left, Expr::Path(path) if path.path.get_ident().is_some()) =>
            {
                Some(&named.right)
            }
            _ => None,
        }
    }
}

pub(super) struct Parsed<'t> {
    pub(super) text: &'t Text<'t>,
    pub(super) names: &'t Names,
    pub(super) macros: HashMap<usize, MacroArgs>,
}

impl<'ast> Visit<'ast> for Parsed<'_> {
    fn visit_macro(&mut self, mac: &'ast Macro) {
        let Some(name) = mac
            .path
            .get_ident()
            .map(ToString::to_string)
            .filter(|name| mac.path.leading_colon.is_none() && !self.names.macros.contains(name))
        else {
            return;
        };
        let Some(format) = known_macro(&name) else {
            return;
        };
        let Some(start) = self.text.of(mac).map(|range| range.start) else {
            return;
        };
        let Ok(args) = mac.parse_body_with(Punctuated::<Expr, syn::Token![,]>::parse_terminated)
        else {
            return;
        };
        let args: Vec<Expr> = args.into_iter().collect();
        for arg in &args {
            self.visit_expr(arg);
        }
        self.macros.insert(
            start,
            MacroArgs {
                args,
                format,
                destination: name == "write" || name == "writeln",
                condition: name == "assert" || name == "debug_assert",
                conditional: if name.starts_with("debug_assert") {
                    0
                } else if name.starts_with("assert") {
                    format
                } else {
                    usize::MAX
                },
            },
        );
    }
}
