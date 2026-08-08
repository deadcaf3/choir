//! L3 node daemon: a minimal git smart-HTTP server (plan.md L3, D12).
//!
//! v1 wraps `git http-backend` (git's own CGI) over bare repositories, the
//! same shape as Tangled's knot: the daemon is a thin, self-hostable shell
//! over git plumbing, and platform behavior (sequencer, queue, identity)
//! layers on top. ForgeMark benchmarks this surface directly.
//!
//! Authentication is per-actor basic auth ([`AuthTable`], `--auth-file`);
//! the platform API ([`platform`]) additionally verifies ed25519 op
//! signatures. The bind stays loopback-only: beyond localhost you still
//! need TLS or an SSH tunnel so tokens aren't sent in the clear.

use std::io::Read;
use std::path::{Path, PathBuf};

pub mod platform;

pub use platform::Platform;

/// Per-actor credentials: username → token, checked as HTTP basic auth
/// (the standard git-over-HTTP shape; every forge client speaks it).
///
/// L8 note: usernames are actor ids and tokens are per-actor secrets
/// minted by the operator; key-signature-based challenge auth can
/// replace the token *check* later without changing the wire shape.
pub type AuthTable = std::collections::HashMap<String, String>;

/// A running node daemon serving repos under a root directory.
pub struct Node {
    root: PathBuf,
    server: std::sync::Arc<tiny_http::Server>,
    port: u16,
    auth: std::sync::Arc<Option<AuthTable>>,
    platform: Option<std::sync::Arc<Platform>>,
    /// Loopback secret handed to repo hooks via env so their callback to
    /// `/api/git-update` passes the auth gate without user credentials.
    internal_token: String,
}

impl Node {
    /// Binds to `127.0.0.1:port` (0 = ephemeral) over `root`, with no
    /// authentication — localhost/dev only.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be bound or `root` cannot be
    /// created.
    pub fn bind(root: &Path, port: u16) -> std::io::Result<Self> {
        Self::bind_with_auth(root, port, None)
    }

    /// Binds like [`Node::bind`]; when `auth` is `Some`, every request
    /// must carry valid basic-auth credentials from the table or it is
    /// answered with 401 before touching git.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Node::bind`].
    pub fn bind_with_auth(
        root: &Path,
        port: u16,
        auth: Option<AuthTable>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let server = tiny_http::Server::http(("127.0.0.1", port))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let port = match server.server_addr().to_ip() {
            Some(addr) => addr.port(),
            None => 0,
        };
        Ok(Self {
            root: root.to_path_buf(),
            server: std::sync::Arc::new(server),
            port,
            auth: std::sync::Arc::new(auth),
            platform: None,
            internal_token: choir_identity::ActorKey::generate().actor_id().to_hex(),
        })
    }

    /// Enables the platform API (`/api/submit`, `/api/view`) backed by
    /// `platform`. Call before [`Node::serve_forever`].
    pub fn enable_platform(&mut self, platform: Platform) {
        self.platform = Some(std::sync::Arc::new(platform));
    }

