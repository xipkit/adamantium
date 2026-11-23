use std::{collections::BTreeSet, slice};

use proc_macro2::TokenTree;
use syn::{parse::Parser, visit_mut::VisitMut};

use super::{
    super::cfg::{Features, Target},
    attrs::{is_unsafe_attribute, item_attrs},
    flatten::{Expand, reads_input},
    visitor::trailing_path,
    *,
};

#[test]
fn cfg_if_gives_each_branch_the_cfg_that_selects_it() {
    let target = Target::parse("unix\ntarget_os=\"linux\"\n");
    let features = Features::default();
    let cfg = Cfg {
        target: &target,
        features: &features,
        proc_macro: false,
    };
    let mut file = syn::parse_file(
        r#"
            cfg_if::cfg_if! {
                if #[cfg(windows)] { fn a() {} }
                else if #[cfg(unix)] { fn b() {} struct B; }
                else if #[cfg(loom)] { fn c() {} }
                else { fn d() {} }
            }
            cfg_if! {
                if #[cfg(loom)] {
                    fn e() {}
                    cfg_if! {
                        if #[cfg(unix)] { fn f() {} } else { fn g() {} }
                    }
                } else {
                    #[cfg(windows)]
                    fn h() {}
                    fn i() {}
                }
            }
            cfg_if! { if #[cfg(unix, windows)] { fn j() {} } else { fn l() {} } }
            #[cfg(windows)]
            cfg_if! { if #[cfg(unix)] { fn k() {} } }
            "#,
    )
    .unwrap();
    Expand.visit_file_mut(&mut file);
    let built: Vec<(String, Truth)> = file
        .items
        .iter()
        .filter_map(|item| {
            let name = match item {
                Item::Fn(item) => item.sig.ident.to_string(),
                Item::Struct(item) => item.ident.to_string(),
                Item::Macro(call) if reads_input(&call.mac) => {
                    return None;
                }
                _ => panic!("an item that isn't a function, struct or flattened call"),
            };
            Some((name, cfg.enabled(item_attrs(item))))
        })
        .collect();
    let expected = [
        ("a", Truth::False),
        ("b", Truth::True),
        ("B", Truth::True),
        ("c", Truth::False),
        ("d", Truth::False),
        ("e", Truth::Unknown),
        ("f", Truth::Unknown),
        ("g", Truth::False),
        ("h", Truth::False),
        ("i", Truth::Unknown),
        ("j", Truth::False),
        ("l", Truth::False),
        ("k", Truth::False),
    ]
    .map(|(name, truth)| (name.to_owned(), truth));
    assert_eq!(built, expected);
}

#[test]
fn only_cfg_if_bodies_are_read_as_branches() {
    for source in [
        "cfg_if! { if #[cfg(unix)] { fn a() {} } else }",
        "cfg_if! { if #[cfg(unix)] #[cfg(windows)] { fn a() {} } }",
        "cfg_if! { if #[cfg] { fn a() {} } }",
        "cfg_if! { if #[cfg()] { fn a() {} } }",
        "cfg_if! { if #[cfg_attr(unix, cfg(windows))] { fn a() {} } }",
        "cfg_if! { if #[cfg(unix)] { fn a() {} } else { fn b() {} } else { fn c() {} } }",
        "cfg_if! { if #[cfg(unix)] { mod a; 1 + 1 } }",
        "other! { if #[cfg(unix)] { fn a() {} } }",
    ] {
        let mut file = syn::parse_file(source).unwrap();
        Expand.visit_file_mut(&mut file);
        assert!(
            matches!(&file.items[..], [Item::Macro(_)]),
            "{source}: {:?}",
            file.items.len()
        );
    }
}

#[test]
fn cfg_select_gives_each_arm_the_cfg_that_selects_it() {
    let target = Target::parse("unix\ntarget_pointer_width=\"64\"\n");
    let features = Features::default();
    let cfg = Cfg {
        target: &target,
        features: &features,
        proc_macro: false,
    };
    let mut file = syn::parse_file(
        r#"
            cfg_select! {
                windows => { fn a() {} },
                all(unix, target_pointer_width = "64") => { fn b() {} struct B; }
                platform_custom => { fn c() {} }
                _ => { fn d() {} },
            }
            std::cfg_select! {
                platform_custom => {
                    fn e() {}
                    core::cfg_select! {
                        unix => { fn f() {} }
                        _ => { fn g() {} }
                    }
                }
                _ => {
                    #[cfg(windows)]
                    fn h() {}
                    fn i() {}
                }
            }
            ::core::cfg_select! {
                target_pointer_width = "8" => { fn j() {} }
                _ => { fn k() {} }
            }
            #[cfg(windows)]
            ::std::cfg_select! { unix => { fn l() {} } }
            cfg_select! { _ => { fn m() {} } }
            cfg_select! { feature = "off" => { fn n() {} } }
            cfg_select! {
                platform_custom => { fn o() {} }
                unix => { fn p() {} }
                platform_later => { fn q() {} }
                _ => { fn r() {} }
            }
            "#,
    )
    .unwrap();
    Expand.visit_file_mut(&mut file);
    let built: Vec<_> = file
        .items
        .iter()
        .filter_map(|item| {
            let name = match item {
                Item::Fn(item) => item.sig.ident.to_string(),
                Item::Struct(item) => item.ident.to_string(),
                Item::Macro(call) if reads_input(&call.mac) => {
                    return None;
                }
                _ => panic!("an item that isn't a function, struct or flattened call"),
            };
            Some((name, cfg.enabled(item_attrs(item))))
        })
        .collect();
    let expected = [
        ("a", Truth::False),
        ("b", Truth::True),
        ("B", Truth::True),
        ("c", Truth::False),
        ("d", Truth::False),
        ("e", Truth::Unknown),
        ("f", Truth::Unknown),
        ("g", Truth::False),
        ("h", Truth::False),
        ("i", Truth::Unknown),
        ("j", Truth::False),
        ("k", Truth::True),
        ("l", Truth::False),
        ("m", Truth::True),
        ("n", Truth::False),
        ("o", Truth::Unknown),
        ("p", Truth::Unknown),
        ("q", Truth::False),
        ("r", Truth::False),
    ]
    .map(|(name, truth)| (name.to_owned(), truth));
    assert_eq!(built, expected);
}

#[test]
fn unreadable_cfg_select_bodies_stay_macros() {
    for source in [
        "cfg_select! {}",
        "cfg_select! { => { fn a() {} } }",
        "cfg_select! { unix { fn a() {} } }",
        "cfg_select! { unix => fn a() {} }",
        "cfg_select! { unix => { mod a; 1 + 1 } }",
        "cfg_select! { _ => { fn a() {} } unix => { fn b() {} } }",
        "cfg_select! { unix => { fn a() {} },, }",
        "cfg_select! { unix => { fn a() {} } _ => 42 }",
        "other! { unix => { fn a() {} } }",
        "other::cfg_select! { unix => { fn a() {} } }",
        "std::other::cfg_select! { unix => { fn a() {} } }",
    ] {
        let mut file = syn::parse_file(source).unwrap();
        Expand.visit_file_mut(&mut file);
        assert!(
            matches!(&file.items[..], [Item::Macro(_)]),
            "{source}: {:?}",
            file.items.len()
        );
    }
}

#[test]
fn trailing_path_reads_a_path_back_from_its_end() {
    for (source, path) in [
        (
            "core::arch::global_asm",
            Some((&["core", "arch", "global_asm"][..], false)),
        ),
        (
            "::std::arch::global_asm",
            Some((&["std", "arch", "global_asm"], true)),
        ),
        (
            "$crate::reexported",
            Some((&["crate", "reexported"], false)),
        ),
        ("let x: ::core::name", Some((&["core", "name"], true))),
        ("a; b::c", Some((&["b", "c"], false))),
        ("r#global_asm", Some((&["global_asm"], false))),
        ("x.", None),
        ("", None),
    ] {
        let trees: Vec<TokenTree> = source.parse::<TokenStream>().unwrap().into_iter().collect();
        let expected = path.map(|(path, leading)| {
            (
                path.iter().map(|segment| segment.to_string()).collect(),
                leading,
            )
        });
        assert_eq!(trailing_path(&trees), expected, "{source}");
    }
}

#[test]
fn unsafe_attributes_are_the_ones_unsafe_code_reports() {
    for (source, reported) in [
        ("#[no_mangle]", true),
        ("#[export_name = \"name\"]", true),
        ("#[link_section = \".data\"]", true),
        ("#[naked]", true),
        ("#[ffi_pure]", true),
        ("#[ffi_const]", true),
        ("#[force_target_feature(enable = \"avx2\")]", true),
        ("#[allow_internal_unsafe]", true),
        ("#[unsafe(no_mangle)]", true),
        ("#[unsafe(export_name = \"name\")]", true),
        ("#[used]", false),
        ("#[link_name = \"name\"]", false),
        ("#[inline]", false),
        ("#[target_feature(enable = \"avx2\")]", false),
        ("#[allow(unsafe_code)]", false),
        ("#[other::no_mangle]", false),
    ] {
        let attrs = Attribute::parse_outer.parse_str(source).unwrap();
        let [attr] = &attrs[..] else {
            panic!("{source} is one attribute");
        };
        assert_eq!(is_unsafe_attribute(&attr.meta), reported, "{source}");
    }
}

fn scan_files(files: &[(&str, &str)]) -> Scan {
    scan_with(files, false, &[])
}

#[derive(Clone, Copy)]
struct Library<'t> {
    name: &'t str,
    features: &'t [&'t str],
    files: &'t [(&'t str, &'t str)],
}

