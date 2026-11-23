use std::fs;

use super::{
    baseline::portable_path,
    diagnostic::{Code, Diagnostic, Span, SpanLine},
    *,
};
use crate::lints::Lint;

fn diagnostic(file: &str, level: &str, code: Option<&str>, message: &str) -> Diagnostic {
    Diagnostic {
        message: message.to_owned(),
        level: level.to_owned(),
        code: code.map(|code| Code {
            code: code.to_owned(),
        }),
        rendered: None,
        spans: vec![Span {
            file_name: file.to_owned(),
            line_start: 7,
            column_start: 3,
            is_primary: true,
            text: vec![SpanLine {
                text: "    let value = option.unwrap();  ".to_owned(),
            }],
            expansion: None,
        }],
    }
}

fn classify_diagnostics(diagnostics: Vec<Diagnostic>) -> Report {
    classify_against(None, diagnostics)
}

fn classify_against(baseline: Option<Baseline>, diagnostics: Vec<Diagnostic>) -> Report {
    let config = Config::default();
    classify_with_config(&config, baseline, diagnostics)
}

fn classify_with_config(
    config: &Config,
    mut baseline: Option<Baseline>,
    diagnostics: Vec<Diagnostic>,
) -> Report {
    let root = Path::new("project");
    let rules = lints::selected(false).map(Lint::qualified).collect();
    let setting = Setting {
        root,
        clippy_config: Path::new("generated/clippy.toml"),
        reasons: &lints::Reasons::parse(lints::CLIPPY_TOML),
        config,
        rules: &rules,
        echo: false,
    };
    let mut sorted = Sorted {
        baseline: baseline.as_mut(),
        ..Sorted::default()
    };
    for diagnostic in diagnostics {
        classify(diagnostic, &setting, &mut sorted);
    }
    Report {
        schema_version: SCHEMA_VERSION,
        root: root.to_path_buf(),
        status: Status::Clean,
        findings: sorted.findings,
        skipped: sorted.skipped,
        baselined: sorted.baselined,
        baseline_unmatched: sorted.baseline.as_deref().map(Baseline::unmatched),
        warnings: sorted.warnings,
        errors: sorted.errors,
    }
}

fn expanded_macro(file: &str, line: u64, column: u64, text: &str) -> Diagnostic {
    let mut span = serde_json::json!({
        "file_name": file,
        "line_start": line,
        "column_start": column,
        "is_primary": false,
        "text": [{ "text": text }],
        "expansion": null,
    });
    for line in [808, 780, 765] {
        span = serde_json::json!({
            "file_name": "/registry/macros/src/lib.rs",
            "line_start": line,
            "column_start": 9,
            "is_primary": true,
            "text": [{ "text": "$crate::recursive!(@next)" }],
            "expansion": {
                "span": span,
                "macro_decl_name": "recursive!",
                "def_site_span": null,
            },
        });
    }
    serde_json::from_value(serde_json::json!({
        "message": "use of a disallowed method",
        "level": "warning",
        "code": { "code": "clippy::disallowed_methods" },
        "spans": [span],
        "rendered": null,
    }))
    .unwrap()
}

#[test]
fn macro_findings_use_call_sites_and_deduplicate_expansions() {
    let source = "    recursive!();";
    let mut another_expansion = expanded_macro("src/lib.rs", 10, 5, source);
    another_expansion.spans.first_mut().unwrap().line_start = 774;
    let report = classify_diagnostics(vec![
        expanded_macro("src/lib.rs", 10, 5, source),
        expanded_macro("src/lib.rs", 10, 5, source),
        another_expansion,
        expanded_macro("src/lib.rs", 20, 5, source),
        expanded_macro("src/lib.rs", 20, 25, source),
        expanded_macro("src/other.rs", 10, 5, source),
    ]);
    let locations: Vec<_> = report
        .findings
        .iter()
        .map(|finding| (finding.file.as_str(), finding.line, finding.column))
        .collect();
    assert_eq!(
        locations,
        [
            ("src/lib.rs", Some(10), Some(5)),
            ("src/lib.rs", Some(20), Some(5)),
            ("src/lib.rs", Some(20), Some(25)),
            ("src/other.rs", Some(10), Some(5)),
        ]
    );
    for finding in report.findings {
        assert_eq!(finding.source.as_deref(), Some("recursive!();"));
    }
}