    /// Port the daemon is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Creates a bare repo `name` (e.g. `"owner/repo.git"`) with pushes
    /// enabled.
    ///
    /// # Errors
    ///
    /// Fails when the path exists or `git init` fails.
    pub fn create_repo(&self, name: &str) -> std::io::Result<()> {
        let path = self.repo_path(name)?;
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                name.to_string(),
            ));
        }
        let ok = std::process::Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&path)
            .status()?
            .success()
            && std::process::Command::new("git")
                .args(["config", "http.receivepack", "true"])
                .current_dir(&path)
                .status()?
                .success()
            // Pin hooks to this repo: a host-global core.hooksPath (set
            // by e.g. husky) would otherwise silently bypass the
            // sequencer hook below.
            && std::process::Command::new("git")
                .args(["config", "core.hooksPath", "hooks"])
                .current_dir(&path)
                .status()?
                .success()
            // Advertise push certificates (`git push --signed`) and
            // verify their ssh signatures against the allowed-signers
            // file (per-actor keys, L8).
            && std::process::Command::new("git")
                .args([
                    "config",
                    "receive.certNonceSeed",
                    &choir_identity::ActorKey::generate().actor_id().to_hex(),
                ])
                .current_dir(&path)
                .status()?
                .success()
            && std::process::Command::new("git")
                .args(["config", "gpg.format", "ssh"])
                .current_dir(&path)
                .status()?
                .success()
            && std::process::Command::new("git")
                .args(["config", "gpg.ssh.allowedSignersFile"])
                .arg(
                    self.root
                        .canonicalize()
                        .unwrap_or_else(|_| self.root.clone())
                        .join(".choir")
                        .join("allowed_signers"),
                )
                .current_dir(&path)
                .status()?
                .success();
        if !ok {
            return Err(std::io::Error::other("git init/config failed"));
        }
        // The pre-receive hook routes every ref update of a push through
        // the platform sequencer (pre-receive, not update: only
        // pre-/post-receive see GIT_PUSH_CERT_* for signed pushes, and
        // one invocation covers the whole push). Outside the daemon (no
        // CHOIR_API) it is a no-op.
        let hook = path.join("hooks").join("pre-receive");
        std::fs::write(
            &hook,
            concat!(
                "#!/bin/sh\n",
                "# choir: route this push's ref updates through the platform sequencer.\n",
                "if [ -z \"$CHOIR_API\" ]; then cat >/dev/null; exit 0; fi\n",
                "while read old new ref; do\n",
                "  payload=$(printf '{\"repo\":\"%s\",\"refname\":\"%s\",\"old\":\"%s\",\"new\":\"%s\",\"user\":\"%s\",\"cert_status\":\"%s\",\"signer\":\"%s\"}' \\\n",
                "    \"$CHOIR_REPO\" \"$ref\" \"$old\" \"$new\" \"$CHOIR_USER\" \"$GIT_PUSH_CERT_STATUS\" \"$GIT_PUSH_CERT_SIGNER\")\n",
                "  if ! curl -sf -X POST -H \"X-Choir-Internal: $CHOIR_INTERNAL\" \\\n",
                "      -d \"$payload\" \"$CHOIR_API\" >/dev/null; then\n",
                "    echo \"choir: ref update rejected by sequencer: $ref\" >&2\n",
                "    exit 1\n",
                "  fi\n",
                "done\n",
                "exit 0\n",
            ),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    }

    /// Rejects path traversal and normalizes the repo path under root.
    fn repo_path(&self, name: &str) -> std::io::Result<PathBuf> {
        if name.split('/').any(|c| c == ".." || c.is_empty()) || name.starts_with('/') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bad repo name",
            ));
        }
        Ok(self.root.join(name))
    }

    /// Serves requests until the process exits. Run on a dedicated thread.
    pub fn serve_forever(&self) {
        for request in self.server.incoming_requests() {
            let root = self.root.clone();
            let auth = self.auth.clone();
            let platform = self.platform.clone();
            let internal_token = self.internal_token.clone();
            let port = self.port;
            std::thread::spawn(move || {
                // Hook callbacks authenticate with the loopback secret
                // instead of user credentials.
                let internal_ok = request.url().starts_with("/api/git-update")
                    && header(&request, "X-Choir-Internal").as_deref() == Some(&internal_token);
                let mut user = "anon".to_string();
                if let Some(table) = auth.as_ref() {
                    match authorized(table, &request) {
                        Some(u) => user = u,
                        None if internal_ok => {}
                        None => {
                            let response = tiny_http::Response::from_string("unauthorized\n")
                                .with_status_code(401)
                                .with_header(
                                    tiny_http::Header::from_bytes(
                                        &b"WWW-Authenticate"[..],
                                        &b"Basic realm=\"choir\""[..],
                                    )
                                    .expect("static header"),
                                );
                            let _ = request.respond(response);
                            return;
                        }
                    }
                }
                if request.url().starts_with("/api/") {
                    let _ = handle_api(platform.as_deref(), request);
                    return;
                }
                // Platform-enabled daemons pass the sequencer callback
                // into git's hook environment.
                let mut extra_env = Vec::new();
                if platform.is_some() {
                    if let Some(repo) = repo_from_path(request.url()) {
                        extra_env.push((
                            "CHOIR_API".to_string(),
                            format!("http://127.0.0.1:{port}/api/git-update"),
                        ));
                        extra_env.push(("CHOIR_REPO".to_string(), repo));
                        extra_env.push(("CHOIR_USER".to_string(), user));
                        extra_env.push(("CHOIR_INTERNAL".to_string(), internal_token));
                    }
                }
                let _ = handle(root, request, &extra_env);
            });
        }
    }

    /// Handle for stopping the accept loop (used by tests).
    pub fn unblock(&self) {
        self.server.unblock();
    }
}