fn package(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        let path = dir.path().join("src").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    dir
}

fn scan_with(files: &[(&str, &str)], edition_2015: bool, libraries: &[Library]) -> Scan {
    let target = Target::parse("unix\ntarget_os=\"linux\"\n");
    let no_externs = BTreeMap::new();
    let dirs: Vec<_> = libraries
        .iter()
        .map(|library| package(library.files))
        .collect();
    let srcs: Vec<_> = dirs
        .iter()
        .map(|dir| dir.path().join("src/lib.rs"))
        .collect();
    let features: Vec<_> = libraries
        .iter()
        .map(|library| {
            Features::from_sets(&[library.features.iter().map(|f| (*f).to_owned()).collect()])
        })
        .collect();
    let mut crates: Vec<_> = libraries
        .iter()
        .zip(&dirs)
        .zip(&srcs)
        .zip(&features)
        .map(|(((library, dir), src), features)| Crate {
            name: library.name,
            root: dir.path(),
            src,
            edition_2015: false,
            externs: &no_externs,
            dependencies: BTreeMap::new(),
            cfg: Cfg {
                target: &target,
                features,
                proc_macro: false,
            },
        })
        .collect();
    let externs = libraries
        .iter()
        .map(|library| (library.name.to_owned(), library.name.to_owned()))
        .collect();
    let dependencies = libraries
        .iter()
        .enumerate()
        .map(|(index, library)| (library.name.to_owned(), index))
        .collect();
    let dir = package(files);
    let features = Features::default();
    let src = dir.path().join("src/lib.rs");
    let entry = crates.len();
    crates.push(Crate {
        name: "demo",
        root: dir.path(),
        src: &src,
        edition_2015,
        externs: &externs,
        dependencies,
        cfg: Cfg {
            target: &target,
            features: &features,
            proc_macro: false,
        },
    });
    scan(&crates, entry, &Disallowed::bundled().unwrap())
}

fn unscanned(file: &str, line: usize, reason: &str) -> Unscanned {
    Unscanned {
        file: file.to_owned(),
        line: Some(line),
        reason: reason.to_owned(),
    }
}

#[test]
fn calls_of_crate_macros_without_input_are_expanded() {
    let scan = scan_files(&[
        ("lib.rs", "#[macro_use]\nmod macros;\n\nroot!();\n"),
        (
            "macros.rs",
            r"macro_rules! root {
    () => {
        mod engine;
        pub struct Handle;
        unsafe impl Send for Handle {}
        pub fn start() {
            std::thread::spawn(|| ());
        }
    };
}
",
        ),
        ("engine.rs", "pub unsafe fn run() {}\n"),
    ]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(
        scan.unsafe_code,
        UnsafeCounts {
            functions: 1,
            impls: 1,
            ..UnsafeCounts::default()
        }
    );
    let sites: Vec<_> = scan.calls.values().flatten().collect();
    let site = Site {
        file: "src/macros.rs".to_owned(),
        line: 7,
        column: 13,
    };
    assert_eq!(sites, [&site]);
}

