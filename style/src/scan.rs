use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;

pub(crate) struct Scan {
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
}

impl Scan {
    pub fn discover(path: &Path) -> Result<Self> {
        let target = path
            .canonicalize()
            .with_context(|| format!("cannot access {}", path.display()))?;
        if target.is_file() {
            if target.extension().is_none_or(|ext| ext != "rs") {
                bail!("{} is not a Rust source file", target.display());
            }
            let root = target
                .parent()
                .context("source file has no parent directory")?;
            return Ok(Self {
                root: root.to_path_buf(),
                files: vec![target],
            });
        }
        if !target.is_dir() {
            bail!("{} is not a file or directory", target.display());
        }

        let mut builder = WalkBuilder::new(&target);
        builder
            .hidden(true)
            .require_git(false)
            .follow_links(false)
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !entry.file_type().is_some_and(|kind| kind.is_dir())
                    || !matches!(
                        entry.file_name().to_str(),
                        Some(".git" | "target" | "node_modules")
                    )
            });
        let mut files = Vec::new();
        for entry in builder.build() {
            let entry = entry.context("failed to walk the target directory")?;
            if entry.file_type().is_some_and(|kind| kind.is_file())
                && entry.path().extension().is_some_and(|ext| ext == "rs")
            {
                files.push(entry.into_path());
            }
        }
        files.sort();
        if files.is_empty() {
            bail!("no Rust source files found in {}", target.display());
        }
        Ok(Self {
            root: target,
            files,
        })
    }

    pub fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn finds_sources_and_respects_ignores() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("target/generated")).unwrap();
        fs::create_dir_all(dir.path().join("ignored")).unwrap();
        fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::write(dir.path().join("src/lib.rs"), "fn example() {}\n").unwrap();
        fs::write(dir.path().join("target/generated/build.rs"), "").unwrap();
        fs::write(dir.path().join("ignored/not_checked.rs"), "").unwrap();
        fs::write(dir.path().join(".hidden/not_checked.rs"), "").unwrap();

        let scan = Scan::discover(dir.path()).unwrap();
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.relative_path(&scan.files[0]), "src/lib.rs");
    }

    #[test]
    fn accepts_a_single_rust_file_and_rejects_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let rust = dir.path().join("example.rs");
        let other = dir.path().join("example.txt");
        fs::write(&rust, "fn example() {}\n").unwrap();
        fs::write(&other, "").unwrap();
        let scan = Scan::discover(&rust).unwrap();
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.relative_path(&scan.files[0]), "example.rs");
        assert!(Scan::discover(&other).is_err());
    }
}
