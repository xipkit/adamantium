//! The dependencies of a workspace, from `cargo metadata`, and the host
//! target they are resolved for, from rustc.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    env,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// When a dependency's code runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Linked into the workspace's programs.
    Runtime,
    /// Runs only while compiling: a build dependency, a procedural macro, or
    /// something only they depend on.
    Build,
}

/// A dependency and what is needed to read its library's source.
#[derive(Debug)]
pub struct Dependency {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub kind: Kind,
    /// The directory holding the dependency's `Cargo.toml`.
    pub root: PathBuf,
    pub lib: Option<Lib>,
    /// The features Cargo resolved for it.
    pub features: BTreeSet<String>,
    /// The name each of its normal dependencies goes by in its code, mapped
    /// to that dependency's library name.
    pub externs: BTreeMap<String, String>,
}

/// A dependency's library target.
#[derive(Debug)]
pub struct Lib {
    /// The crate name its code and other crates call it by.
    pub name: String,
    pub src_path: PathBuf,
    pub edition: String,
    pub proc_macro: bool,
}

/// The dependencies of the workspace at `root`, resolved for `target`, in
/// order of name and version. Workspace members are left out: `check` covers
/// them.
pub fn dependencies(root: &Path, target: &str, cargo_args: &[String]) -> Result<Vec<Dependency>> {
    let output = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            target,
        ])
        .args(cargo_args)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).context("reading cargo metadata's output")?;
    resolve(metadata)
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    resolve: Option<Resolve>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    version: String,
    source: Option<String>,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
    edition: String,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<NodeDep>,
    features: Vec<String>,
}

#[derive(Deserialize)]
struct NodeDep {
    name: String,
    pkg: String,
    dep_kinds: Vec<DepKind>,
}

#[derive(Deserialize)]
struct DepKind {
    kind: Option<String>,
}

/// The crate kinds Cargo builds as a library another crate can use.
const LIB_KINDS: [&str; 6] = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

impl Package {
    fn lib(&self) -> Option<Lib> {
        let target = self.targets.iter().find(|target| {
            target
                .kind
                .iter()
                .any(|kind| LIB_KINDS.contains(&kind.as_str()))
        })?;
        Some(Lib {
            name: target.name.replace('-', "_"),
            src_path: target.src_path.clone(),
            edition: target.edition.clone(),
            proc_macro: target.kind.iter().any(|kind| kind == "proc-macro"),
        })
    }
}

impl NodeDep {
    fn is(&self, kind: Option<&str>) -> bool {
        self.dep_kinds
            .iter()
            .any(|dep_kind| dep_kind.kind.as_deref() == kind)
    }
}

fn resolve(metadata: Metadata) -> Result<Vec<Dependency>> {
    let resolve = metadata
        .resolve
        .ok_or_else(|| anyhow!("cargo metadata resolved no dependencies"))?;
    let packages: HashMap<&str, &Package> = metadata
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package))
        .collect();
    let nodes: HashMap<&str, &Node> = resolve
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let libs: HashMap<&str, Lib> = metadata
        .packages
        .iter()
        .filter_map(|package| Some((package.id.as_str(), package.lib()?)))
        .collect();
    let members: HashSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let is_proc_macro = |id: &str| libs.get(id).is_some_and(|lib| lib.proc_macro);

    // Runtime code is what the members reach through normal dependencies
    // without passing through a procedural macro, which runs in the compiler.
    let runtime = reachable(&members, &nodes, |dep| {
        dep.is(None) && !is_proc_macro(&dep.pkg)
    });
    let built = reachable(&members, &nodes, |dep| {
        dep.is(None) || dep.is(Some("build"))
    });

    let mut dependencies = Vec::new();
    for id in built.difference(&members) {
        let package = packages
            .get(id)
            .ok_or_else(|| anyhow!("cargo metadata has no package {id}"))?;
        let node = nodes
            .get(id)
            .ok_or_else(|| anyhow!("cargo metadata did not resolve {id}"))?;
        let externs = node
            .deps
            .iter()
            .filter(|dep| dep.is(None))
            .filter_map(|dep| Some((dep.name.clone(), libs.get(dep.pkg.as_str())?.name.clone())))
            .collect();
        dependencies.push(Dependency {
            name: package.name.clone(),
            version: package.version.clone(),
            source: package.source.clone(),
            kind: if runtime.contains(id) {
                Kind::Runtime
            } else {
                Kind::Build
            },
            root: package
                .manifest_path
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| anyhow!("{} has no parent", package.manifest_path.display()))?,
            lib: package.lib(),
            features: node.features.iter().cloned().collect(),
            externs,
        });
    }
    dependencies
        .sort_by(|a, b| (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source)));
    Ok(dependencies)
}

/// The packages reachable from `start` through the edges `follow` accepts,
/// including `start`.
fn reachable<'a>(
    start: &HashSet<&'a str>,
    nodes: &HashMap<&str, &'a Node>,
    follow: impl Fn(&NodeDep) -> bool,
) -> HashSet<&'a str> {
    let mut seen = start.clone();
    let mut queue: VecDeque<&str> = start.iter().copied().collect();
    while let Some(id) = queue.pop_front() {
        let Some(node) = nodes.get(id) else {
            continue;
        };
        for dep in node.deps.iter().filter(|dep| follow(dep)) {
            if seen.insert(dep.pkg.as_str()) {
                queue.push_back(dep.pkg.as_str());
            }
        }
    }
    seen
}

/// The host target triple and the configuration rustc sets for it.
pub fn host(root: &Path) -> Result<(String, String)> {
    let version = rustc(root, &["-vV"])?;
    let triple = version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| anyhow!("rustc -vV did not name the host target"))?
        .trim()
        .to_owned();
    let cfg = rustc(root, &["--print", "cfg"])?;
    Ok((triple, cfg))
}

fn rustc(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .current_dir(root)
        .args(args)
        .output()
        .with_context(|| format!("running rustc {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "rustc {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    String::from_utf8(output.stdout).context("reading rustc's output")
}