#[test]
fn inner_macro_use_keeps_a_modules_macros_in_scope() {
    let scan = scan_files(&[
        (
            "lib.rs",
            r"macro_rules! root { () => { mod old; }; }
mod macros;
root!();
mod inline {
    #![macro_use]
    macro_rules! other { () => { mod inner; }; }
}
other!();
",
        ),
        (
            "macros.rs",
            "#![macro_use]\nmacro_rules! root { () => { mod new; }; }\n",
        ),
        ("new.rs", "pub unsafe fn a() {}\n"),
        ("inner.rs", "pub unsafe fn b() {}\n"),
    ]);
    let reason = "module `old` is declared in a macro body the scan can't read";
    assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 1, reason)]);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn macros_with_calls_that_are_not_expanded_are_read_as_tokens() {
    let scan = scan_files(&[(
        "lib.rs",
        r"macro_rules! runner {
    () => {
        pub fn run() {
            fn spawn(_: fn()) {}
            spawn(|| ());
        }
    };
}
runner!();
pub fn outer() {
    use std::thread::spawn;
    runner!();
}
",
    )]);
    assert_eq!(scan.unresolved, BTreeMap::from([("spawn".to_owned(), 1)]));
    assert_eq!(scan.calls, BTreeMap::new());
}

#[test]
fn only_macros_in_textual_scope_are_expanded() {
    let scan = scan_files(&[
        (
            "lib.rs",
            r"mod private {
    macro_rules! private { () => { mod one; }; }
}
private!();
macro_rules! early { () => { mod two; }; }
mod outer {
    early!();
}
",
        ),
        ("outer/two.rs", "pub unsafe fn f() {}\n"),
        ("one.rs", "pub unsafe fn g() {}\n"),
    ]);
    let reason = "module `one` is declared in a macro body the scan can't read";
    assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 2, reason)]);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn calls_expand_each_definition_a_build_may_have() {
    let scan = scan_files(&[
        (
            "lib.rs",
            r"macro_rules! pick { () => { mod hidden; }; }
macro_rules! pick { () => { mod first; }; }
#[cfg(demo_custom)]
macro_rules! pick { () => { mod custom; }; }
#[cfg(windows)]
macro_rules! pick { () => { mod never; }; }
#[cfg_attr(windows, macro_use)]
mod nothing {
    macro_rules! pick { () => { mod never; }; }
}
pick!();
",
        ),
        ("first.rs", "pub unsafe fn f() {}\n"),
        ("custom.rs", "pub unsafe fn g() {}\n"),
    ]);
    let reason =
        |name: &str| format!("module `{name}` is declared in a macro body the scan can't read");
    let listed = [(1, "hidden"), (9, "never")]
        .map(|(line, name)| unscanned("src/lib.rs", line, &reason(name)));
    assert_eq!(scan.unscanned, listed);
    assert_eq!(scan.unsafe_code.functions, 2);
}

const PSM: Library = Library {
    name: "psm",
    features: &[],
    files: &[(
        "lib.rs",
        r"#[cfg(switchable_stack)]
#[macro_export]
macro_rules! psm_stack_manipulation {
    (yes { $($yes:tt)* } no { $($no:tt)* }) => { $($yes)* };
}

#[cfg(not(switchable_stack))]
#[macro_export]
macro_rules! psm_stack_manipulation {
    (yes { $($yes:tt)* } no { $($no:tt)* }) => { $($no)* };
}
",
    )],
};

