//! Blast radius: how far a change can reach (D23, the unbuilt half).
//!
//! D23 registers "blast-radius-gated landing" but the term had no
//! operational definition anywhere in the code. This is signal #1 of the
//! five the D23 research ranked by cost-to-compute against predictive
//! value: **reverse-dependency reach in the cargo unit graph**. It was
//! ranked first because it needs *zero historical data* — no labelled
//! corpus, no revert history, no coverage run. Pure graph traversal.
//!
//! Deliberately measured and recorded, **not gated on**. A gate needs a
//! threshold, a threshold needs calibration, and calibration needs the
//! base rate we have not measured yet (the revised D23 tripwire). So
//! this instruments first: compute the score, attach it to the change,
//! and let it accumulate into the data a threshold could later be fitted
//! against. Instrumenting before gating is the whole point.
//!
//! **Granularity is crate-level, and that is a real limitation, not a
//! detail.** A one-character change to `choir-hash` scores exactly the
//! same as rewriting it, because both touch the same package and the
//! same set of packages depend on it. The score answers "how much of the
//! workspace *could* this change break", never "how likely is it to".
//! Module- or item-level reach would need parsing Rust, which is a
//! different and much larger job.
//!
//! Input is `cargo metadata --no-deps` JSON rather than a path, so the
//! analysis is a pure function over data. [`workspace_metadata`] is the
//! separate impure half that shells out. That split keeps the tests
//! hermetic and, more practically, stops a test from invoking cargo
//! inside cargo.

use std::collections::{BTreeMap, BTreeSet};

/// How far a change reaches through the workspace dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlastRadius {
    /// Packages that directly contain at least one changed file, sorted.
    pub touched: Vec<String>,
    /// Every package that transitively depends on a touched one, plus the
    /// touched packages themselves, sorted. This is the set a break could
    /// propagate to.
    pub reached: Vec<String>,
    /// Workspace packages in total, the denominator of [`Self::fraction`].
    pub total: usize,
    /// Changed files that fell outside every package (workspace-root
    /// files like `plan.md`, or a path from another repo), sorted.
    ///
    /// Kept rather than dropped: a change that is *entirely* unattributed
    /// scores zero, and zero should be distinguishable from "touched
    /// nothing that matters" by looking at this field.
    pub unattributed: Vec<String>,
}

impl BlastRadius {
    /// Reached packages as a fraction of the workspace, in `0.0..=1.0`.
    ///
    /// An empty workspace scores 0.0 rather than dividing by zero.
    #[must_use]
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.reached.len() as f64 / self.total as f64
    }
}

/// One workspace package as the analysis needs it.
struct Package {
    name: String,
    /// Directory containing its `Cargo.toml`, with a trailing separator so
    /// prefix matching cannot pair `choir-hash` with `choir-hashing`.
    dir: String,
    /// Names of its dependencies that are also workspace members.
    deps: Vec<String>,
}

