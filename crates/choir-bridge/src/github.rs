//! GitHub App auth + status write-back (D21 write-back stage, risk #15).
//!
//! Credential shape: App JWT (RS256, signed by shelling out to
//! `openssl` against the operator's PEM path — the key bytes never pass
//! through this process's callers) → short-lived installation token →
//! commit-status POST. Permissions required of the App: Commit
//! statuses (read & write) plus, for the queue stage, Pull requests
//! (read), Checks (read), and Contents (read & write) to publish the
//! train branch. Nothing else.

use std::path::Path;

/// URL-safe base64 without padding (JWT alphabet).
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// Mints a short-lived (9 min) App JWT for `app_id`, signing with the
/// RS256 key at `pem` via the `openssl` binary.
///
/// # Errors
///
/// Human-readable failure of the temp I/O or openssl invocation.
pub fn app_jwt(app_id: &str, pem: &Path) -> Result<String, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = b64url(
        // iat backdated 60 s against clock skew, per GitHub's docs.
        format!(
            r#"{{"iat":{},"exp":{},"iss":"{app_id}"}}"#,
            now - 60,
            now + 540
        )
        .as_bytes(),
    );
    let signing_input = format!("{header}.{payload}");

    let dir = std::env::temp_dir().join(format!("choir-jwt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let input = dir.join("input");
    let sig = dir.join("sig");
    std::fs::write(&input, &signing_input).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("openssl")
        .arg("dgst")
        .arg("-sha256")
        .arg("-sign")
        .arg(pem)
        .arg("-out")
        .arg(&sig)
        .arg(&input)
        .output()
        .map_err(|e| format!("spawn openssl: {e}"))?;
    if !out.status.success() {
        std::fs::remove_dir_all(&dir).ok();
        return Err(format!(
            "openssl sign failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let signature = std::fs::read(&sig).map_err(|e| e.to_string())?;
    std::fs::remove_dir_all(&dir).ok();
    Ok(format!("{signing_input}.{}", b64url(&signature)))
}

/// GitHub API call; returns (status, body).
fn gh(method: &str, url: &str, auth: &str, body: Option<&str>) -> Result<(u16, String), String> {
    let mut args: Vec<String> = vec![
        "-s".into(),
        "-w".into(),
        "\n%{http_code}".into(),
        "-X".into(),
        method.into(),
        "-H".into(),
        format!("Authorization: Bearer {auth}"),
        "-H".into(),
        "Accept: application/vnd.github+json".into(),
        "-H".into(),
        "User-Agent: choir-bridge".into(),
    ];
    if let Some(b) = body {
        args.push("-d".into());
        args.push(b.into());
    }
    args.push(url.into());
    let out = std::process::Command::new("curl")
        .args(&args)
        .output()
        .map_err(|e| format!("spawn curl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    match text.rsplit_once('\n') {
        Some((b, code)) => Ok((code.trim().parse().unwrap_or(0), b.to_string())),
        None => Ok((0, text)),
    }
}

/// Exchanges the App JWT for an installation token scoped to whatever
/// repos the installation covers.
///
/// # Errors
///
/// API failures with GitHub's response body included.
pub fn installation_token(jwt: &str) -> Result<String, String> {
    let (status, body) = gh("GET", "https://api.github.com/app/installations", jwt, None)?;
    if status != 200 {
        return Err(format!("list installations: {status}: {body}"));
    }
    let installs: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("installations json: {e}"))?;
    let id = installs
        .as_array()
        .and_then(|a| a.first())
        .and_then(|i| i["id"].as_u64())
        .ok_or("no installations found — is the App installed on a repo?")?;
    let (status, body) = gh(
        "POST",
        &format!("https://api.github.com/app/installations/{id}/access_tokens"),
        jwt,
        None,
    )?;
    if status != 201 {
        return Err(format!("mint token: {status}: {body}"));
    }
    let tok: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("token json: {e}"))?;
    tok["token"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "token field missing".to_string())
}

/// Returns the raw installations JSON (debug aid: shows the permission
/// set each installation has actually accepted).
///
/// # Errors
///
/// API failures with GitHub's response body included.
pub fn installations_debug(jwt: &str) -> Result<String, String> {
    let (status, body) = gh("GET", "https://api.github.com/app/installations", jwt, None)?;
    if status != 200 {
        return Err(format!("list installations: {status}: {body}"));
    }
    Ok(body)
}

/// Resolves the repo's default-branch HEAD sha via the API (works on
/// private repos the installation covers).
///
/// # Errors
///
/// API failures with GitHub's response body included.
pub fn head_sha(token: &str, repo: &str) -> Result<String, String> {
    let (status, body) = gh(
        "GET",
        &format!("https://api.github.com/repos/{repo}/commits/HEAD"),
        token,
        None,
    )?;
    if status != 200 {
        return Err(format!("resolve HEAD: {status}: {body}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    v["sha"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "sha field missing".to_string())
}

/// Posts a commit status (`state`: success/failure/error/pending) on
/// `owner/repo`@`sha` under `context`.
///
/// # Errors
///
/// API failures with GitHub's response body included.
pub fn post_status(
    token: &str,
    repo: &str,
    sha: &str,
    context: &str,
    state: &str,
    description: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "state": state,
        "context": context,
        "description": description,
    })
    .to_string();
    let (status, resp) = gh(
        "POST",
        &format!("https://api.github.com/repos/{repo}/statuses/{sha}"),
        token,
        Some(&body),
    )?;
    if status == 201 {
        Ok(())
    } else {
        Err(format!("post status: {status}: {resp}"))
    }
}

/// An open pull request as the queue sees it.
///
/// **This struct is a security boundary, not a convenience.** The queue
/// reads pull requests from an upstream forge (untrusted input), holds a
/// GitHub App private key (privileged credential), and with `--land`
/// fast-forwards a base branch (external write) — all three legs of the
/// prompt-injection lethal trifecta in one process. The Rule-of-Two
/// mitigation is that no untrusted *text* may reach a decision path, and
/// this type is where that is enforced: it carries a number and an oid,
/// both structured, and deliberately carries **no title, body, branch
/// name, or author**.
///
/// Adding a text field here is not a cosmetic change. A PR title in a
/// status description is a channel from attacker-controlled text into
/// the bot's own output, and a PR body reaching any conditional is the
/// vulnerability itself. `bridge_trifecta.rs` fails if this type starts
/// carrying attacker-controlled text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pr {
    /// PR number.
    pub number: u64,
    /// Head commit sha (what the verdict status is posted on).
    pub head_sha: String,
}

/// Parses the response body of `GET /repos/{repo}/pulls` into queue
/// entries, oldest PR first (train order = submission order).
#[must_use]
pub fn parse_prs(body: &str) -> Vec<Pr> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let mut prs: Vec<Pr> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(Pr {
                number: p["number"].as_u64()?,
                head_sha: p["head"]["sha"].as_str()?.to_string(),
            })
        })
        .collect();
    prs.sort_by_key(|p| p.number);
    prs
}

