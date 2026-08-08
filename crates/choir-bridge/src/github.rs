//! GitHub App auth + status write-back (D21 write-back stage, risk #15).
//!
//! Credential shape: App JWT (RS256, signed by shelling out to
//! `openssl` against the operator's PEM path — the key bytes never pass
//! through this process's callers) → short-lived installation token →
//! commit-status POST. Permissions required of the App: Checks +
//! Commit statuses (read & write), nothing else.

use std::path::Path;

/// URL-safe base64 without padding (JWT alphabet).
fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
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
        format!(r#"{{"iat":{},"exp":{},"iss":"{app_id}"}}"#, now - 60, now + 540).as_bytes(),
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
/// `owner/repo`@`sha` under the `choir/bridge` context.
///
/// # Errors
///
/// API failures with GitHub's response body included.
pub fn post_status(
    token: &str,
    repo: &str,
    sha: &str,
    state: &str,
    description: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "state": state,
        "context": "choir/bridge",
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
