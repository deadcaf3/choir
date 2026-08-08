//! L3 node daemon: a minimal git smart-HTTP server (plan.md L3, D12).
//!
//! v1 wraps `git http-backend` (git's own CGI) over bare repositories, the
//! same shape as Tangled's knot: the daemon is a thin, self-hostable shell
//! over git plumbing, and platform behavior (sequencer, queue, identity)
//! layers on top. ForgeMark benchmarks this surface directly.
//!
//! **No authentication yet**: L8 identity is a later phase; run only on
//! localhost or behind an SSH tunnel until then.

use std::io::Read;
use std::path::{Path, PathBuf};

/// A running node daemon serving repos under a root directory.
pub struct Node {
    root: PathBuf,
    server: std::sync::Arc<tiny_http::Server>,
    port: u16,
}

impl Node {
    /// Binds to `127.0.0.1:port` (0 = ephemeral) over `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be bound or `root` cannot be
    /// created.
    pub fn bind(root: &Path, port: u16) -> std::io::Result<Self> {
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
        })
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
                .success();
        if ok {
            Ok(())
        } else {
            Err(std::io::Error::other("git init/config failed"))
        }
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
            std::thread::spawn(move || {
                let _ = handle(root, request);
            });
        }
    }

    /// Handle for stopping the accept loop (used by tests).
    pub fn unblock(&self) {
        self.server.unblock();
    }
}

/// Bridges one HTTP request to `git http-backend` CGI.
fn handle(root: PathBuf, mut request: tiny_http::Request) -> std::io::Result<()> {
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