#[test]
fn calls_of_macros_another_crate_exports_are_expanded() {
    let scan = scan_with(
        &[
            (
                "lib.rs",
                r"psm_stack_manipulation! {
    yes {
        mod guard;
        use std::thread::spawn as go;
    }
    no {
        mod fallback;
        fn go(_: fn()) {}
    }
}

#[macro_use]
extern crate psm;

pub fn run() {
    go(|| ());
}
",
            ),
            ("guard.rs", "pub unsafe fn a() {}\n"),
            ("fallback.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[PSM],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
    let sites: Vec<_> = scan.calls.values().flatten().collect();
    let site = Site {
        file: "src/lib.rs".to_owned(),
        line: 16,
        column: 5,
    };
    assert_eq!(sites, [&site]);
}

#[test]
fn other_crates_macros_are_defined_by_their_builds() {
    let fast = Library {
        name: "fast",
        features: &["fast"],
        files: &[(
            "lib.rs",
            r#"#[cfg(feature = "fast")]
#[macro_export]
macro_rules! pick { () => { mod quick; }; }

#[cfg(not(feature = "fast"))]
#[macro_export]
macro_rules! pick { () => { mod slow; }; }

#[cfg(windows)]
mod windows {
    #[macro_export]
    macro_rules! never { () => { mod never; }; }
}
"#,
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "#[macro_use]\nextern crate fast;\n\npick!();\nnever!();\n",
            ),
            ("quick.rs", "pub unsafe fn a() {}\n"),
            ("slow.rs", "pub unsafe fn b() {}\n"),
            ("never.rs", "pub unsafe fn c() {}\n"),
        ],
        false,
        &[fast],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn other_crates_macros_name_and_count_for_their_crate() {
    let tokio = Library {
        name: "tokio",
        features: &[],
        files: &[(
            "lib.rs",
            r"pub fn spawn() {}

#[macro_export]
macro_rules! runtime {
    ($name:ident { $($body:tt)* }) => {
        pub use $crate::spawn as start;
        pub unsafe fn hidden() {
            unsafe { std::thread::spawn(|| ()) };
        }
        unsafe impl Send for Handle {}
        pub fn $name() { $($body)* }
    };
}
",
        )],
    };
    for (edition_2015, item) in [
        (false, "extern crate tokio as rt;"),
        (true, "extern crate tokio as rt;"),
        (false, "extern crate tokio;"),
    ] {
        let lib = format!(
            "#[macro_use]\n{item}\n\nruntime! {{ mine {{ unsafe {{ std::hint::spin_loop() }} }} }}\n\npub struct Handle;\n\npub fn run() {{\n    start();\n}}\n"
        );
        let scan = scan_with(&[("lib.rs", &lib)], edition_2015, slice::from_ref(&tokio));
        assert_eq!(scan.unscanned, [], "{item}");
        assert_eq!(
            scan.unsafe_code,
            UnsafeCounts {
                blocks: 1,
                ..UnsafeCounts::default()
            },
            "{item}"
        );
        let calls: Vec<_> = scan
            .calls
            .iter()
            .map(|(entry, sites)| {
                (
                    Disallowed::bundled().unwrap().entries[*entry].path.clone(),
                    sites.clone(),
                )
            })
            .collect();
        let site = Site {
            file: "src/lib.rs".to_owned(),
            line: 9,
            column: 5,
        };
        assert_eq!(calls, [("tokio::spawn".to_owned(), vec![site])], "{item}");
    }
}

#[test]
fn other_crates_macros_come_after_textual_scope() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            r"#[macro_export]
macro_rules! pick { () => { mod theirs; }; }
#[macro_export]
macro_rules! skipped { () => { mod skipped; }; }
",
        )],
    };
    let plain = Library {
        name: "plain",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! plain { () => { mod plain; }; }\n",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                r"mod before;

macro_rules! pick { () => { mod own; }; }
pick!();
skipped!();
plain!();

#[macro_use(pick)]
extern crate picks;
extern crate plain;
",
            ),
            ("before.rs", "pick!();\n"),
            ("before/theirs.rs", "pub unsafe fn a() {}\n"),
            ("own.rs", "pub unsafe fn b() {}\n"),
            ("theirs.rs", "pub unsafe fn c() {}\n"),
            ("skipped.rs", "pub unsafe fn d() {}\n"),
            ("plain.rs", "pub unsafe fn e() {}\n"),
        ],
        false,
        &[picks, plain],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn other_crates_macros_are_hidden_by_imports() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { () => { mod theirs; }; }\n",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "#[macro_use]\nextern crate picks;\n\nmod open;\nmod imports;\nmod globs;\n",
            ),
            ("open.rs", "pick!();\n"),
            ("open/theirs.rs", "pub unsafe fn a() {}\n"),
            ("imports.rs", "use other::pick;\npick!();\n"),
            ("imports/theirs.rs", "pub unsafe fn b() {}\n"),
            ("globs.rs", "use other::*;\npick!();\n"),
            ("globs/theirs.rs", "pub unsafe fn c() {}\n"),
        ],
        false,
        &[picks],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn later_macro_use_imports_hide_earlier_ones() {
    let unreadable = Library {
        name: "unreadable",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { () => { $$ }; }\n",
        )],
    };
    let first = Library {
        name: "first",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { () => { mod first; }; }\n",
        )],
    };
    let second = Library {
        name: "second",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { () => { mod second; }; }\n",
        )],
    };
    for (cfg, functions) in [("", 1), ("#[cfg(demo_custom)]\n", 2)] {
        let lib = format!(
            "#[macro_use]\nextern crate first;\n{cfg}#[macro_use]\nextern crate second;\n\npick!();\n"
        );
        let scan = scan_with(
            &[
                ("lib.rs", &lib),
                ("first.rs", "pub unsafe fn a() {}\n"),
                ("second.rs", "pub unsafe fn b() {}\n"),
            ],
            false,
            &[first, second],
        );
        assert_eq!(scan.unscanned, [], "{cfg}");
        assert_eq!(scan.unsafe_code.functions, functions, "{cfg}");
    }
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "#[macro_use]\nextern crate unreadable;\n#[macro_use]\nextern crate second;\n\npick!();\n",
            ),
            ("second.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[unreadable, second],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn macros_other_crates_macros_define_do_not_hide_each_other() {
    let configure = Library {
        name: "configure",
        features: &[],
        files: &[(
            "lib.rs",
            r"#[cfg(choice)]
#[macro_export]
macro_rules! configure {
    () => { macro_rules! selected { () => { mod first; }; } };
}

#[cfg(not(choice))]
#[macro_export]
macro_rules! configure {
    () => { macro_rules! selected { () => { mod second; }; } };
}
",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "#[macro_use]\nextern crate configure;\n\nconfigure!();\nselected!();\n",
            ),
            ("first.rs", "pub unsafe fn a() {}\n"),
            ("second.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[configure],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn crate_passed_on_names_its_crate() {
    let tokio = Library {
        name: "tokio",
        features: &[],
        files: &[(
            "lib.rs",
            r"pub fn spawn() {}

#[macro_export]
macro_rules! outer {
    () => { inner!($crate); };
}

#[macro_export]
macro_rules! inner {
    ($c:ident) => {
        mod target;
        pub use $c::spawn as go;
        pub use {$crate::spawn as start, std::thread::spawn as thread};
        pub fn local() { use {$crate::spawn as begin}; begin(); }
    };
    (::$c:ident) => { mod fallback; };
}
",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "#[macro_use]\nextern crate tokio;\n\nouter!();\n\npub fn run() {\n    go();\n    start();\n}\n",
            ),
            ("target.rs", "pub unsafe fn a() {}\n"),
            ("fallback.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[tokio],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
    let paths: Vec<_> = scan
        .calls
        .iter()
        .map(|(entry, sites)| {
            let path = Disallowed::bundled().unwrap().entries[*entry].path.clone();
            (path, sites.iter().map(|site| site.line).collect::<Vec<_>>())
        })
        .collect();
    assert_eq!(paths, [("tokio::spawn".to_owned(), vec![7, 8])]);
}

#[test]
fn local_inner_macros_call_their_crates_macros() {
    let outer = Library {
        name: "outer",
        features: &[],
        files: &[(
            "lib.rs",
            r"#[macro_export(local_inner_macros)]
macro_rules! outer { () => { helper!(); }; }

#[macro_export]
macro_rules! helper { () => { mod theirs; }; }
",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "#[macro_use]\nextern crate outer;\n\nmacro_rules! helper { () => { mod mine; }; }\nouter!();\n",
            ),
            ("mine.rs", "pub unsafe fn a() {}\n"),
            ("theirs.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[outer],
    );
    let reason = "module `mine` is declared in a macro body the scan can't read";
    assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 4, reason)]);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn calls_of_other_crates_macros_by_path_are_expanded() {
    let lib = r"psm::psm_stack_manipulation! {
    yes { mod guard; }
    no { mod fallback; }
}

mod inner {
    ::psm::psm_stack_manipulation! {
        yes { mod guard; }
        no { mod fallback; }
    }
}

extern crate psm as stack;

stack::psm_stack_manipulation! {
    yes { mod renamed; }
    no { mod renamed_fallback; }
}
";
    let files = [
        ("lib.rs", lib),
        ("guard.rs", "pub unsafe fn a() {}\n"),
        ("fallback.rs", "pub unsafe fn b() {}\n"),
        ("inner/guard.rs", "pub unsafe fn c() {}\n"),
        ("inner/fallback.rs", "pub unsafe fn d() {}\n"),
        ("renamed.rs", "pub unsafe fn e() {}\n"),
        ("renamed_fallback.rs", "pub unsafe fn f() {}\n"),
    ];
    for edition_2015 in [false, true] {
        let scan = scan_with(&files, edition_2015, &[PSM]);
        assert_eq!(scan.unscanned, [], "2015: {edition_2015}");
        assert_eq!(scan.unsafe_code.functions, 6, "2015: {edition_2015}");
    }
}

#[test]
fn calls_of_other_crates_macros_a_use_brings_in_are_expanded() {
    let scan = scan_with(
        &[
            ("lib.rs", "mod named;\nmod renamed;\n"),
            (
                "named.rs",
                "use psm::psm_stack_manipulation;\n\npsm_stack_manipulation! {\n    yes { mod guard; }\n    no { mod fallback; }\n}\n",
            ),
            (
                "renamed.rs",
                "use psm::psm_stack_manipulation as stack;\n\nstack! {\n    yes { mod guard; }\n    no { mod fallback; }\n}\n",
            ),
            ("named/guard.rs", "pub unsafe fn a() {}\n"),
            ("named/fallback.rs", "pub unsafe fn b() {}\n"),
            ("renamed/guard.rs", "pub unsafe fn c() {}\n"),
            ("renamed/fallback.rs", "pub unsafe fn d() {}\n"),
        ],
        false,
        &[PSM],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 4);
}

#[test]
fn macros_a_use_brings_in_come_between_textual_scope_and_the_prelude() {
    let library = |name, files| Library {
        name,
        features: &[],
        files,
    };
    let picks = library(
        "picks",
        &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { () => { mod theirs; }; }\n",
        )],
    );
    let prelude = library(
        "prelude",
        &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { () => { mod prelude; }; }\n",
        )],
    );
    let modules = ["imported", "maybe", "textual", "unknown"];
    let mut files = vec![
            (
                "lib.rs".to_owned(),
                "#[macro_use]\nextern crate prelude;\n\nmod imported;\nmod maybe;\nmod textual;\nmod unknown;\n"
                    .to_owned(),
            ),
            (
                "imported.rs".to_owned(),
                "use picks::pick;\npick!();\n".to_owned(),
            ),
            (
                "maybe.rs".to_owned(),
                "#[cfg(demo_custom)]\nuse picks::pick;\npick!();\n".to_owned(),
            ),
            (
                "textual.rs".to_owned(),
                "use picks::pick;\nmacro_rules! pick { () => { mod own; }; }\npick!();\n".to_owned(),
            ),
            (
                "unknown.rs".to_owned(),
                "use other::pick;\nmacro_rules! pick { () => { mod own; }; }\npick!();\n".to_owned(),
            ),
        ];
    for module in modules {
        for child in ["theirs", "prelude", "own"] {
            files.push((
                format!("{module}/{child}.rs"),
                "pub fn run() {\n    std::thread::spawn(|| ());\n}\n".to_owned(),
            ));
        }
    }
    let files: Vec<_> = files
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_str()))
        .collect();
    let scan = scan_with(&files, false, &[picks, prelude]);
    assert_eq!(scan.unscanned, []);
    let scanned: BTreeSet<_> = scan
        .calls
        .values()
        .flatten()
        .map(|site| site.file.as_str())
        .collect();
    assert_eq!(
        scanned,
        BTreeSet::from([
            "src/imported/theirs.rs",
            "src/maybe/prelude.rs",
            "src/maybe/theirs.rs",
            "src/textual/own.rs",
            "src/unknown/own.rs",
        ])
    );
}

