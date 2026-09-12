//! `choir repo follower` — the remotes a node pushes its landings to
//! (D21), managed on the node host.
//!
//! A follower is a remote on a bare repository under the state
//! directory. `add` writes one with `git remote add` and drops the
//! marker that makes `choir node serve` start the daemon with
//! `--followers`; `list` reads them back; `push` pushes now, which is
//! what the daemon does after every landing once restarted. Nothing
//! here fetches, and nothing forces: the node is canonical, a follower
//! copies it, and one that diverged is a push that refuses.
//!
//! # Examples
//!
//! ```
//! use choir_cli::follower::repository_name;
//!
//! // `.git` is the bare repository's own spelling, added when missing.
//! assert_eq!(repository_name("me/thing"), "me/thing.git");
//! assert_eq!(repository_name("me/thing.git"), "me/thing.git");
//! ```

/// A repository as `repos.list` and the root spell it.
#[must_use]
pub fn repository_name(name: &str) -> String {
    if name.ends_with(".git") {
        name.to_string()
    } else {
        format!("{name}.git")
    }
}

/// The repositories a node holds, in `repos.list` order.
#[must_use]
pub fn repositories(layout: &crate::serve::Layout) -> Vec<String> {
    std::fs::read_to_string(layout.state.join("repos.list"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Adds one remote to one repository, and the marker if it is not there.
///
/// Returns whether the marker was written now: the daemon reads it at
/// start, so a marker written now means a restart before landings
/// follow.
///
/// # Errors
///
/// Returns a description when the repository is not under the root, the
/// name is already a remote, or git refuses.
pub fn add(
    layout: &crate::serve::Layout,
    repo: &str,
    name: &str,
    url: &str,
) -> Result<bool, String> {
    let repo = repository_name(repo);
    let bare = layout.repos.join(&repo);
    if !bare.join("HEAD").is_file() {
        return Err(format!(
            "no repository {repo} under {}\n\n  what is here: choir repo follower list",
            layout.repos.display()
        ));
    }
    if name.is_empty() || name.starts_with('-') || name.contains(['/', ' ']) {
        return Err(format!("{name:?} is not a remote name"));
    }
    if choir_node::followers::remotes(&bare)
        .iter()
        .any(|(n, _)| n == name)
    {
        return Err(format!(
            "{repo} already has a remote named {name}\n\n  \
             see it: git --git-dir {} remote -v",
            bare.display()
        ));
    }
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["remote", "add", name, url])
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git remote add refused: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let marker = &layout.followers_marker;
    if marker.exists() {
        return Ok(false);
    }
    choir_fs::write_atomic(
        marker,
        "# Landings are pushed to every remote of their repository (D21).\n\
         # Written by `choir repo follower add`; `choir node serve` passes\n\
         # --followers while this file exists. Delete it to stop.\n",
    )
    .map_err(|e| format!("write {}: {e}", marker.display()))?;
    Ok(true)
}

/// Every repository with its remotes, `(repo, remotes)`.
#[must_use]
pub fn list(layout: &crate::serve::Layout) -> Vec<(String, Vec<(String, String)>)> {
    repositories(layout)
        .into_iter()
        .map(|repo| {
            let remotes = choir_node::followers::remotes(&layout.repos.join(&repo));
            (repo, remotes)
        })
        .collect()
}

/// Pushes one repository, or every listed one, to each of its remotes.
#[must_use]
pub fn push(
    layout: &crate::serve::Layout,
    only: Option<&str>,
) -> Vec<choir_node::followers::Outcome> {
    let repos = match only {
        Some(repo) => vec![repository_name(repo)],
        None => repositories(layout),
    };
    repos
        .iter()
        .flat_map(|repo| choir_node::followers::push_all(&layout.repos, repo))
        .collect()
}

/// Whether the marker that makes the daemon push after landings exists.
#[must_use]
pub fn following(layout: &crate::serve::Layout) -> bool {
    layout.followers_marker.exists()
}

/// The bare repository's path, for the message that names it.
#[must_use]
pub fn bare(layout: &crate::serve::Layout, repo: &str) -> std::path::PathBuf {
    layout.repos.join(repository_name(repo))
}
