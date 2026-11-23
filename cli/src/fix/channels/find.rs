use syn::{Attribute, Block, Expr, Local, Stmt, visit::Visit};

use super::{ATTRIBUTE, ATTRIBUTE_MACRO, Constructors, Kind};
use crate::fix::{
    scan::macro_attribute,
    source::{Text, bare},
};

pub(super) struct Found<'a> {
    pub(super) start: usize,
    pub(super) kind: Kind,
    pub(super) local: &'a Local,
    pub(super) rest: &'a [Stmt],
    pub(super) refusal: Option<&'static str>,
}

pub(super) struct Finder<'t, 'c> {
    pub(super) text: &'t Text<'t>,
    pub(super) constructors: &'c Constructors,
    pub(super) hazards: usize,
    pub(super) found: Vec<Found<'t>>,
}

impl<'t> Finder<'t, '_> {
    fn constructor(&self, expr: &Expr) -> Option<(usize, Kind)> {
        let Expr::Call(call) = expr else {
            return None;
        };
        let range = self.text.of(&call.func)?;
        let &(end, kind) = self.constructors.get(&range.start)?;
        (end == range.end).then_some((range.start, kind))
    }

    /// Visits an item, counting it as a hazard when an attribute macro on it
    /// might rewrite what it contains.
    fn within(&mut self, attrs: &[Attribute], visit: impl FnOnce(&mut Self)) {
        let hazard = usize::from(
            attrs
                .iter()
                .any(|attr| macro_attribute(std::slice::from_ref(attr)) && !runtime_entry(attr)),
        );
        self.hazards += hazard;
        visit(self);
        self.hazards -= hazard;
    }
}

fn runtime_entry(attr: &Attribute) -> bool {
    let segments: Vec<String> = attr
        .path()
        .segments
        .iter()
        .map(|segment| bare(&segment.ident))
        .collect();
    segments == ["tokio", "main"] || segments == ["tokio", "test"]
}

impl<'t> Visit<'t> for Finder<'t, '_> {
    fn visit_item_fn(&mut self, item: &'t syn::ItemFn) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_item_fn(finder, item)
        });
    }

    fn visit_impl_item_fn(&mut self, item: &'t syn::ImplItemFn) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_impl_item_fn(finder, item)
        });
    }

    fn visit_trait_item_fn(&mut self, item: &'t syn::TraitItemFn) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_trait_item_fn(finder, item)
        });
    }

    fn visit_item_impl(&mut self, item: &'t syn::ItemImpl) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_item_impl(finder, item)
        });
    }

    fn visit_item_trait(&mut self, item: &'t syn::ItemTrait) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_item_trait(finder, item)
        });
    }

    fn visit_item_mod(&mut self, item: &'t syn::ItemMod) {
        self.within(&item.attrs, |finder| {
            syn::visit::visit_item_mod(finder, item)
        });
    }

    fn visit_block(&mut self, block: &'t Block) {
        for (index, stmt) in block.stmts.iter().enumerate() {
            let Stmt::Local(local) = stmt else {
                continue;
            };
            let Some((start, kind)) = local
                .init
                .as_ref()
                .and_then(|init| self.constructor(&init.expr))
            else {
                continue;
            };
            let refusal = if self.hazards > 0 {
                Some(ATTRIBUTE_MACRO)
            } else if !local.attrs.is_empty() {
                Some(ATTRIBUTE)
            } else {
                None
            };
            self.found.push(Found {
                start,
                kind,
                local,
                rest: block.stmts.get(index + 1..).unwrap_or_default(),
                refusal,
            });
        }
        syn::visit::visit_block(self, block);
    }
}
