//! The release shelf: a directory of build artifacts the operator put
//! there on purpose, served to anybody (D79).
//!
//! This is the one place a node hands out bytes that are not a git
//! object and not a page about one. It exists because the install
//! command was the last instruction on our own front door that named
//! somebody else's host, and a project whose claim is that a node is a
//! code host should not need a forge to answer `curl`.
//!
//! Three things are served under [`PREFIX`], and only three:
//!
//! - `install.sh`, rendered here rather than read from the shelf, so
//!   the script can never disagree with the layout it fetches from.
//! - any file the operator placed on the shelf, by exact name.
//! - an index of the shelf, so the address is not a guessing game.
//!
//! **Nothing is generated, mirrored or fetched.** The shelf is whatever
//! is in the directory; putting a release on it is an operator action
//! (`choirctl publish-release`), which is what keeps this module free of
//! any notion of a version, a release or an upstream.

use crate::ui::esc;

/// Where the shelf is served. Named once because the route, the index,
/// the installer's own `BASE` and the tests must agree.
pub(crate) const PREFIX: &str = "/download/";

/// The installer, rendered by [`installer`] rather than served as it
/// sits here.
///
/// Kept as a file rather than a Rust string so it can be linted, run and
/// read as the shell script it is. `shellcheck` in the gate reads this
/// path, not a heredoc inside a `const`.
const INSTALLER: &str = include_str!("install.sh");

/// The one name on the shelf that is never read from disk.
///
/// A file of this name in the directory is ignored rather than refused:
/// an operator who unpacks a whole release onto the shelf gets
/// `choir-cli-installer.sh` and friends, and shadowing the one script
/// that knows this node's layout with one that names a forge is the
/// exact failure this module exists to prevent.
pub(crate) const INSTALLER_NAME: &str = "install.sh";

/// The placeholder [`installer`] replaces, and the reason an
/// unsubstituted copy of the script refuses to run.
const BASE_MARK: &str = "__CHOIR_DOWNLOAD_BASE__";

/// The installer with its `BASE` bound to the origin that asked for it.
///
/// The script fetches every archive from the host it was itself fetched
/// from, which is the property that makes it correct on any node without
/// anybody configuring an address. [`crate::Node`] deliberately learns
/// no public address of its own, and this keeps it that way.
///
/// `origin` is caller-validated (see [`safe_origin`]); it lands inside a
/// double-quoted shell string, so a quote, a `$` or a backtick reaching
/// here would be a command, not a hostname.
pub(crate) fn installer(origin: &str) -> String {
    // No trailing slash: the script joins with `"$BASE/$archive"`, and a
    // doubled slash in a URL a reader is asked to trust looks like a bug
    // even where it resolves.
    let base = format!("{origin}{}", PREFIX.trim_end_matches('/'));
    INSTALLER.replace(BASE_MARK, &base)
}

/// Whether a `Host` header may be pasted into a shell script.
///
/// A `Host` is attacker-supplied, and this one is baked into a script
/// somebody is about to pipe to `sh`. The answer is not to escape it but
/// to refuse anything that is not a hostname: letters, digits, dot,
/// hyphen and the port colon. That admits every real `Host` and no shell
/// metacharacter, so the substitution has nothing left to get wrong.
///
/// The refusal is a `400`, not a fallback to a guessed address. A script
/// that silently pointed somewhere other than where it was fetched from
/// is the one outcome worse than no script.
pub(crate) fn safe_origin(origin: &str) -> bool {
    !origin.is_empty()
        && origin.len() <= 255
        && origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .is_some_and(|host| {
                !host.is_empty()
                    && host
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
            })
}

/// The one shape a name on the shelf may have.
///
/// One path segment of the characters a release artifact actually uses,
/// which is narrow enough that traversal, absolute paths and every
/// encoded spelling of `..` are all refused by the same rule rather than
/// by a list of things to strip. A leading dot is out because the shelf
/// is a public directory and dotfiles are how something private ends up
/// in one by accident.
pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// What to say a file is, by the only thing we know about it: its name.
///
/// `.sh` is `text/plain` on purpose. A browser that offers to run a
/// shell script is doing the reader no favour, and the whole argument
/// for a short installer is that somebody can open the URL and read it
/// before piping it anywhere.
pub(crate) fn content_type(name: &str) -> &'static str {
    if name.ends_with(".sh") || name.ends_with(".sha256") || name.ends_with(".sum") {
        "text/plain; charset=utf-8"
    } else if name.ends_with(".json") {
        "application/json"
    } else if name.ends_with(".tar.xz") || name.ends_with(".xz") {
        "application/x-xz"
    } else if name.ends_with(".tar.gz") || name.ends_with(".gz") {
        "application/gzip"
    } else {
        "application/octet-stream"
    }
}

/// Every file on the shelf, by name and size, sorted.
///
/// Regular files only, and no recursion: a directory on the shelf is not
/// descended into and a symlink is not followed, so the shelf can only
/// ever hand out something an operator put directly in it. Unreadable
/// entries are skipped rather than reported, because this is a listing
/// and not a diagnostic.
pub(crate) fn entries(dir: &std::path::Path) -> Vec<(String, u64)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(String, u64)> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !safe_name(&name) || name == INSTALLER_NAME {
                return None;
            }
            let meta = entry.metadata().ok()?;
            let len = meta.len();
            meta.is_file().then_some((name, len))
        })
        .collect();
    rows.sort();
    rows
}