#[test]
fn deep_macro_backtraces_are_not_dropped_by_the_json_parser() {
    let mut span = serde_json::json!({
        "file_name": "src/lib.rs",
        "line_start": 2,
        "column_start": 5,
        "is_primary": false,
        "text": [{ "text": "    deep!();" }],
        "expansion": null,
    });
    for line in 1..=70 {
        span = serde_json::json!({
            "file_name": "/registry/macros/src/lib.rs",
            "line_start": line,
            "column_start": 9,
            "is_primary": true,
            "text": [],
            "expansion": { "span": span },
        });
    }
    let line = serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "message": "use of a disallowed method",
            "level": "warning",
            "code": { "code": "clippy::disallowed_methods" },
            "spans": [span],
            "rendered": null,
        },
    })
    .to_string();
    let parsed = parse_cargo_line(&line).unwrap();
    let report = classify_diagnostics(vec![parsed.message.unwrap()]);
    assert_eq!(report.findings.len(), 1);
    let finding = report.findings.first().unwrap();
    assert_eq!(finding.file, "src/lib.rs");
    assert_eq!(finding.line, Some(2));
    assert_eq!(finding.column, Some(5));
    assert_eq!(finding.source.as_deref(), Some("deep!();"));
    assert!(parse_cargo_line(&format!("{line} trailing data")).is_err());
}

#[test]
fn macro_baselines_match_caller_source_and_count_distinct_invocations() {
    let baseline = baseline(
        "/elsewhere/project",
        serde_json::json!([earlier(
            "clippy::disallowed_methods",
            "src/lib.rs",
            Some("recursive!();"),
        )]),
    );
    let report = classify_against(
        Some(baseline),
        vec![
            expanded_macro("project/src/lib.rs", 40, 9, "        recursive!();"),
            expanded_macro("project/src/lib.rs", 40, 9, "        recursive!();"),
            expanded_macro("project/src/lib.rs", 50, 9, "        recursive!();"),
            expanded_macro("src/lib.rs", 60, 5, "    recursive!(edited);"),
            expanded_macro("src/other.rs", 40, 5, "    recursive!();"),
        ],
    );
    let locations: Vec<_> = report
        .findings
        .iter()
        .map(|finding| (finding.file.as_str(), finding.line))
        .collect();
    assert_eq!(
        locations,
        [
            ("project/src/lib.rs", Some(50)),
            ("src/lib.rs", Some(60)),
            ("src/other.rs", Some(40)),
        ]
    );
    assert_eq!(
        report.baselined,
        BTreeMap::from([("clippy::disallowed_methods".to_owned(), 1)])
    );
    assert_eq!(report.baseline_unmatched, Some(0));
}

#[test]
fn macro_skip_paths_and_exceptions_match_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
            dir.path().join("Cargo.toml"),
            "[package.metadata.adamantium]\nskip = [\"tests/\"]\n\n[package.metadata.adamantium.exceptions]\ndisallowed_methods = [\"src/allowed.rs\"]\n",
        )
        .unwrap();
    let config = Config::load(dir.path()).unwrap();
    let report = classify_with_config(
        &config,
        None,
        vec![
            expanded_macro("project/tests/cases.rs", 10, 5, "    recursive!();"),
            expanded_macro("project/tests/cases.rs", 10, 5, "    recursive!();"),
            expanded_macro("src/allowed.rs", 10, 5, "    recursive!();"),
            expanded_macro("src/lib.rs", 10, 5, "    recursive!();"),
        ],
    );
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings.first().unwrap().file, "src/lib.rs");
    assert_eq!(
        report.skipped,
        BTreeMap::from([("clippy::disallowed_methods".to_owned(), 2)])
    );
}