/// Value of the first header named `name`, if present.
fn header(request: &tiny_http::Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

/// Extracts `owner/repo.git` from a smart-HTTP path like
/// `/owner/repo.git/git-receive-pack`.
fn repo_from_path(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let end = path.find(".git/").map(|i| i + 4).or_else(|| {
        path.ends_with(".git").then_some(path.len())
    })?;
    Some(path[1..end].to_string())
}

/// Checks a request's basic-auth credentials against the table; returns
/// the authenticated username.
fn authorized(table: &AuthTable, request: &tiny_http::Request) -> Option<String> {
    let auth_header = header(request, "Authorization")?;
    let b64 = auth_header.strip_prefix("Basic ")?.trim();
    let creds = String::from_utf8(base64_decode(b64)?).ok()?;
    let (user, token) = creds.split_once(':')?;
    // Compare without early exit on length/content so timing doesn't
    // leak how much of the token matched.
    let expected = table.get(user)?;
    let a = expected.as_bytes();
    let b = token.as_bytes();
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().min(b.len()) {
        diff |= (a[i] ^ b[i]) as usize;
    }
    (diff == 0).then(|| user.to_string())
}

/// Encodes bytes as standard base64 with `=` padding.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Renders a raw ed25519 public key in OpenSSH `ssh-ed25519 <b64>` form
/// (the wire blob is two length-prefixed strings: key type, key bytes).
pub fn ssh_ed25519_pubkey(raw: &[u8; 32]) -> String {
    let mut blob = Vec::new();
    for part in [b"ssh-ed25519".as_slice(), raw.as_slice()] {
        blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
        blob.extend_from_slice(part);
    }
    format!("ssh-ed25519 {}", base64_encode(&blob))
}

/// Writes `<root>/.choir/allowed_signers` — the file git's ssh signature
/// verification checks push certificates against. One line per actor:
/// principal (the actor id) followed by the OpenSSH public key.
///
/// # Errors
///
/// Propagates filesystem failures.
pub fn write_allowed_signers(
    root: &Path,
    keys: &[(String, [u8; 32])],
) -> std::io::Result<PathBuf> {
    let dir = root.join(".choir");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("allowed_signers");
    let mut contents = String::new();
    for (principal, raw) in keys {
        contents.push_str(&format!("{principal} {}\n", ssh_ed25519_pubkey(raw)));
    }
    std::fs::write(&path, contents)?;
    Ok(path)
}

/// Decodes standard base64 (with `=` padding); `None` on any bad input.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in input.bytes() {
        let v = rev[c as usize];
        if v == 255 {
            return None;
        }
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Routes one `/api/...` request to the platform (503 when disabled).
fn handle_api(
    platform: Option<&Platform>,
    mut request: tiny_http::Request,
) -> std::io::Result<()> {
    let (status, body) = match platform {
        Some(p) => {
            let mut req_body = Vec::new();
            request.as_reader().read_to_end(&mut req_body)?;
            let method = request.method().as_str().to_string();
            let path = request.url().to_string();
            p.handle_api(&method, &path, &req_body)
        }
        None => (503, r#"{"error":"platform API not enabled"}"#.to_string()),
    };
    let response = tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    request.respond(response)
}

/// Bridges one HTTP request to `git http-backend` CGI. `extra_env` is
/// added to the CGI child (and thus inherited by git hooks).
fn handle(
    root: PathBuf,
    mut request: tiny_http::Request,
    extra_env: &[(String, String)],
) -> std::io::Result<()> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    let mut body = Vec::new();
    request.as_reader().read_to_end(&mut body)?;

    let method = request.method().as_str().to_string();
    let content_type = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();

    let mut child = std::process::Command::new("git")
        .arg("http-backend")
        .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("GIT_PROJECT_ROOT", &root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("REQUEST_METHOD", &method)
        .env("CONTENT_TYPE", &content_type)
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(&body)?;
    let mut out = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout piped")
        .read_to_end(&mut out)?;
    child.wait()?;

    // Split the CGI response into headers and body.
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| out.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2)));
    let (head, rest) = match split {
        Some((h, b)) => (&out[..h], &out[b..]),
        None => (&[][..], &out[..]),
    };

    let mut status = 200;
    let mut headers = Vec::new();
    for line in String::from_utf8_lossy(head).lines() {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("Status") {
                status = v
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(200);
            } else if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                headers.push(h);
            }
        }
    }

    let mut response = tiny_http::Response::from_data(rest.to_vec()).with_status_code(status);
    for h in headers {
        response.add_header(h);
    }
    request.respond(response)?;
    Ok(())
}
