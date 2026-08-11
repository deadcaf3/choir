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
pub mod provision;
pub mod reject;

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
    scheme: &'static str,
    /// Loopback secret handed to repo hooks via env so their callback to
    /// `/api/git-update` passes the auth gate without user credentials.
    internal_token: String,
    /// Trusted-keys file to watch, so `allowed_signers` tracks it
    /// without a restart. `None` = generated once at startup.
    keys_watch: Option<std::sync::Arc<KeysWatch>>,
}

/// A watched trusted-keys file and the mtime last folded into
/// `allowed_signers`.
struct KeysWatch {
    path: PathBuf,
    mtime: std::sync::Mutex<Option<std::time::SystemTime>>,
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
        Self::bind_full(root, "127.0.0.1", port, auth, None)
    }

    /// Full-control bind: address, port, auth, and optional TLS
    /// (PEM certificate chain + PEM private key).
    ///
    /// A non-loopback `addr` is refused without TLS — plaintext basic
    /// auth must never cross a real network (standing privacy rule:
    /// nothing leaves loopback without an explicit, protected choice).
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be bound, `root` cannot
    /// be created, the TLS material is invalid, or a non-loopback bind
    /// is requested without TLS.
    pub fn bind_full(
        root: &Path,
        addr: &str,
        port: u16,
        auth: Option<AuthTable>,
        tls: Option<(Vec<u8>, Vec<u8>)>,
    ) -> std::io::Result<Self> {
        let loopback = addr
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
        if !loopback && tls.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing non-loopback bind without TLS",
            ));
        }
        std::fs::create_dir_all(root)?;
        let scheme = if tls.is_some() { "https" } else { "http" };
        let server = match tls {
            Some((certificate, private_key)) => tiny_http::Server::https(
                (addr, port),
                tiny_http::SslConfig {
                    certificate,
                    private_key,
                },
            ),
            None => tiny_http::Server::http((addr, port)),
        }
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
            scheme,
            internal_token: choir_identity::ActorKey::generate().actor_id().to_hex(),
            keys_watch: None,
        })
    }

    /// Enables the platform API (`/api/submit`, `/api/view`) backed by
    /// `platform`. Call before [`Node::serve_forever`].
    pub fn enable_platform(&mut self, platform: Platform) {
        self.platform = Some(std::sync::Arc::new(platform));
    }

    /// Brings the bare repos back into agreement with the view before the
    /// node serves anything. See [`Platform::reconcile_git_refs`] for what
    /// it repairs and what it refuses to.
    ///
    /// Separate from [`Node::enable_platform`] and from
    /// [`Node::serve_forever`] so it is called deliberately: it writes git
    /// refs and can append compensating ops, which is not something a
    /// constructor should do behind a caller's back.
    pub fn reconcile_refs(&self) -> crate::platform::RefReconciliation {
        self.platform
            .as_ref()
            .map(|p| p.reconcile_git_refs(&self.root))
            .unwrap_or_default()
    }

    /// Watches the trusted-keys file and regenerates
    /// `<root>/.choir/allowed_signers` whenever its mtime moves, so
    /// registering a *signing* key is "append a line" — the same
    /// mechanism the platform registry already uses for submission
    /// keys, which until now diverged from push-certificate
    /// verification and left the two lists out of step.
    pub fn watch_keys_file(&mut self, path: PathBuf) {
        self.keys_watch = Some(std::sync::Arc::new(KeysWatch {
            mtime: std::sync::Mutex::new(
                std::fs::metadata(&path).and_then(|m| m.modified()).ok(),
            ),
            path,
        }));
    }

    /// Rewrites `allowed_signers` if the watched keys file changed.
    /// Runs on the accept loop, so rewrites never race each other.
    ///
    /// A malformed keys file leaves the existing signer list in place
    /// (a partial list would silently stop verifying somebody's
    /// pushes); the mtime is still recorded so the complaint is printed
    /// once per edit rather than once per request.
    fn refresh_allowed_signers(&self) {
        let Some(watch) = &self.keys_watch else {
            return;
        };
        let mtime = std::fs::metadata(&watch.path).and_then(|m| m.modified()).ok();
        let mut last = watch.mtime.lock().expect("keys mtime lock");
        if mtime.is_none() || mtime == *last {
            return;
        }
        *last = mtime;
        match parse_keys_file(&watch.path) {
            Ok(signers) => {
                if let Err(e) = write_allowed_signers(&self.root, &signers) {
                    eprintln!("allowed_signers: write failed: {e}");
                } else {
                    eprintln!("allowed_signers: reloaded ({} keys)", signers.len());
                }
                // Same file, same edit: pick up name bindings here too, so
                // binding a name to an already-trusted key does not wait
                // for the policy's failed-signature reload trigger.
                if let Some(platform) = &self.platform {
                    platform.set_key_names(&signers);
                }
            }
            Err(e) => eprintln!("allowed_signers: keys file unusable, keeping previous: {e}"),
        }
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
        //
        // Git applies no ref until this hook exits zero, so a refusal on
        // the third ref of a push has already left two ops in the durable
        // log for refs git will never create. Those refs are then stuck:
        // the pusher's `old` is git's absent value while the view holds
        // the stranded one, so every retry loses the CAS. Hence the
        // retraction pass over what this push already had accepted, which
        // is a compensating op rather than an erasure -- the log is
        // append-only, and an aborted push belongs in the history.
        let hook = path.join("hooks").join("pre-receive");
        std::fs::write(
            &hook,
            concat!(
                "#!/bin/sh\n",
                "# choir: route this push's ref updates through the platform sequencer.\n",
                "if [ -z \"$CHOIR_API\" ]; then cat >/dev/null; exit 0; fi\n",
                "post() {\n",
                "  payload=$(printf '{\"repo\":\"%s\",\"refname\":\"%s\",\"old\":\"%s\",\"new\":\"%s\",\"user\":\"%s\",\"cert_status\":\"%s\",\"signer\":\"%s\"}' \\\n",
                "    \"$CHOIR_REPO\" \"$3\" \"$1\" \"$2\" \"$CHOIR_USER\" \"$GIT_PUSH_CERT_STATUS\" \"$GIT_PUSH_CERT_SIGNER\")\n",
                "  curl -skf -X POST -H \"X-Choir-Internal: $CHOIR_INTERNAL\" \\\n",
                "    -d \"$payload\" \"$4\" >/dev/null\n",
                "}\n",
                // A file, not a shell variable: this has to survive being
                // read back line by line, and the accepted list is the
                // only record of what needs undoing.
                "done_refs=$(mktemp) || exit 1\n",
                "abort() {\n",
                "  while read -r a_old a_new a_ref; do\n",
                "    post \"$a_old\" \"$a_new\" \"$a_ref\" \"$CHOIR_ABORT\" ||\n",
                "      echo \"choir: could not retract $a_ref; the node's log now holds a ref \\\n",
                "this push did not create\" >&2\n",
                "  done < \"$done_refs\"\n",
                "}\n",
                "while read old new ref; do\n",
                "  if ! post \"$old\" \"$new\" \"$ref\" \"$CHOIR_API\"; then\n",
                "    echo \"choir: ref update rejected by sequencer: $ref\" >&2\n",
                "    abort\n",
                "    rm -f \"$done_refs\"\n",
                "    exit 1\n",
                "  fi\n",
                "  printf '%s %s %s\\n' \"$old\" \"$new\" \"$ref\" >> \"$done_refs\"\n",
                "done\n",
                "rm -f \"$done_refs\"\n",
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
            // Cheap stat between accepting a request and handling it, so
            // an edited keys file takes effect on *this* request: an
            // appended signing key becomes usable, and a newly bound
            // channel name becomes enforced, with no restart and no wait
            // for some later event.
            self.refresh_allowed_signers();
            // A writer that has failed a durability barrier refuses every
            // submission from then on. Staying up in that state is worse
            // than being down: process supervision only restarts a process
            // that *exits*, so the node would sit there looking healthy
            // to launchd while rejecting everything, and a transient fsync
            // error would become permanent downtime that reads as uptime.
            // Exiting hands it back to supervision, which restarts into
            // the same replay path the D20 flip proved with `kill -9`.
            if self.platform.as_ref().is_some_and(|p| p.durability_failed()) {
                eprintln!(
                    "choir: the op log is no longer durable, so this node has stopped \
                     accepting writes; exiting so supervision restarts it. Check the \
                     filesystem backing the log."
                );
                // EX_TEMPFAIL: the condition may well clear on restart.
                std::process::exit(75);
            }
            let root = self.root.clone();
            let auth = self.auth.clone();
            let platform = self.platform.clone();
            let internal_token = self.internal_token.clone();
            let port = self.port;
            let scheme = self.scheme;
            std::thread::spawn(move || {
                // Hook callbacks authenticate with the loopback secret
                // instead of user credentials.
                let internal_ok = (request.url().starts_with("/api/git-update")
                    || request.url().starts_with("/api/git-abort"))
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
                // The surface as plain text, for an agent that has never
                // seen choir. Behind auth like everything else; it
                // describes the node rather than exposing its contents.
                // The surface as plain text, and the sync contract it
                // points at. `llms.txt` naming a file only a cloner can
                // read would be worse than not naming it, so the
                // document a remote agent is told to follow is served
                // from the same place it is told about.
                if let Some(text) = match request.url() {
                    "/llms.txt" => Some(LLMS_TXT),
                    "/sync.md" => Some(SYNC_MD),
                    _ => None,
                } {
                    let response = tiny_http::Response::from_string(text).with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"text/plain; charset=utf-8"[..],
                        )
                        .expect("static header"),
                    );
                    let _ = request.respond(response);
                    return;
                }
                if request.url().starts_with("/api/") {
                    let base_url = format!("{scheme}://127.0.0.1:{port}");
                    let _ = handle_api(platform.as_deref(), &root, &base_url, &user, request);
                    return;
                }
                // Platform-enabled daemons pass the sequencer callback
                // into git's hook environment.
                let mut extra_env = Vec::new();
                if platform.is_some() {
                    if let Some(repo) = repo_from_path(request.url()) {
                        extra_env.push((
                            "CHOIR_API".to_string(),
                            format!("{scheme}://127.0.0.1:{port}/api/git-update"),
                        ));
                        extra_env.push((
                            "CHOIR_ABORT".to_string(),
                            format!("{scheme}://127.0.0.1:{port}/api/git-abort"),
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

/// One line of the trusted-keys file: a public key the node accepts, and
/// optionally the channel name its holder is allowed to speak as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    /// Operator-assigned channel name this key may act as, when the line
    /// carries one. `None` = key trusted, name unconstrained (the
    /// pre-existing behaviour, and what a bare hex line still means).
    pub name: Option<String>,
    /// Actor id (hex): the git push principal, and the `key_id` a
    /// signature carries. **Always derived from the key**, never from the
    /// name — so adding a name column cannot shift push attribution.
    pub actor_id: String,
    /// The raw ed25519 public key.
    pub key: [u8; 32],
}

/// Parses a trusted-keys file. One key per line, `#` comments and blank
/// lines skipped, in either form:
///
/// ```text
/// <64-char hex>            # trusted, speaks as any channel
/// <name> <64-char hex>     # trusted, bound to that channel name
/// ```
///
/// The name column is additive: files written before it existed parse
/// unchanged, and a key with no name keeps exactly its old permissions.
/// Binding is opt-in per key, so adding one line cannot lock anybody
/// else out.
///
/// # Errors
///
/// Filesystem failures, and [`std::io::ErrorKind::InvalidData`] for a
/// line that is not a valid ed25519 public key, or that binds one name
/// to two keys. An invalid line fails the whole parse: a partial signer
/// list would silently drop the ability to verify somebody's pushes.
pub fn parse_keys_file(path: &Path) -> std::io::Result<Vec<TrustedKey>> {
    let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);
    let mut registry = choir_identity::Registry::new();
    let mut out: Vec<TrustedKey> = Vec::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, hex) = match line.rsplit_once(char::is_whitespace) {
            Some((name, hex)) => (Some(name.trim().to_string()), hex),
            None => (None, line),
        };
        let bytes = platform::hex_decode(hex)
            .filter(|b| b.len() == 32)
            .ok_or_else(|| {
                invalid("keys file lines are `<hex>` or `<name> <hex>`, 64 hex chars".to_string())
            })?;
        // One name, one key. Two keys sharing a name would make the
        // binding meaningless in exactly the direction it exists to
        // prevent: either holder could speak as that channel.
        if let Some(name) = &name {
            if out.iter().any(|k| k.name.as_ref() == Some(name)) {
                return Err(invalid(format!("keys file binds {name:?} to more than one key")));
            }
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        let actor_id = registry
            .register(&key)
            .map_err(|e| invalid(format!("{e:?}")))?;
        out.push(TrustedKey {
            name,
            actor_id: actor_id.to_hex(),
            key,
        });
    }
    Ok(out)
}

/// Writes `<root>/.choir/allowed_signers` — the file git's ssh signature
/// verification checks push certificates against. One line per actor:
/// principal (the actor id) followed by the OpenSSH public key.
///
/// # Errors
///
/// Propagates filesystem failures.
pub fn write_allowed_signers(root: &Path, keys: &[TrustedKey]) -> std::io::Result<PathBuf> {
    let dir = root.join(".choir");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("allowed_signers");
    let mut contents = String::new();
    for k in keys {
        // Principal stays the actor id even when the line carries a name:
        // push attribution (`key/<principal>`) is a property of the key,
        // and rewriting it would silently reattribute pushes.
        contents.push_str(&format!("{} {}\n", k.actor_id, ssh_ed25519_pubkey(&k.key)));
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

/// The agent-facing surface as plain text, generated from
/// `crates/choir-cli/src/surface.rs` and checked for staleness by
/// `choir-cli/tests/it/surface.rs`. Included rather than depended on: the
/// node has no business linking the CLI, and a generated file with a
/// staleness test is the cheaper coupling.
const LLMS_TXT: &str = include_str!("llms.txt");

/// The sync contract, served at `/sync.md`. Hand-authored, unlike
/// `llms.txt`, and included from the repository root so the served copy
/// and the committed one cannot disagree.
const SYNC_MD: &str = include_str!("../../../SYNC.md");

/// Routes one `/api/...` request to the platform (503 when disabled).
fn handle_api(
    platform: Option<&Platform>,
    root: &Path,
    base_url: &str,
    user: &str,
    mut request: tiny_http::Request,
) -> std::io::Result<()> {
    let (status, body) = match platform {
        Some(p) => {
            let mut req_body = Vec::new();
            request.as_reader().read_to_end(&mut req_body)?;
            let method = request.method().as_str().to_string();
            let path = request.url().to_string();
            if (method.as_str(), path.as_str()) == ("POST", "/api/workspace") {
                provision::create_workspace(root, p, base_url, &format!("git/{user}"), &req_body)
            } else if (method.as_str(), path.as_str()) == ("GET", "/api/ref-agreement") {
                // Routed here rather than inside the platform for the
                // same reason as `/api/workspace`: it needs the repo
                // root, which the platform does not hold.
                let findings = p.survey_git_refs(root);
                let body = serde_json::json!({
                    "format_version": 1,
                    "agree": findings.is_empty(),
                    "findings": findings.iter().map(platform::RefFinding::to_json)
                        .collect::<Vec<_>>(),
                });
                (200, body.to_string())
            } else {
                p.handle_api(&method, &path, &req_body)
            }
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