#[test]
fn recorded_tokio_expansion_matches_the_rendered_location() {
    let diagnostic: Diagnostic =
        serde_json::from_str(include_str!("../../tests/diagnostics/tokio-select.json")).unwrap();
    let rendered = diagnostic.rendered.as_ref().unwrap().clone();
    let config = Config::default();
    let rules = HashSet::from(["clippy::disallowed_macros".to_owned()]);
    let setting = Setting {
        root: Path::new("project"),
        clippy_config: Path::new("generated/clippy.toml"),
        reasons: &lints::Reasons::parse(lints::CLIPPY_TOML),
        config: &config,
        rules: &rules,
        echo: false,
    };
    let mut sorted = Sorted::default();
    classify(diagnostic, &setting, &mut sorted);
    assert_eq!(sorted.findings.len(), 1);
    let finding = sorted.findings.first().unwrap();
    assert_eq!(finding.file, "src/lib.rs");
    assert_eq!(finding.line, Some(2));
    assert_eq!(finding.column, Some(5));
    assert_eq!(finding.source.as_deref(), Some("tokio::select! {"));
    assert!(rendered.contains("--> src/lib.rs:2:5"), "{rendered}");
}

#[test]
fn config_warnings_preserve_distinct_messages_and_deduplicate_targets() {
    let report = classify_diagnostics(vec![
        diagnostic("generated/clippy.toml", "warning", None, "unresolved path"),
        diagnostic("generated/clippy.toml", "warning", None, "unresolved path"),
        diagnostic("generated/clippy.toml", "warning", None, "unknown key"),
        diagnostic(
            "generated/clippy.toml",
            "warning",
            Some("future_config_warning"),
            "a config warning with a code",
        ),
    ]);
    let report = serde_json::to_value(report).unwrap();
    assert_eq!(
        report["warnings"],
        serde_json::json!([
            { "message": "unresolved path", "file": "clippy.toml", "line": 7 },
            { "message": "unknown key", "file": "clippy.toml", "line": 7 },
            { "message": "a config warning with a code", "file": "clippy.toml", "line": 7 },
        ])
    );
    assert_eq!(report["findings"], serde_json::json!([]));
    assert_eq!(report["errors"], serde_json::json!([]));
}

fn spanless(level: &str, code: Option<&str>, message: &str) -> Diagnostic {
    let mut diagnostic = diagnostic("", level, code, message);
    diagnostic.spans.clear();
    diagnostic
}

#[test]
fn spanless_lint_warnings_deduplicate_by_code_and_message() {
    let report = classify_diagnostics(vec![
        spanless("warning", Some("E0602"), "lint unavailable"),
        spanless("warning", Some("E0602"), "lint unavailable"),
        spanless("warning", Some("E0602"), "another lint unavailable"),
        spanless(
            "warning",
            Some("renamed_and_removed_lints"),
            "lint unavailable",
        ),
        spanless(
            "warning",
            Some("renamed_and_removed_lints"),
            "lint unavailable",
        ),
    ]);
    let report = serde_json::to_value(report).unwrap();
    assert_eq!(
        report["warnings"],
        serde_json::json!([
            { "message": "lint unavailable", "file": null, "line": null },
            { "message": "another lint unavailable", "file": null, "line": null },
            { "message": "lint unavailable", "file": null, "line": null },
        ])
    );
    assert_eq!(report["findings"], serde_json::json!([]));
    assert_eq!(report["errors"], serde_json::json!([]));
}

#[test]
fn other_spanless_diagnostics_are_dropped() {
    let report = classify_diagnostics(vec![
        spanless("warning", None, "2 warnings emitted"),
        spanless("error", None, "aborting due to 2 previous errors"),
        spanless("warning", Some("unused_imports"), "unused import"),
        spanless("warning", Some("clippy::unwrap_used"), "unwrap used"),
        spanless("error", Some("E0602"), "unknown lint"),
        spanless("note", Some("E0602"), "unknown lint"),
        spanless("error", Some("renamed_and_removed_lints"), "removed lint"),
        spanless("note", Some("renamed_and_removed_lints"), "removed lint"),
    ]);
    assert!(report.warnings.is_empty());
    assert!(report.errors.is_empty());
    assert!(report.findings.is_empty());
}

