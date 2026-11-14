//! Narrowing a scan to the files git reports as changed.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::style::scan::Scan;

/// Keeps only the scanned files that differ from where the current branch
/// left `base`: changes committed since then, staged and unstaged changes,
/// and untracked files that aren't ignored. Deleted files are gone from the
/// scan already. With `base` of `HEAD`, only uncommitted changes remain.
pub(crate) fn retain_changed(scan: &mut Scan, base: &str) -> Result<()> {
    let top = git(&scan.root, &["rev-parse", "--show-toplevel"])
        .context("--changed needs a git repository")?;
    let top = path_from_git(trim_newline(&top))?
        .canonicalize()
        .context("cannot resolve the git repository root")?;
    let commit = format!("{base}^{{commit}}");
    let base = git(
        &top,
        &["rev-parse", "--verify", "--end-of-options", &commit],
    )
    .with_context(|| format!("unknown --changed base: {base}"))?;
    let base = String::from_utf8(base).context("git printed a non-UTF-8 commit")?;
    let merge_base = git(&top, &["merge-base", base.trim_end(), "HEAD"])?;
    let merge_base = String::from_utf8(merge_base).context("git printed a non-UTF-8 commit")?;

    let diff = git(
        &top,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=d",
            merge_base.trim_end(),
            "--",
        ],
    )?;
    let untracked = git(&top, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let changed = diff
        .split(|byte| *byte == 0)
        .chain(untracked.split(|byte| *byte == 0))
        .filter(|path| !path.is_empty())
        .map(|path| path_from_git(path).map(|path| top.join(path)))
        .collect::<Result<BTreeSet<_>>>()?;
    scan.files.retain(|path| changed.contains(path));
    Ok(())
}

/// Runs git in `dir` and returns its standard output.
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn trim_newline(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

#[cfg(unix)]
fn path_from_git(bytes: &[u8]) -> Result<PathBuf> {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    Ok(PathBuf::from(OsStr::from_bytes(bytes)))
}

/// Git prints paths as UTF-8 outside Unix.
#[cfg(not(unix))]
fn path_from_git(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::str::from_utf8(bytes).context("git printed a non-UTF-8 path")?,
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn changed(path: &Path, base: &str) -> Result<Vec<String>> {
        let mut scan = Scan::discover(path)?;
        retain_changed(&mut scan, base)?;
        Ok(scan
            .files
            .iter()
            .map(|path| scan.relative_path(path))
            .collect())
    }

    /// A repository with `main` at one commit of four files, and a `topic`
    /// branch checked out from it.
    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "--quiet", "--initial-branch=main"]);
        fs::create_dir(root.join("src")).unwrap();
        for name in [
            "src/kept.rs",
            "src/edited.rs",
            "src/staged.rs",
            "src/gone.rs",
        ] {
            fs::write(root.join(name), "fn example() {}\n").unwrap();
        }
        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "--quiet", "-m", "initial"]);
        run(root, &["checkout", "--quiet", "-b", "topic"]);
        dir
    }

    #[test]
    fn keeps_staged_unstaged_and_untracked_files() {
        let dir = repository();
        let root = dir.path();
        fs::write(root.join("src/edited.rs"), "fn edited() {}\n").unwrap();
        fs::write(root.join("src/staged.rs"), "fn staged() {}\n").unwrap();
        run(root, &["add", "src/staged.rs"]);
        fs::write(root.join("src/untracked.rs"), "fn new() {}\n").unwrap();
        fs::write(root.join("ignored.rs"), "fn ignored() {}\n").unwrap();
        fs::write(root.join("notes.txt"), "not rust\n").unwrap();
        fs::remove_file(root.join("src/gone.rs")).unwrap();

        assert_eq!(
            changed(root, "HEAD").unwrap(),
            ["src/edited.rs", "src/staged.rs", "src/untracked.rs"]
        );
        // Paths stay relative to the scan root, below the repository root.
        assert_eq!(
            changed(&root.join("src"), "HEAD").unwrap(),
            ["edited.rs", "staged.rs", "untracked.rs"]
        );
        assert_eq!(
            changed(&root.join("src/edited.rs"), "HEAD").unwrap(),
            ["edited.rs"]
        );
        assert!(
            changed(&root.join("src/kept.rs"), "HEAD")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_base_adds_commits_since_the_branch_left_it() {
        let dir = repository();
        let root = dir.path();
        fs::write(root.join("src/edited.rs"), "fn committed() {}\n").unwrap();
        run(root, &["commit", "--quiet", "-am", "topic change"]);
        run(root, &["checkout", "--quiet", "main"]);
        fs::write(root.join("src/kept.rs"), "fn on_main() {}\n").unwrap();
        run(root, &["commit", "--quiet", "-am", "main change"]);
        run(root, &["checkout", "--quiet", "topic"]);
        fs::write(root.join("src/staged.rs"), "fn uncommitted() {}\n").unwrap();

        assert_eq!(changed(root, "HEAD").unwrap(), ["src/staged.rs"]);
        assert_eq!(
            changed(root, "main").unwrap(),
            ["src/edited.rs", "src/staged.rs"]
        );
    }

    #[test]
    fn rejects_unknown_bases_and_directories_outside_git() {
        let dir = repository();
        let error = changed(dir.path(), "no-such-branch").unwrap_err();
        assert!(format!("{error:#}").contains("unknown --changed base: no-such-branch"));
        let error = changed(dir.path(), "--all").unwrap_err();
        assert!(format!("{error:#}").contains("unknown --changed base"));

        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("lib.rs"), "fn example() {}\n").unwrap();
        let error = changed(outside.path(), "HEAD").unwrap_err();
        assert!(format!("{error:#}").contains("--changed needs a git repository"));
    }
}