/// Computes the blast radius of `changed` against `metadata`, the output
/// of `cargo metadata --no-deps --format-version 1`.
///
/// Paths in `changed` may be absolute or relative to the workspace root;
/// both are matched against each package's directory. A file is
/// attributed to the *longest* matching package directory, so a nested
/// package wins over the workspace root.
///
/// # Errors
///
/// Returns a description when `metadata` is not JSON, or lacks the
/// `packages` array with `name` and `manifest_path` on each entry.
///
/// # Examples
///
/// ```
/// # use choir_queue::blast::blast_radius;
/// let metadata = r#"{"packages":[
///   {"name":"lib","manifest_path":"/w/lib/Cargo.toml","dependencies":[]},
///   {"name":"app","manifest_path":"/w/app/Cargo.toml",
///    "dependencies":[{"name":"lib"}]}
/// ]}"#;
/// // Touching the leaf reaches only itself.
/// let r = blast_radius(metadata, &["/w/app/src/main.rs"]).unwrap();
/// assert_eq!(r.reached, ["app"]);
/// // Touching the library reaches its dependent too.
/// let r = blast_radius(metadata, &["/w/lib/src/lib.rs"]).unwrap();
/// assert_eq!(r.reached, ["app", "lib"]);
/// ```
pub fn blast_radius(metadata: &str, changed: &[&str]) -> Result<BlastRadius, String> {
    let (packages, root) = parse_packages(metadata)?;
    let names: BTreeSet<&str> = packages.iter().map(|p| p.name.as_str()).collect();

    // Reverse edges: dependency -> everything that depends on it.
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pkg in &packages {
        for dep in &pkg.deps {
            if names.contains(dep.as_str()) {
                dependents.entry(dep.as_str()).or_default().push(&pkg.name);
            }
        }
    }

    let mut touched = BTreeSet::new();
    let mut unattributed = BTreeSet::new();
    for path in changed {
        // Both the package dirs and the changed path are reduced to
        // workspace-relative form, so a caller may pass either an
        // absolute path or one relative to the workspace root without the
        // matching rule needing two branches.
        let rel = strip_root(path, &root);
        // Longest directory wins, so a package nested inside another is
        // attributed to the inner one.
        match packages
            .iter()
            .filter(|p| rel.starts_with(strip_root(&p.dir, &root)))
            .max_by_key(|p| p.dir.len())
        {
            Some(pkg) => {
                touched.insert(pkg.name.as_str());
            }
            None => {
                unattributed.insert((*path).to_string());
            }
        }
    }

    // Transitive closure over reverse edges. A cyclic graph would loop
    // forever without the visited set; cargo forbids cycles, but this
    // does not depend on cargo continuing to.
    let mut reached: BTreeSet<&str> = touched.clone();
    let mut frontier: Vec<&str> = touched.iter().copied().collect();
    while let Some(name) = frontier.pop() {
        for &dependent in dependents.get(name).into_iter().flatten() {
            if reached.insert(dependent) {
                frontier.push(dependent);
            }
        }
    }

    Ok(BlastRadius {
        touched: touched.into_iter().map(String::from).collect(),
        reached: reached.into_iter().map(String::from).collect(),
        total: packages.len(),
        unattributed: unattributed.into_iter().collect(),
    })
}

/// Reduces `path` to workspace-relative form. A path already relative, or
/// one under a different root, is returned unchanged — so a stray absolute
/// path from elsewhere simply fails to match any package and lands in
/// `unattributed` rather than being silently attributed to one.
fn strip_root<'a>(path: &'a str, root: &str) -> &'a str {
    if root.is_empty() {
        return path;
    }
    path.strip_prefix(root)
        .map_or(path, |rest| rest.trim_start_matches('/'))
}

/// Pulls the fields the analysis needs out of `cargo metadata` JSON.
fn parse_packages(metadata: &str) -> Result<(Vec<Package>, String), String> {
    let root: serde_json::Value =
        serde_json::from_str(metadata).map_err(|e| format!("metadata is not json: {e}"))?;
    let list = root
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or("metadata has no `packages` array")?;
    // Absent in hand-written metadata; then everything stays as given,
    // which is correct as long as the caller is consistent about form.
    let root_dir = root
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    list.iter()
        .map(|p| {
            let name = p
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or("package has no `name`")?
                .to_string();
            let manifest = p
                .get("manifest_path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("package {name} has no `manifest_path`"))?;
            let dir = manifest
                .strip_suffix("Cargo.toml")
                .unwrap_or(manifest)
                .to_string();
            let deps = p
                .get("dependencies")
                .and_then(serde_json::Value::as_array)
                .map(|ds| {
                    ds.iter()
                        .filter_map(|d| d.get("name").and_then(serde_json::Value::as_str))
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            Ok(Package { name, dir, deps })
        })
        .collect::<Result<Vec<_>, String>>()
        .map(|pkgs| (pkgs, root_dir))
}

/// Runs `cargo metadata --no-deps --format-version 1` in `root` and
/// returns its JSON, for feeding to [`blast_radius`].
///
/// Shells out rather than linking a manifest parser, the same posture the
/// workspace takes with `curl`, `openssl`, and `mergiraf`. `--no-deps`
/// keeps it offline: registry packages are never resolved.
///
/// # Errors
///
/// Returns a description if cargo cannot be spawned or exits nonzero.
pub fn workspace_metadata(root: &std::path::Path) -> Result<String, String> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("metadata is not utf-8: {e}"))
}
