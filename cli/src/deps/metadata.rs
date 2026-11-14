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

use super::cfg::{self, Features, Truth};

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
    /// The features a normal build gives it.
    pub features: Features,
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
/// them. `cargo_args` choose features.
///
/// `cargo metadata` gives the graph and the packages' sources, but it
/// resolves features with the dev-dependencies' features merged in, as a
/// test build would. `cargo tree` with only normal and build edges gives the
/// packages and features of a normal build, so it decides which packages
/// are built and with what features.
pub fn dependencies(
    root: &Path,
    triple: &str,
    target: &cfg::Target,
    cargo_args: &[String],
) -> Result<Vec<Dependency>> {
    let metadata = cargo(
        root,
        &[
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            triple,
        ],
        cargo_args,
    )?;
    let metadata: Metadata =
        serde_json::from_slice(&metadata).context("reading cargo metadata's output")?;
    let tree = cargo(
        root,
        &[
            "tree",
            "--edges",
            "normal,build",
            "--workspace",
            "--prefix",
            "none",
            "--format",
            "|{p}|{f}|",
            "--target",
            triple,
        ],
        cargo_args,
    )?;
    let tree = String::from_utf8(tree).context("reading cargo tree's output")?;
    let built = tree.lines().filter_map(Built::parse).collect::<Vec<_>>();
    resolve(metadata, &built, &Host { triple, target })
}

fn cargo(root: &Path, args: &[&str], cargo_args: &[String]) -> Result<Vec<u8>> {
    let output = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args(args)
        .args(cargo_args)
        .output()
        .with_context(|| format!("running cargo {}", args.first().unwrap_or(&"")))?;
    if !output.status.success() {
        bail!(
            "cargo {} failed:\n{}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim_end()
        );
    }
    Ok(output.stdout)
}

/// A package as one line of `cargo tree --format '|{p}|{f}|'` shows it:
/// `|name v1.0.0 (proc-macro) (/path/to/it)|feature,feature|`, with the
/// location only for packages outside a registry.
#[derive(Debug, PartialEq, Eq)]
struct Built {
    name: String,
    version: String,
    location: Option<String>,
    features: BTreeSet<String>,
}

impl Built {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.strip_prefix('|')?.split('|');
        let package = fields.next()?;
        let features = fields.next()?;
        let (name, rest) = package.split_once(' ')?;
        let (version, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let rest = rest.trim();
        let rest = rest.strip_prefix("(proc-macro)").unwrap_or(rest).trim();
        let location = rest
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            .map(str::to_owned);
        Some(Self {
            name: name.to_owned(),
            version: version.strip_prefix('v')?.to_owned(),
            location,
            features: features
                .split(',')
                .filter(|feature| !feature.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    /// Whether this line shows `package`. A package from a registry is
    /// shown by name and version alone; a path package by its directory.
    fn shows(&self, package: &Package) -> bool {
        if self.name != package.name || self.version != package.version {
            return false;
        }
        match (&self.location, &package.source) {
            (Some(location), None) => package.manifest_path.parent() == Some(Path::new(location)),
            _ => true,
        }
    }
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
    /// The platform the edge is for, as `cfg(...)` or a target triple.
    target: Option<String>,
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

/// The host a build is for, which decides which platform-specific
/// dependency edges are active.
struct Host<'a> {
    triple: &'a str,
    target: &'a cfg::Target,
}

impl NodeDep {
    /// Whether the dependency has an edge of `kind` that is active on the
    /// host. `--filter-platform` drops dependencies with no active edge, but
    /// keeps every edge of a dependency with one, so a Windows-only normal
    /// edge survives beside an active build edge. An edge whose platform
    /// can't be decided counts as active.
    fn is(&self, kind: Option<&str>, host: &Host) -> bool {
        self.dep_kinds.iter().any(|dep_kind| {
            dep_kind.kind.as_deref() == kind
                && dep_kind.target.as_deref().is_none_or(|platform| {
                    host.target.platform(host.triple, platform) != Truth::False
                })
        })
    }
}

fn resolve(metadata: Metadata, built: &[Built], host: &Host) -> Result<Vec<Dependency>> {
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
    // The feature sets of each package a normal build compiles. A package
    // that only a dev-dependency's features pull in has none.
    let feature_sets: HashMap<&str, Vec<BTreeSet<String>>> = metadata
        .packages
        .iter()
        .map(|package| {
            let sets = built
                .iter()
                .filter(|line| line.shows(package))
                .map(|line| line.features.clone())
                .collect::<Vec<_>>();
            (package.id.as_str(), sets)
        })
        .filter(|(_, sets)| !sets.is_empty())
        .collect();
    let is_built = |id: &str| feature_sets.contains_key(id);

    // Runtime code is what the members reach through normal dependencies
    // without passing through a procedural macro, which runs in the compiler.
    let runtime = reachable(&members, &nodes, |dep| {
        dep.is(None, host) && !is_proc_macro(&dep.pkg) && is_built(&dep.pkg)
    });
    let built = reachable(&members, &nodes, |dep| {
        (dep.is(None, host) || dep.is(Some("build"), host)) && is_built(&dep.pkg)
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
            .filter(|dep| dep.is(None, host))
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
            features: Features::from_sets(feature_sets.get(id).map_or(&[][..], Vec::as_slice)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cargo_tree_lines() {
        assert_eq!(
            Built::parse("|serde v1.0.229|default,derive,std|"),
            Some(Built {
                name: "serde".to_owned(),
                version: "1.0.229".to_owned(),
                location: None,
                features: BTreeSet::from(["default", "derive", "std"].map(str::to_owned)),
            })
        );
        assert_eq!(
            Built::parse("|codegen v0.0.0 (proc-macro) (/src/codegen)|| (*)"),
            Some(Built {
                name: "codegen".to_owned(),
                version: "0.0.0".to_owned(),
                location: Some("/src/codegen".to_owned()),
                features: BTreeSet::new(),
            })
        );
        assert_eq!(Built::parse("warning: something"), None);
    }
}
