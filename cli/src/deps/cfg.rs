//! Evaluating `#[cfg]` predicates for a dependency without compiling it.
//!
//! Features and the host target are known from Cargo and rustc. Everything
//! else, such as the profile's `debug_assertions` or a `--cfg` that a build
//! script or `RUSTFLAGS` sets, is unknown, and code behind an unknown
//! predicate counts as code that builds.

use std::collections::{BTreeSet, HashSet};

use syn::{
    Attribute, Ident, LitStr, Meta, Token, ext::IdentExt, parenthesized, parse::ParseStream,
    punctuated::Punctuated, token,
};

/// What is known about a predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn from_bool(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }

    pub fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }

    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    pub fn or(self, other: Self) -> Self {
        self.not().and(other.not()).not()
    }
}

/// Configuration names that the build profile or the compiler's flags set,
/// whatever `rustc --print cfg` says.
const PROFILE_NAMES: [&str; 7] = [
    "debug_assertions",
    "overflow_checks",
    "ub_checks",
    "panic",
    "relocation_model",
    "fmt_debug",
    "sanitize",
];

/// Configuration names that describe the target. Each one rustc doesn't
/// print for the host is false there.
const TARGET_NAMES: [&str; 14] = [
    "unix",
    "windows",
    "target_os",
    "target_family",
    "target_arch",
    "target_env",
    "target_abi",
    "target_vendor",
    "target_endian",
    "target_pointer_width",
    "target_has_atomic",
    "target_has_atomic_load_store",
    "target_has_atomic_equal_alignment",
    "target_thread_local",
];

/// Configuration names that are never set when a dependency's library is
/// built for another crate.
const UNSET_NAMES: [&str; 4] = ["test", "doc", "doctest", "miri"];

/// The configuration rustc sets for the host target, from `rustc --print cfg`.
#[derive(Debug, Default)]
pub struct Target {
    options: HashSet<(String, Option<String>)>,
    names: HashSet<String>,
}

impl Target {
    /// Reads the output of `rustc --print cfg`: one `name` or `name="value"`
    /// per line.
    pub fn parse(printed: &str) -> Self {
        let mut target = Self::default();
        for line in printed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let (name, value) = match line.split_once('=') {
                Some((name, value)) => (name, Some(value.trim_matches('"').to_owned())),
                None => (line, None),
            };
            target.names.insert(name.to_owned());
            target.options.insert((name.to_owned(), value));
        }
        target
    }
}

/// Everything that decides which of one crate's `cfg`s hold.
#[derive(Clone, Copy)]
pub struct Cfg<'a> {
    pub target: &'a Target,
    pub features: &'a BTreeSet<String>,
    pub proc_macro: bool,
}

impl Cfg<'_> {
    /// Whether code with these attributes builds: every `cfg` on it, and
    /// every `cfg` that a `cfg_attr` adds, must hold.
    pub fn enabled(&self, attrs: &[Attribute]) -> Truth {
        let mut truth = Truth::True;
        for (applies, meta) in self.effective(attrs) {
            if meta.path().is_ident("cfg") {
                let holds = match &meta {
                    Meta::List(list) => list
                        .parse_args_with(|input: ParseStream| self.predicate(input))
                        .unwrap_or(Truth::Unknown),
                    _ => Truth::Unknown,
                };
                truth = truth.and(applies.not().or(holds));
            }
        }
        truth
    }

    /// The attributes as they apply after `cfg_attr` is expanded, each with
    /// whether it applies. Attributes of a `cfg_attr` whose predicate is
    /// false are left out.
    pub fn effective(&self, attrs: &[Attribute]) -> Vec<(Truth, Meta)> {
        let mut metas = Vec::new();
        for attr in attrs {
            self.expand(Truth::True, attr.meta.clone(), &mut metas);
        }
        metas
    }

    fn expand(&self, applies: Truth, meta: Meta, metas: &mut Vec<(Truth, Meta)>) {
        let Meta::List(list) = &meta else {
            metas.push((applies, meta));
            return;
        };
        if !list.path.is_ident("cfg_attr") {
            metas.push((applies, meta));
            return;
        }
        let parsed = list.parse_args_with(|input: ParseStream| {
            let holds = self.predicate(input)?;
            input.parse::<Token![,]>()?;
            let inner = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
            Ok((holds, inner))
        });
        // A `cfg_attr` that doesn't parse can't add a `cfg` that is known to
        // be false, so the code still counts.
        if let Ok((holds, inner)) = parsed {
            let applies = applies.and(holds);
            if applies != Truth::False {
                for meta in inner {
                    self.expand(applies, meta, metas);
                }
            }
        }
    }

    fn predicate(&self, input: ParseStream) -> syn::Result<Truth> {
        let name = Ident::parse_any(input)?.to_string();
        if input.peek(Token![=]) {
            input.parse::<Token![=]>()?;
            let value: LitStr = input.parse()?;
            return Ok(self.option(&name, Some(&value.value())));
        }
        if input.peek(token::Paren) {
            let content;
            parenthesized!(content in input);
            let mut args = Vec::new();
            while !content.is_empty() {
                args.push(self.predicate(&content)?);
                if !content.is_empty() {
                    content.parse::<Token![,]>()?;
                }
            }
            return match name.as_str() {
                "all" => Ok(args.into_iter().fold(Truth::True, Truth::and)),
                "any" => Ok(args.into_iter().fold(Truth::False, Truth::or)),
                "not" if args.len() == 1 => {
                    Ok(args.into_iter().next().map_or(Truth::Unknown, Truth::not))
                }
                _ => Err(content.error("expected all, any or not")),
            };
        }
        Ok(match name.as_str() {
            "true" => Truth::True,
            "false" => Truth::False,
            _ => self.option(&name, None),
        })
    }

    fn option(&self, name: &str, value: Option<&str>) -> Truth {
        if name == "feature" {
            return Truth::from_bool(value.is_some_and(|value| self.features.contains(value)));
        }
        if name == "proc_macro" {
            return Truth::from_bool(value.is_none() && self.proc_macro);
        }
        if UNSET_NAMES.contains(&name) {
            return Truth::False;
        }
        if PROFILE_NAMES.contains(&name) {
            return Truth::Unknown;
        }
        let set = self
            .target
            .options
            .contains(&(name.to_owned(), value.map(str::to_owned)));
        // `RUSTFLAGS` can turn on target features beyond the host's defaults.
        if name == "target_feature" {
            return if set { Truth::True } else { Truth::Unknown };
        }
        if TARGET_NAMES.contains(&name) || self.target.names.contains(name) {
            return Truth::from_bool(set);
        }
        Truth::Unknown
    }
}