#[test]
fn only_primary_spans_in_the_generated_config_produce_warnings() {
    let mut without_primary = diagnostic("generated/clippy.toml", "warning", None, "summary");
    without_primary.spans.clear();
    let mut secondary = diagnostic("generated/clippy.toml", "warning", None, "secondary");
    secondary.spans.first_mut().unwrap().is_primary = false;
    let report = classify_diagnostics(vec![
        diagnostic(
            "src/lib.rs",
            "warning",
            Some("unused_imports"),
            "unused import",
        ),
        diagnostic("project/clippy.toml", "warning", None, "another config"),
        diagnostic("generated/clippy.toml", "note", None, "a note"),
        without_primary,
        secondary,
    ]);
    assert!(report.warnings.is_empty());
    assert!(report.errors.is_empty());
    assert!(report.findings.is_empty());
}

#[cfg(unix)]
#[test]
fn clippy_config_is_named_by_its_canonical_path() {
    let real = tempfile::tempdir().unwrap();
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("conf");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    let (path, toml) = write_clippy_config(&link, false, false).unwrap();
    assert_eq!(
        path,
        real.path().canonicalize().unwrap().join("clippy.toml")
    );
    assert_eq!(toml, lints::CLIPPY_TOML);
    assert_eq!(fs::read_to_string(&path).unwrap(), lints::CLIPPY_TOML);
}

#[test]
fn config_errors_keep_their_level_and_use_a_stable_file_name() {
    let report = classify_diagnostics(vec![
        diagnostic("generated/clippy.toml", "warning", None, "config problem"),
        diagnostic("generated/clippy.toml", "error", None, "config problem"),
        diagnostic("src/lib.rs", "error", Some("E0001"), "compile error"),
        diagnostic(
            "src/lib.rs",
            "warning",
            Some("clippy::unwrap_used"),
            "unwrap used",
        ),
    ]);
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(
        report.errors,
        vec![
            Problem {
                message: "config problem".to_owned(),
                file: Some("clippy.toml".to_owned()),
                line: Some(7),
            },
            Problem {
                message: "compile error".to_owned(),
                file: Some("src/lib.rs".to_owned()),
                line: Some(7),
            },
        ]
    );
    assert_eq!(report.findings.len(), 1);
    let finding = report.findings.first().unwrap();
    assert_eq!(finding.rule, "clippy::unwrap_used");
    assert_eq!(
        finding.source.as_deref(),
        Some("let value = option.unwrap();")
    );
}

fn unwrap_at(file: &str, line: u64, text: &str, message: &str) -> Diagnostic {
    let mut diagnostic = diagnostic(file, "warning", Some("clippy::unwrap_used"), message);
    let span = diagnostic.spans.first_mut().unwrap();
    span.line_start = line;
    span.text = vec![SpanLine {
        text: text.to_owned(),
    }];
    diagnostic
}

fn baseline(root: &str, findings: serde_json::Value) -> Baseline {
    Baseline::parse(
        serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "root": root,
            "status": "findings",
            "findings": findings,
        })
        .to_string(),
    )
    .unwrap()
}

fn earlier(rule: &str, file: &str, source: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "rule": rule,
        "message": "an older wording",
        "file": file,
        "line": 1,
        "column": 1,
        "source": source,
    })
}