/// Lists open PRs on `repo` (needs App permission Pull requests: read).
///
/// # Errors
///
/// API failures with GitHub's response body included.
pub fn list_open_prs(token: &str, repo: &str) -> Result<Vec<Pr>, String> {
    let (status, body) = gh(
        "GET",
        &format!("https://api.github.com/repos/{repo}/pulls?state=open&per_page=100"),
        token,
        None,
    )?;
    if status != 200 {
        return Err(format!("list prs: {status}: {body}"));
    }
    Ok(parse_prs(&body))
}

/// Aggregate CI verdict for one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No check runs reported yet (CI may not have started).
    NoRuns,
    /// At least one run still queued or in progress.
    Pending,
    /// All runs completed successfully (incl. neutral/skipped).
    Success,
    /// At least one run completed unsuccessfully.
    Failure,
}

/// Parses the response body of `GET /commits/{sha}/check-runs` into a
/// [`Verdict`]. Neutral and skipped conclusions count as success;
/// anything else non-success (failure, cancelled, timed out) fails the
/// train.
///
/// Reads **only** `status` and `conclusion`, each compared against a
/// fixed set of literals. Check-run names, titles, summaries and output
/// text are attacker-influenceable — a workflow is defined in the
/// repository, so a pull request can propose one — and none of them
/// reach this decision. See [`Pr`] for why that matters.
#[must_use]
pub fn parse_check_verdict(body: &str) -> Verdict {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let runs: Vec<&serde_json::Value> = v["check_runs"].as_array().into_iter().flatten().collect();
    if runs.is_empty() {
        return Verdict::NoRuns;
    }
    let mut verdict = Verdict::Success;
    for run in runs {
        if run["status"].as_str() != Some("completed") {
            return Verdict::Pending;
        }
        match run["conclusion"].as_str() {
            Some("success" | "neutral" | "skipped") => {}
            _ => verdict = Verdict::Failure,
        }
    }
    verdict
}

/// Fetches the CI verdict for `sha` (needs App permission Checks:
/// read; GitHub Actions reports through the Checks API).
///
/// # Errors
///
/// API failures with GitHub's response body included.
pub fn check_verdict(token: &str, repo: &str, sha: &str) -> Result<Verdict, String> {
    let (status, body) = gh(
        "GET",
        &format!("https://api.github.com/repos/{repo}/commits/{sha}/check-runs?per_page=100"),
        token,
        None,
    )?;
    if status != 200 {
        return Err(format!("check runs: {status}: {body}"));
    }
    Ok(parse_check_verdict(&body))
}

/// Resolves the repo's default branch name.
///
/// # Errors
///
/// API failures with GitHub's response body included.
pub fn default_branch(token: &str, repo: &str) -> Result<String, String> {
    let (status, body) = gh(
        "GET",
        &format!("https://api.github.com/repos/{repo}"),
        token,
        None,
    )?;
    if status != 200 {
        return Err(format!("repo info: {status}: {body}"));
    }
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    v["default_branch"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "default_branch missing".to_string())
}
