use std::path::{Component, Path, PathBuf};

/// Resolves `.` and `..` segments without touching the file system. A `..`
/// with no directory before it to leave is kept.
pub(crate) fn lexical(path: &Path) -> PathBuf {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if matches!(parts.last(), Some(Component::Normal(_))) => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}