#[test]
fn baseline_matches_by_rule_path_and_trimmed_line_text() {
    let baseline = baseline(
        "/elsewhere/project",
        serde_json::json!([
            earlier(
                "clippy::unwrap_used",
                "src/lib.rs",
                Some("let a = x.unwrap();")
            ),
            earlier(
                "clippy::unwrap_used",
                "/elsewhere/project/src/main.rs",
                Some("let b = y.unwrap();"),
            ),
            earlier(
                "clippy::unwrap_used",
                "src/gone.rs",
                Some("let c = z.unwrap();")
            ),
        ]),
    );
    let report = classify_against(
        Some(baseline),
        vec![
            unwrap_at(
                "src/lib.rs",
                40,
                "        let a = x.unwrap();",
                "new wording",
            ),
            unwrap_at("project/src/main.rs", 3, "let b = y.unwrap();", "unwrap"),
            unwrap_at("src/lib.rs", 41, "let a = x.unwrap(); // edited", "unwrap"),
            unwrap_at("src/other.rs", 1, "let a = x.unwrap();", "unwrap"),
        ],
    );
    let new: Vec<(&str, u64)> = report
        .findings
        .iter()
        .map(|finding| (finding.file.as_str(), finding.line.unwrap()))
        .collect();
    assert_eq!(new, [("src/lib.rs", 41), ("src/other.rs", 1)]);
    assert_eq!(
        report.baselined,
        BTreeMap::from([("clippy::unwrap_used".to_owned(), 2)])
    );
    assert_eq!(report.baseline_unmatched, Some(1));
}

#[test]
fn baseline_counts_findings_that_share_a_line_text() {
    let line = Some("v.unwrap();");
    let baseline = baseline(
        "/project",
        serde_json::json!([
            earlier("clippy::unwrap_used", "src/lib.rs", line),
            earlier("clippy::unwrap_used", "src/lib.rs", line),
        ]),
    );
    let report = classify_against(
        Some(baseline),
        vec![
            unwrap_at("src/lib.rs", 1, "v.unwrap();", "unwrap"),
            unwrap_at("src/lib.rs", 2, "v.unwrap();", "unwrap"),
            unwrap_at("src/lib.rs", 3, "v.unwrap();", "unwrap"),
        ],
    );
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings.first().unwrap().line, Some(3));
    assert_eq!(report.baseline_unmatched, Some(0));
}

#[test]
fn baseline_only_matches_rule_findings() {
    let baseline = baseline(
        "/project",
        serde_json::json!([earlier(
            "clippy::unwrap_used",
            "src/lib.rs",
            Some("let value = option.unwrap();")
        ),]),
    );
    let report = classify_against(
        Some(baseline),
        vec![
            diagnostic("src/lib.rs", "error", Some("E0001"), "compile error"),
            diagnostic(
                "src/lib.rs",
                "warning",
                Some("clippy::unwrap_used"),
                "unwrap",
            ),
        ],
    );
    assert!(report.findings.is_empty());
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.skipped, BTreeMap::new());
    assert_eq!(report.baseline_unmatched, Some(0));

    let report = classify_diagnostics(vec![diagnostic(
        "src/lib.rs",
        "warning",
        Some("clippy::unwrap_used"),
        "unwrap",
    )]);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.baselined, BTreeMap::new());
    assert_eq!(report.baseline_unmatched, None);
}

#[test]
fn baseline_rejects_reports_it_cannot_trust() {
    let report = |version: serde_json::Value, status: &str| {
        serde_json::json!({
            "schema_version": version,
            "root": "/project",
            "status": status,
            "findings": [],
        })
        .to_string()
    };
    for (text, expected) in [
        (report(2.into(), "findings"), "it has schema version 2"),
        (report(4.into(), "clean"), "it has schema version 4"),
        ("{}".to_owned(), "it has no schema version"),
        ("not json".to_owned(), "it is not a JSON report"),
        (
            serde_json::json!({ "schema_version": SCHEMA_VERSION }).to_string(),
            "it is not a check report",
        ),
        (
            report(SCHEMA_VERSION.into(), "error"),
            "the project did not build",
        ),
    ] {
        let error = Baseline::parse(text.clone()).unwrap_err().to_string();
        assert!(error.starts_with(expected), "{text}: {error}");
    }
    assert_eq!(
        Baseline::parse(report(SCHEMA_VERSION.into(), "clean"))
            .unwrap()
            .unmatched(),
        0
    );
}