/// A size a person can read, in the unit the number lands in.
///
/// Binary units, spelled `MiB`, because these are file sizes and the
/// number a reader compares this against is the one their file manager
/// shows.
fn size(bytes: u64) -> String {
    // A display string for a file size: the bits a `f64` loses here are
    // far below the one decimal place this prints.
    #[allow(clippy::cast_precision_loss)]
    let n = bytes as f64;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", n / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", n / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// The shelf as a page: the install command first, the files under it.
///
/// The command comes first because it is what almost every reader wants,
/// and the listing is there for the reader who wants to check what they
/// are about to run against what is actually published.
pub(crate) fn index(dir: &std::path::Path, origin: &str, theme: Option<&str>) -> String {
    let rows = entries(dir);
    let mut h = String::with_capacity(4 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(&mut h, theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>downloads</title>");
    h.push_str(crate::ui::STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    h.push_str("<header class=\"top\"><h1>downloads</h1><div class=\"sub\"><span class=\"pill\">");
    h.push_str(&format!("{} files", rows.len()));
    h.push_str("</span></div></header><main id=\"main\"><section>");

    h.push_str(
        "<p>Prebuilt binaries, served by this node. The installer fetches every archive \
         from here and checks it against the digest published beside it.</p>",
    );
    h.push_str("<pre class=\"cmd\">curl -fsSL ");
    h.push_str(&esc(origin));
    h.push_str(PREFIX);
    h.push_str(INSTALLER_NAME);
    h.push_str(" | sh</pre>");
    h.push_str(
        "<p class=\"muted\">The daemon as well as the client: append \
         <code>| sh -s -- choir-cli choir-node</code>.</p>",
    );

    if rows.is_empty() {
        // An empty shelf is a configured node with nothing published
        // yet, which is a different thing from a node that does not do
        // this, and the page should not read like the second.
        h.push_str("<p class=\"muted\">Nothing is published here yet.</p>");
    } else {
        h.push_str("<table class=\"repos\"><tbody>");
        for (name, bytes) in rows {
            h.push_str("<tr><td><a href=\"");
            h.push_str(PREFIX);
            h.push_str(&esc(&name));
            h.push_str("\" class=\"mono\">");
            h.push_str(&esc(&name));
            h.push_str("</a></td><td class=\"num\">");
            h.push_str(&esc(&size(bytes)));
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section></main><footer>Every write here is a signed operation.</footer>");
    h.push_str("</body></html>");
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_one_segment_of_the_characters_a_release_uses() {
        for good in [
            "choir-cli-aarch64-apple-darwin.tar.xz",
            "choir-cli-aarch64-apple-darwin.tar.xz.sha256",
            "sha256.sum",
            "dist-manifest.json",
        ] {
            assert!(safe_name(good), "refused a real artifact name: {good}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "../etc/passwd",
            "a%2fb",
            "a b",
            "a\0b",
            "/etc/passwd",
        ] {
            assert!(!safe_name(bad), "accepted a name it must not: {bad:?}");
        }
    }

    /// The `Host` header lands inside a double-quoted shell string, so
    /// this predicate is the whole defence. Asserted on the shapes an
    /// injection would actually take rather than on a charset.
    #[test]
    fn an_origin_that_could_be_a_command_is_not_an_origin() {
        assert!(safe_origin("https://choirs.dev"));
        assert!(safe_origin("http://127.0.0.1:8417"));
        for bad in [
            "",
            "choirs.dev",
            "https://",
            "https://a\"; rm -rf /; \"",
            "https://a$(id)",
            "https://a`id`",
            "https://a b",
            "https://a\nb",
            "https://a/b",
            "https://a\\b",
        ] {
            assert!(!safe_origin(bad), "accepted an unsafe origin: {bad:?}");
        }
    }

    /// The script must not be servable in a state where it silently
    /// points somewhere other than where it came from.
    #[test]
    fn the_installer_carries_no_address_until_one_is_bound() {
        assert!(
            INSTALLER.contains(BASE_MARK),
            "the placeholder the renderer replaces is gone from install.sh"
        );
        assert!(
            !INSTALLER.contains("github.com"),
            "install.sh names a forge; it must fetch from the node that served it"
        );
        let rendered = installer("https://choirs.dev");
        assert!(rendered.contains("BASE=\"https://choirs.dev/download\""));
        assert!(
            !rendered.contains(BASE_MARK),
            "a rendered installer still carries the placeholder"
        );
        // The guard against an unrendered copy must survive rendering.
        // Spelled in full it would be replaced along with the
        // assignment, turning the check into "refuse when BASE starts
        // with BASE" -- which refused every rendered script and no
        // unrendered one, and did so identically for both.
        assert!(
            rendered.contains("*CHOIR_DOWNLOAD_BASE*)"),
            "rendering ate the guard that catches an unrendered copy"
        );
    }

    #[test]
    fn a_shell_script_is_offered_as_text_to_read() {
        assert_eq!(content_type("install.sh"), "text/plain; charset=utf-8");
        assert_eq!(content_type("x.tar.xz.sha256"), "text/plain; charset=utf-8");
        assert_eq!(content_type("x.tar.xz"), "application/x-xz");
        assert_eq!(content_type("choir"), "application/octet-stream");
    }
}