#[cfg(test)]
mod tests {
    use syn::parse::Parser;

    use super::*;

    /// Evaluates a `cfg` predicate written as source.
    fn eval(cfg: &Cfg, predicate: &str) -> syn::Result<Truth> {
        (|input: ParseStream| cfg.predicate(input)).parse_str(predicate)
    }

    const LINUX: &str = "debug_assertions\npanic=\"unwind\"\ntarget_arch=\"x86_64\"\n\
        target_feature=\"sse2\"\ntarget_family=\"unix\"\ntarget_os=\"linux\"\n\
        target_pointer_width=\"64\"\nunix\n";

    fn check(predicate: &str, expected: Truth) {
        let target = Target::parse(LINUX);
        let features = BTreeSet::from(["on".to_owned()]);
        let cfg = Cfg {
            target: &target,
            features: &features,
            proc_macro: false,
        };
        assert_eq!(eval(&cfg, predicate).unwrap(), expected, "{predicate}");
    }

    #[test]
    fn features_are_known() {
        check(r#"feature = "on""#, Truth::True);
        check(r#"feature = "off""#, Truth::False);
        check(r#"not(feature = "off")"#, Truth::True);
    }

    #[test]
    fn the_host_target_is_known() {
        check("unix", Truth::True);
        check("windows", Truth::False);
        check(r#"target_os = "linux""#, Truth::True);
        check(r#"target_os = "macos""#, Truth::False);
        check(r#"target_pointer_width = "8""#, Truth::False);
        check(r#"target_env = "msvc""#, Truth::False);
    }

    #[test]
    fn tests_and_docs_are_off() {
        check("test", Truth::False);
        check("not(test)", Truth::True);
        check("doc", Truth::False);
    }

    #[test]
    fn profile_flags_and_custom_names_are_unknown() {
        check("debug_assertions", Truth::Unknown);
        check(r#"panic = "abort""#, Truth::Unknown);
        check("tokio_unstable", Truth::Unknown);
        check("not(tokio_unstable)", Truth::Unknown);
        check(r#"target_feature = "avx2""#, Truth::Unknown);
        check(r#"target_feature = "sse2""#, Truth::True);
    }

    #[test]
    fn unknown_combines_with_known() {
        check("all(unix, tokio_unstable)", Truth::Unknown);
        check("all(windows, tokio_unstable)", Truth::False);
        check("any(unix, tokio_unstable)", Truth::True);
        check("any(windows, tokio_unstable)", Truth::Unknown);
        check("all()", Truth::True);
        check("any()", Truth::False);
        check("true", Truth::True);
        check("false", Truth::False);
    }

    #[test]
    fn attributes_combine_cfg_and_cfg_attr() {
        let target = Target::parse(LINUX);
        let features = BTreeSet::new();
        let cfg = Cfg {
            target: &target,
            features: &features,
            proc_macro: false,
        };
        let item: syn::ItemFn =
            syn::parse_str("#[cfg(unix)] #[cfg_attr(windows, cfg(test))] fn f() {}").unwrap();
        assert_eq!(cfg.enabled(&item.attrs), Truth::True);
        let item: syn::ItemFn = syn::parse_str("#[cfg_attr(unix, cfg(test))] fn f() {}").unwrap();
        assert_eq!(cfg.enabled(&item.attrs), Truth::False);
        let item: syn::ItemFn = syn::parse_str("#[cfg_attr(loom, cfg(test))] fn f() {}").unwrap();
        assert_eq!(cfg.enabled(&item.attrs), Truth::Unknown);
        let item: syn::ItemFn = syn::parse_str("#[cfg(unix)] #[cfg(test)] fn f() {}").unwrap();
        assert_eq!(cfg.enabled(&item.attrs), Truth::False);
    }
}