#[test]
fn baseline_paths_match_across_platforms() {
    for (root, file) in [
        (r"C:\work\project", r"src\lib.rs"),
        (r"C:\work\project", r"C:\work\project\src\lib.rs"),
        (r"C:\work\project\", r"C:\work\project\src\lib.rs"),
        ("/home/ci/project", "/home/ci/project/src/lib.rs"),
        ("/home/ci/project/", "src/lib.rs"),
    ] {
        assert_eq!(
            portable_path(Path::new(root), file),
            "src/lib.rs",
            "{root} {file}"
        );
    }
    assert_eq!(
        portable_path(
            Path::new("/home/ci/project"),
            "/home/ci/project-two/src/lib.rs"
        ),
        "/home/ci/project-two/src/lib.rs"
    );

    let baseline = baseline(
        r"C:\work\project",
        serde_json::json!([
            earlier(
                "clippy::unwrap_used",
                r"src\lib.rs",
                Some("let a = x.unwrap();")
            ),
            earlier(
                "clippy::unwrap_used",
                r"C:\work\project\src\main.rs",
                Some("let b = y.unwrap();"),
            ),
        ]),
    );
    let report = classify_against(
        Some(baseline),
        vec![
            unwrap_at("src/lib.rs", 1, "let a = x.unwrap();", "unwrap"),
            unwrap_at("project/src/main.rs", 1, "let b = y.unwrap();", "unwrap"),
        ],
    );
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.baseline_unmatched, Some(0));
}

#[test]
fn findings_carry_rule_and_path_specific_remediation() {
    let report = classify_diagnostics(vec![
        diagnostic(
            "src/lib.rs",
            "warning",
            Some("clippy::disallowed_methods"),
            "use of a disallowed method `tokio::spawn`",
        ),
        diagnostic(
            "src/lib.rs",
            "warning",
            Some("clippy::disallowed_methods"),
            "use of a disallowed method `std::thread::Builder::spawn`",
        ),
        diagnostic(
            "src/lib.rs",
            "warning",
            Some("clippy::disallowed_methods"),
            "use of a disallowed method `std::mem::forget`",
        ),
        diagnostic(
            "src/lib.rs",
            "warning",
            Some("clippy::disallowed_types"),
            "use of a disallowed type `std::sync::Mutex`",
        ),
        diagnostic(
            "src/lib.rs",
            "warning",
            Some("clippy::disallowed_types"),
            "use of a disallowed type `tokio::sync::RwLock`",
        ),
        diagnostic(
            "src/lib.rs",
            "warning",
            Some("clippy::unwrap_used"),
            "used `unwrap()` on an `Option` value",
        ),
    ]);
    let remediations: Vec<Option<&str>> = report
        .findings
        .iter()
        .map(|finding| finding.remediation.as_deref())
        .collect();
    assert_eq!(
        remediations,
        [
            Some(
                "An unstructured task; spawn inside adamantium::Tasks. Review https://github.com/xipkit/adamantium/blob/main/docs/migrating.md#tasks."
            ),
            Some(
                "An unstructured thread; use adamantium::Tasks or adamantium::blocking. Review https://github.com/xipkit/adamantium/blob/main/docs/migrating.md#blocking-work."
            ),
            Some("Leaks. Run cargo adamantium explain clippy::disallowed_methods."),
            Some(
                "Use adamantium::Actor, which never holds its lock across an await or inside another actor. Review https://github.com/xipkit/adamantium/blob/main/docs/migrating.md#shared-state."
            ),
            Some(
                "An async lock can be held across an await; use adamantium::Actor. Review https://github.com/xipkit/adamantium/blob/main/docs/migrating.md#rwlock."
            ),
            Some(
                "Return or handle None/Err. Propagate Result with ? so an enclosing adamantium::Tasks scope can observe failure. Run cargo adamantium explain clippy::unwrap_used."
            ),
        ]
    );

    let mut without_guidance = report.findings.first().unwrap().clone();
    without_guidance.remediation = None;
    let encoded = serde_json::to_value(&without_guidance).unwrap();
    assert!(encoded.get("remediation").is_none(), "{encoded}");
}
