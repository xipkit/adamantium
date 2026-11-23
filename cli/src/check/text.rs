use std::{
    cmp::Reverse,
    collections::BTreeMap,
    io::{self, Write},
};

use super::{Report, Status};

pub(super) fn findings_label(count: usize) -> &'static str {
    if count == 1 { "finding" } else { "findings" }
}

pub(super) fn print_text(
    stderr: &mut impl Write,
    report: &Report,
    summary: bool,
) -> io::Result<()> {
    for error in &report.errors {
        if error.file.is_none() {
            writeln!(stderr, "error: {}", error.message)?;
        }
    }
    let note = left_out_note(report);
    let count = report.findings.len();
    let label = findings_label(count);
    match report.status {
        Status::Clean => writeln!(stderr, "adamantium: clean{note}"),
        Status::Findings if summary => {
            writeln!(stderr, "adamantium: {count} {label}{note}")?;
            writeln!(stderr, "Findings by rule:")?;
            for (rule, count) in
                ranked_counts(report.findings.iter().map(|finding| finding.rule.as_str()))
            {
                writeln!(stderr, "  {count:>5}  {rule}")?;
            }
            writeln!(stderr, "Top hotspot files:")?;
            let files = ranked_counts(report.findings.iter().map(|finding| finding.file.as_str()));
            for (file, count) in files.into_iter().take(10) {
                writeln!(stderr, "  {count:>5}  {file}")?;
            }
            Ok(())
        }
        Status::Findings => {
            let mut files: Vec<&str> = report
                .findings
                .iter()
                .map(|finding| finding.file.as_str())
                .collect();
            files.sort_unstable();
            files.dedup();
            writeln!(
                stderr,
                "adamantium: {count} {label} in {}{note}",
                files.join(", ")
            )
        }
        Status::Error => writeln!(
            stderr,
            "adamantium: the project did not build, so the rules were not all checked"
        ),
    }
}

fn ranked_counts<'a>(names: impl Iterator<Item = &'a str>) -> Vec<(&'a str, usize)> {
    let mut counts = BTreeMap::new();
    for name in names {
        *counts.entry(name).or_insert(0) += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort_unstable_by_key(|&(name, count)| (Reverse(count), name));
    counts
}

fn left_out_note(report: &Report) -> String {
    let skipped: usize = report.skipped.values().sum();
    let baselined: usize = report.baselined.values().sum();
    let mut parts = Vec::new();
    if skipped > 0 {
        parts.push(format!("{skipped} skipped by the skip list in Cargo.toml"));
    }
    if baselined > 0 {
        parts.push(format!("{baselined} in the baseline"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}