#[test]
fn paths_to_other_crates_macros_follow_their_reexports() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            r"#[macro_export]
macro_rules! pick {
    (yes { $($yes:tt)* }) => { $($yes)* };
}

pub mod macros {
    pub use crate::pick as choose;
}

mod functions {
    pub fn pick() {}
}

pub use functions::pick;
",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "picks::macros::choose! { yes { mod chosen; } }\npicks::pick! { yes { mod picked; } }\n",
            ),
            ("chosen.rs", "pub unsafe fn a() {}\n"),
            ("picked.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[picks],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn macro_paths_the_scan_cannot_follow_are_not_expanded() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            r"#[macro_export]
macro_rules! pick {
    (yes { $($yes:tt)* }) => { $($yes)* };
}

pub fn spawn() {}
",
        )],
    };
    let scan = scan_with(
        &[(
            "lib.rs",
            r"other::pick! { yes { mod other; } }
picks::spawn! { yes { mod spawn; } }
picks::missing! { yes { mod missing; } }
pub fn own() {}
crate::own! { yes { mod own; } }
",
        )],
        false,
        &[picks],
    );
    let reason =
        |name: &str| format!("module `{name}` is declared in a macro body the scan can't read");
    let listed = [(1, "other"), (2, "spawn"), (3, "missing"), (5, "own")]
        .map(|(line, name)| unscanned("src/lib.rs", line, &reason(name)));
    assert_eq!(scan.unscanned, listed);
}

#[test]
fn macros_a_use_and_the_prelude_both_bring_in_are_read_once() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { (yes { $($yes:tt)* }) => { $($yes)* }; }\n",
        )],
    };
    let scan = scan_with(
        &[(
            "lib.rs",
            "#[macro_use]\nextern crate picks;\n\n#[cfg(demo_custom)]\nuse picks::pick;\n\npick! { yes { pub unsafe fn f() {} } }\n",
        )],
        false,
        &[picks],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn macros_a_parent_module_brings_in_are_followed() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { (yes { $($yes:tt)* }) => { $($yes)* }; }\n",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "use picks::pick;\n\nmod child;\n\nmod inline {\n    use super::pick;\n    pick! { yes { mod body; } }\n}\n",
            ),
            (
                "child.rs",
                "use super::pick;\npick! { yes { mod body; } }\n",
            ),
            ("child/body.rs", "pub unsafe fn a() {}\n"),
            ("inline/body.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[picks],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn uses_of_other_items_do_not_hide_macros() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { (yes { $($yes:tt)* }) => { $($yes)* }; }\n\npub fn spawn() {}\n",
        )],
    };
    let prelude = Library {
        name: "prelude",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! spawn { (yes { $($yes:tt)* }) => { $($yes)* }; }\n",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "#[macro_use]\nextern crate prelude;\n\nmod both;\nmod function;\n",
            ),
            (
                "both.rs",
                "use picks::pick;\nuse picks::spawn as pick;\npick! { yes { mod body; } }\n",
            ),
            ("both/body.rs", "pub unsafe fn a() {}\n"),
            (
                "function.rs",
                "use picks::spawn;\nspawn! { yes { mod body; } }\n",
            ),
            ("function/body.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[picks, prelude],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn macros_reached_two_ways_are_read_once() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            r"#[macro_export]
macro_rules! pick {
    (yes { $($yes:tt)* }) => { $($yes)* };
}

pub use crate::pick as alias;
",
        )],
    };
    let scan = scan_with(
        &[(
            "lib.rs",
            r"#[cfg(demo_custom)]
use picks::pick;
#[cfg(not(demo_custom))]
use picks::alias as pick;

pick! { yes { pub unsafe fn f() {} } }
",
        )],
        false,
        &[picks],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn macros_of_dependencies_named_like_keywords_are_expanded() {
    let keyword = Library {
        name: "async",
        features: &[],
        files: &[(
            "lib.rs",
            r"#[macro_export]
macro_rules! outer {
    ($name:ident) => { $crate::inner! { yes { mod $name; } } };
}

#[macro_export]
macro_rules! inner {
    (yes { $($body:tt)* }) => { $($body)* };
}
",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                "#[macro_use]\nextern crate r#async;\n\nouter! { first }\nr#async::outer! { second }\n",
            ),
            ("first.rs", "pub unsafe fn a() {}\n"),
            ("second.rs", "pub unsafe fn b() {}\n"),
        ],
        false,
        &[keyword],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn crate_paths_in_other_crates_macros_call_their_macros() {
    let runtime = Library {
        name: "runtime",
        features: &[],
        files: &[(
            "lib.rs",
            r"#[macro_export]
macro_rules! outer {
    ($name:ident) => { $crate::inner! { yes { mod $name; } } };
}

#[macro_export]
macro_rules! inner {
    (yes { $($body:tt)* }) => { $($body)* pub unsafe fn theirs() {} };
}
",
        )],
    };
    let scan = scan_with(
        &[
            ("lib.rs", "runtime::outer! { body }\n"),
            ("body.rs", "pub unsafe fn mine() {}\n"),
        ],
        false,
        &[runtime],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn crate_in_generated_exports_names_its_crate() {
    let tokio = Library {
        name: "tokio",
        features: &[],
        files: &[(
            "lib.rs",
            r"pub fn spawn() {}

macro_rules! define {
    () => {
        #[macro_export]
        macro_rules! runtime { () => { pub use $crate::spawn as start; }; }
    };
}

define!();
",
        )],
    };
    let scan = scan_with(
        &[(
            "lib.rs",
            "#[macro_use]\nextern crate tokio;\n\nruntime!();\n\npub fn run() {\n    start();\n}\n",
        )],
        false,
        &[tokio],
    );
    assert_eq!(scan.unscanned, []);
    let paths: Vec<_> = scan
        .calls
        .iter()
        .map(|(entry, sites)| {
            let path = Disallowed::bundled().unwrap().entries[*entry].path.clone();
            (path, sites.iter().map(|site| site.line).collect::<Vec<_>>())
        })
        .collect();
    assert_eq!(paths, [("tokio::spawn".to_owned(), vec![7])]);
}

#[test]
fn calls_of_crate_macros_with_input_are_expanded() {
    let scan = scan_files(&[
        (
            "lib.rs",
            r"#[macro_use]
mod macros;

feature! {
    #![unix]
    mod symlink;
    pub use self::symlink::symlink;
}

feature! {
    #![windows]
    mod windows;
}

feature! {
    #![demo_custom]
    mod missing;
}
",
        ),
        (
            "macros.rs",
            r"macro_rules! feature {
    (
        #![$meta:meta]
        $($item:item)*
    ) => {
        $(
            #[cfg($meta)]
            $item
        )*
        const _: fn() = || unsafe { std::thread::sleep(std::time::Duration::ZERO) };
    };
}
",
        ),
        (
            "symlink.rs",
            "pub unsafe fn symlink() {\n    std::thread::sleep(std::time::Duration::ZERO);\n}\n",
        ),
        ("windows.rs", "pub unsafe fn windows() {}\n"),
    ]);
    let missing = "module `missing` has no file: neither missing.rs nor missing/mod.rs";
    assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 17, missing)]);
    assert_eq!(scan.unsafe_code.functions, 1);
    assert_eq!(scan.unsafe_code.blocks, 3);
    let site = |file: &str, line, column| Site {
        file: file.to_owned(),
        line,
        column,
    };
    let rule = site("src/macros.rs", 10, 37);
    let mut sites: Vec<_> = scan.calls.values().flatten().cloned().collect();
    sites.sort();
    assert_eq!(
        sites,
        [
            rule.clone(),
            rule.clone(),
            rule,
            site("src/symlink.rs", 2, 5)
        ]
    );
}

#[test]
fn calls_the_scan_cannot_match_are_not_expanded() {
    let scan = scan_files(&[(
        "lib.rs",
        r"macro_rules! expr { ($e:expr) => { mod a; }; }
expr!(1 + 1);
macro_rules! unread { () => { ${ignore(x)} mod b; }; }
unread!();
macro_rules! nothing { (a) => { mod c; }; }
nothing!(b);
macro_rules! maybe { () => { mod d; }; }
#[cfg_attr(demo_custom, macro_use)]
mod inner {
    macro_rules! maybe { () => { mod e; }; }
}
maybe!();
",
    )]);
    let reason =
        |name: &str| format!("module `{name}` is declared in a macro body the scan can't read");
    let listed = [(1, "a"), (3, "b"), (5, "c"), (7, "d"), (10, "e")]
        .map(|(line, name)| unscanned("src/lib.rs", line, &reason(name)));
    assert_eq!(scan.unscanned, listed);
}

#[test]
fn expansion_stops_at_a_macro_that_calls_itself() {
    let scan = scan_files(&[(
        "lib.rs",
        "macro_rules! again { ($($t:tt)*) => { again!($($t)*); }; }\nagain!(unsafe {});\n",
    )]);
    let reason = "`again!` expands to more than 128 nested macro calls";
    assert_eq!(scan.unscanned, [unscanned("src/lib.rs", 1, reason)]);
    assert_eq!(scan.unsafe_code.in_macros, 1);
}

#[test]
fn expansion_stops_at_a_macro_that_calls_itself_twice() {
    let scan = scan_files(&[(
        "lib.rs",
        "macro_rules! fan { () => { fan!(); fan!(); }; }\nfan!();\n",
    )]);
    let deep = "`fan!` expands to more than 128 nested macro calls";
    let many = "the crate's macros expand to more than 10000 calls";
    assert_eq!(
        scan.unscanned,
        [
            unscanned("src/lib.rs", 1, deep),
            unscanned("src/lib.rs", 1, many)
        ]
    );
}

#[test]
fn modules_are_read_from_each_place_a_cfg_dependent_path_may_name() {
    let scan = scan_files(&[
        (
            "lib.rs",
            r#"#[cfg_attr(demo_a, path = "backend/a.rs")]
#[cfg_attr(demo_b, path = "backend/b.rs")]
mod backend;
#[cfg_attr(demo_a, path = "elsewhere.rs")]
mod both;
#[cfg_attr(demo_a, path = "nowhere.rs")]
mod neither;
#[cfg_attr(demo_a, path = "inline")]
mod inline {}
"#,
        ),
        ("backend/a.rs", "pub unsafe fn a() {}\n"),
        ("backend/b.rs", "pub unsafe fn b() {}\n"),
        ("elsewhere.rs", "pub unsafe fn c() {}\n"),
        ("both.rs", "pub unsafe fn d() {}\n"),
    ]);
    let neither = "module `neither` has no file at any place its #[path], which depends on an unknown cfg, may put it";
    let inline = "module `inline` has a #[path] that depends on an unknown cfg";
    assert_eq!(
        scan.unscanned,
        [
            unscanned("src/lib.rs", 7, neither),
            unscanned("src/lib.rs", 9, inline)
        ]
    );
    assert_eq!(scan.unsafe_code.functions, 4);
}

#[test]
fn macros_of_alternative_module_files_do_not_hide_each_other() {
    let scan = scan_files(&[
        (
            "lib.rs",
            "#[macro_use]\n#[cfg_attr(demo_a, path = \"other.rs\")]\nmod macros;\nroot!();\n",
        ),
        ("macros.rs", "macro_rules! root { () => { mod a; }; }\n"),
        ("other.rs", "macro_rules! root { () => { mod b; }; }\n"),
        ("a.rs", "pub unsafe fn a() {}\n"),
        ("b.rs", "pub unsafe fn b() {}\n"),
    ]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn calls_of_the_crates_own_macros_by_path_are_expanded() {
    let mut files = vec![
        (
            "lib.rs",
            r"#[macro_use]
mod macros;

mod uses;

crate::declare!(root_path);
self::declare!(self_path);
macros::local!(module_path);
crate::macros::local!(crate_path);
backend!(dollar_crate);
",
        ),
        (
            "macros.rs",
            r"#[macro_export]
macro_rules! declare {
    ($name:ident) => {
        mod $name;
    };
}

macro_rules! local {
    ($name:ident) => {
        mod $name;
    };
}

pub(crate) use local;

macro_rules! backend {
    ($name:ident) => {
        $crate::declare!($name);
    };
}
",
        ),
        (
            "uses.rs",
            "use crate::macros::local;\nuse crate::declare as renamed;\n\nlocal!(used);\nrenamed!(renamed);\n",
        ),
    ];
    let modules = [
        "root_path.rs",
        "self_path.rs",
        "module_path.rs",
        "crate_path.rs",
        "dollar_crate.rs",
        "uses/used.rs",
        "uses/renamed.rs",
    ];
    files.extend(modules.map(|module| (module, "pub unsafe fn f() {}\n")));
    let scan = scan_files(&files);
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, modules.len());
}

#[test]
fn macro_paths_through_names_bound_later_are_followed() {
    let picks = Library {
        name: "picks",
        features: &[],
        files: &[(
            "lib.rs",
            "#[macro_export]\nmacro_rules! pick { ($name:ident) => { mod $name; }; }\n",
        )],
    };
    let scan = scan_with(
        &[
            (
                "lib.rs",
                r"use later::pick;

pick!(body);
crate::declare!(early);
crate::later::local!(nested);

mod later {
    pub use picks::pick;

    macro_rules! local {
        ($name:ident) => {
            mod $name;
        };
    }

    pub(crate) use local;
}

#[macro_export]
macro_rules! declare {
    ($name:ident) => {
        mod $name;
    };
}
",
            ),
            ("body.rs", "pub unsafe fn a() {}\n"),
            ("early.rs", "pub unsafe fn b() {}\n"),
            ("nested.rs", "pub unsafe fn c() {}\n"),
        ],
        false,
        &[picks],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 3);
}

#[test]
fn the_crates_own_macros_by_path_are_read_as_each_definition() {
    let scan = scan_files(&[(
        "lib.rs",
        r"#[cfg(demo_custom)]
#[macro_export]
macro_rules! pick {
    ($name:ident) => {
        pub unsafe fn $name() {}
    };
}

#[cfg(not(demo_custom))]
#[macro_export]
macro_rules! pick {
    ($name:ident) => {
        pub fn $name() {}
        unsafe impl Send for crate::Handle {}
    };
}

pub struct Handle;

crate::pick!(by_path);
pick!(by_name);
",
    )]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(
        scan.unsafe_code,
        UnsafeCounts {
            functions: 2,
            impls: 2,
            ..UnsafeCounts::default()
        }
    );
}

#[test]
fn macros_one_macro_writes_by_each_call_are_each_a_definition() {
    let scan = scan_files(&[(
        "lib.rs",
        r"macro_rules! make {
    (yes $body:item) => {
        #[macro_export]
        macro_rules! pick {
            () => {
                $body
            };
        }
    };
}

#[cfg(demo_custom)]
make!(yes pub unsafe fn f() {});
#[cfg(not(demo_custom))]
make!(yes pub fn f() {});

mod user {
    crate::pick!();
}
",
    )]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn the_crates_own_macros_by_path_include_later_definitions() {
    let scan = scan_files(&[(
        "lib.rs",
        r"#[cfg(demo_custom)]
#[macro_export]
macro_rules! pick {
    () => {
        pub fn f() {}
    };
}

mod first {
    crate::pick!();
}

#[cfg(not(demo_custom))]
#[macro_export]
macro_rules! pick {
    () => {
        pub unsafe fn f() {}
    };
}

mod second {
    crate::pick!();
}
",
    )]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn calls_through_modules_of_one_path_read_each_reexport() {
    let scan = scan_files(&[(
        "lib.rs",
        r"#[macro_export]
macro_rules! safe {
    () => {
        pub fn f() {}
    };
}

#[macro_export]
macro_rules! danger {
    () => {
        pub unsafe fn f() {}
    };
}

#[cfg(demo_custom)]
mod choices {
    pub use crate::safe as pick;
}

mod first {
    crate::choices::pick!();
}

#[cfg(not(demo_custom))]
mod choices {
    pub use crate::danger as pick;
}

mod second {
    crate::choices::pick!();
}
",
    )]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn definitions_written_at_one_place_are_told_apart() {
    let scan = scan_files(&[(
        "lib.rs",
        r"macro_rules! make {
    (yes $body:item) => {
        #[macro_export]
        macro_rules! pick {
            () => {
                $body
            };
        }
    };
}

#[cfg(demo_custom)]
mod hidden {
    make!(yes pub unsafe fn f() {});
}

#[cfg(not(demo_custom))]
make!(yes pub fn f() {});

pick!();

mod user {
    crate::pick!();
}
",
    )]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn definitions_named_more_than_one_way_are_read_once() {
    let scan = scan_files(&[(
        "lib.rs",
        r"macro_rules! m {
    () => {
        pub unsafe fn f() {}
    };
}

mod a {
    pub(crate) use m;
}

mod b {
    pub(crate) use m;
}

#[cfg(demo_custom)]
use a::m as chosen;
#[cfg(not(demo_custom))]
use b::m as chosen;

chosen!();
",
    )]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

#[test]
fn definitions_keep_their_identity_across_readings() {
    let scan = scan_files(&[(
        "lib.rs",
        r"macro_rules! m {
    () => {
        pub unsafe fn f() {}
    };
}

mod a {
    pub(crate) use m;
}

#[cfg(demo_custom)]
use a::m as chosen;
#[cfg(not(demo_custom))]
use b::m as chosen;

chosen!();

mod b {
    pub(crate) use m;
}
",
    )]);
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
}

struct GraphCrate<'a> {
    name: &'a str,
    deps: &'a [(&'a str, usize)],
    files: &'a [(&'a str, &'a str)],
}

fn scan_graph(files: &[(&str, &str)], demo_deps: &[usize], others: &[GraphCrate]) -> Scan {
    let target = Target::parse("unix\ntarget_os=\"linux\"\n");
    let dirs: Vec<_> = others.iter().map(|krate| package(krate.files)).collect();
    let srcs: Vec<_> = dirs
        .iter()
        .map(|dir| dir.path().join("src/lib.rs"))
        .collect();
    let features = Features::default();
    let extern_maps: Vec<BTreeMap<String, String>> = others
        .iter()
        .map(|krate| {
            krate
                .deps
                .iter()
                .map(|(alias, target)| ((*alias).to_owned(), others[*target].name.to_owned()))
                .collect()
        })
        .collect();
    let mut crates: Vec<Crate> = others
        .iter()
        .enumerate()
        .map(|(index, krate)| Crate {
            name: krate.name,
            root: dirs[index].path(),
            src: &srcs[index],
            edition_2015: false,
            externs: &extern_maps[index],
            dependencies: krate
                .deps
                .iter()
                .map(|(alias, target)| ((*alias).to_owned(), *target))
                .collect(),
            cfg: Cfg {
                target: &target,
                features: &features,
                proc_macro: false,
            },
        })
        .collect();
    let demo_externs: BTreeMap<String, String> = demo_deps
        .iter()
        .map(|&index| (others[index].name.to_owned(), others[index].name.to_owned()))
        .collect();
    let demo_dependencies: BTreeMap<String, usize> = demo_deps
        .iter()
        .map(|&index| (others[index].name.to_owned(), index))
        .collect();
    let dir = package(files);
    let src = dir.path().join("src/lib.rs");
    let entry = crates.len();
    crates.push(Crate {
        name: "demo",
        root: dir.path(),
        src: &src,
        edition_2015: false,
        externs: &demo_externs,
        dependencies: demo_dependencies,
        cfg: Cfg {
            target: &target,
            features: &features,
            proc_macro: false,
        },
    });
    scan(&crates, entry, &Disallowed::bundled().unwrap())
}

#[test]
fn reexported_macros_from_another_crate_are_expanded() {
    let inner_files = [(
        "lib.rs",
        r"#[macro_export]
macro_rules! outer {
    ($name:ident) => { $crate::helper! { mod $name; } };
}

#[macro_export]
macro_rules! helper {
    ($($body:tt)*) => { $($body)* pub unsafe fn theirs() {} };
}
",
    )];
    let facade_files = [("lib.rs", "pub use aliased_inner::outer as select;\n")];
    let scan = scan_graph(
        &[
            (
                "lib.rs",
                "facade::select! { direct }\nuse facade::select;\nselect! { imported }\n",
            ),
            ("direct.rs", "pub unsafe fn a() {}\n"),
            ("imported.rs", "pub unsafe fn b() {}\n"),
        ],
        &[1],
        &[
            GraphCrate {
                name: "inner",
                deps: &[],
                files: &inner_files,
            },
            GraphCrate {
                name: "facade",
                deps: &[("aliased_inner", 0)],
                files: &facade_files,
            },
        ],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}

#[test]
fn use_dollar_crate_in_reexported_macro_resolves_to_defining_crate() {
    let inner_files = [(
        "lib.rs",
        r"#[macro_export]
macro_rules! make {
    ($name:ident, $body:block) => {
        use {$crate::helper, std::mem::forget};
        helper! { mod $name; }
        fn check() $body
    };
}

#[macro_export]
macro_rules! helper {
    ($($body:tt)*) => { $($body)* };
}
",
    )];
    let facade_files = [("lib.rs", "pub use inner::make;\n")];
    let scan = scan_graph(
        &[
            ("lib.rs", "facade::make! { generated, { forget(()); } }\n"),
            ("generated.rs", "pub unsafe fn a() {}\n"),
        ],
        &[1],
        &[
            GraphCrate {
                name: "inner",
                deps: &[],
                files: &inner_files,
            },
            GraphCrate {
                name: "facade",
                deps: &[("inner", 0)],
                files: &facade_files,
            },
        ],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 1);
    assert_eq!(scan.calls.len(), 1);
}

#[test]
fn reexported_module_macros_follow_into_third_crate() {
    let inner_files = [(
        "lib.rs",
        r"#[macro_export]
macro_rules! leaf {
    ($name:ident) => { mod $name; };
}
",
    )];
    let facade_files = [(
        "lib.rs",
        r"pub mod sub {
    pub use inner::leaf;
}

#[macro_export]
macro_rules! wrapper {
    ($name:ident) => { $crate::sub::leaf!($name); };
}
",
    )];
    let scan = scan_graph(
        &[
            (
                "lib.rs",
                "facade::wrapper!(wrapped);\nfacade::sub::leaf!(direct_sub);\n",
            ),
            ("wrapped.rs", "pub unsafe fn a() {}\n"),
            ("direct_sub.rs", "pub unsafe fn b() {}\n"),
        ],
        &[1],
        &[
            GraphCrate {
                name: "inner",
                deps: &[],
                files: &inner_files,
            },
            GraphCrate {
                name: "facade",
                deps: &[("inner", 0)],
                files: &facade_files,
            },
        ],
    );
    assert_eq!(scan.unscanned, []);
    assert_eq!(scan.unsafe_code.functions, 2);
}
