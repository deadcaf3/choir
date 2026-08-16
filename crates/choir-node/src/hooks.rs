//! Outbound ref-landed webhooks (D32): "something moved, go run this".
//!
//! The node speaks to git and to its own clients; until this module it
//! told nobody else when a ref landed. A subscription is a line in an
//! operator-owned file — `<repo:refname pattern> <url> <secret>
//! [allow-private]` — reloaded on mtime like the keys and ACL files, and
//! deliberately *not* a `ViewOp`: who may be notified is configuration,
//! not sequenced state, and putting it in the log would make it
//! replayable and unforgettable.
//!
//! # Invariant 5 is the whole design
//!
//! A webhook is a request to an address someone else chose, so its
//! latency is unbounded by construction. The sequencer's writer thread
//! therefore does exactly one thing with an event: [`Hooks::offer`],
//! which is a non-blocking `try_send` into a bounded queue. Reloading the
//! subscription file, matching patterns, resolving and vetting the
//! address, running `curl` and retrying all happen on the delivery
//! thread this module spawns. When the queue is full the event is
//! **dropped and counted**, never blocked on: a receiver that stops
//! answering must not be able to stall op admission, and blocking the
//! writer is the one failure this module exists to make impossible.
//!
//! # Best-effort, counted, never silent
//!
//! Delivery is best-effort, not at-least-once. At-least-once needs a
//! durable spool with its own fsync discipline, and the only durable
//! writer here is the sequencer — the thread the design keeps out of the
//! delivery path. So every attempt, every refusal and every queue-full
//! drop is appended to the delivery log (`<root>/.choir/hooks.jsonl`), a
//! production observation beside `lag.jsonl`: not hashed, not replayed,
//! never part of the ordered history. A receiver that may not miss a ref
//! polls `GET /api/log?from=N`, which is what agents already do.
//!
//! # Talking to an address someone else chose
//!
//! This is the node's first outbound request to an operator-supplied
//! target, so SSRF is in scope. Vetting refuses non-`http(s)` schemes,
//! resolves the host and refuses loopback, private, carrier-NAT,
//! link-local (which is what covers the cloud metadata service at
//! `169.254.169.254`), unique-local, unspecified, broadcast and
//! multicast addresses unless the subscription says `allow-private`; the
//! vetted address is then pinned into `curl --resolve` so a second DNS
//! answer cannot land somewhere else, redirects are refused outright
//! because a redirect is the standard way past exactly this check, and a
//! non-loopback target must be `https`, since a bearer secret in clear
//! over the internet is the same as no secret.

use std::io::Write;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Events held in memory while the delivery thread works. Small on
/// purpose: a full queue is a receiver problem the operator should see
/// in the delivery log, not a backlog the node quietly grows a heap for.
pub const QUEUE_CAPACITY: usize = 256;

/// Attempts per matching subscription before a delivery is abandoned.
const MAX_ATTEMPTS: u32 = 3;

/// Wait between attempts. Multiplied by the attempt number, so the three
/// attempts of one delivery span well under a second: this thread is the
/// only one delivering, and a long backoff is paid for in dropped events.
const RETRY_BACKOFF: Duration = Duration::from_millis(100);

/// How long the worker waits for an event before looking around. Bounded
/// so a queue-full drop reaches the delivery log even when the drop was
/// the last thing that ever happened.
const IDLE_POLL: Duration = Duration::from_millis(200);

/// Per-attempt ceiling on `curl`. A receiver that never answers costs
/// this much and no more.
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// A ref that moved, as the writer thread saw it.
///
/// Built inside the sequencer's `accepted()` and immediately handed to
/// [`Hooks::offer`]; every field is already in hand there, so building it
/// costs no lookup.
#[derive(Debug, Clone)]
pub struct RefEvent {
    /// The view's ref key, `<repo>:<refname>` for git-derived refs. This
    /// is what subscription patterns match, so a pattern means the same
    /// thing here as in `--protected-refs`.
    pub key: String,
    /// Old git oid in hex, `None` when the ref did not exist.
    pub old: Option<String>,
    /// New git oid in hex, `None` when the ref was deleted.
    pub new: Option<String>,
    /// Log position of the entry that moved it.
    pub seq: u64,
    /// Content hash of that entry, hex. Unique per event, so a receiver
    /// can discard a delivery it has already acted on.
    pub entry: String,
    /// Attribution channel the op was signed on.
    pub actor: String,
    /// Signing key id, when the op carried an author signature.
    pub key_id: Option<String>,
}

impl RefEvent {
    /// Repository half of the ref key, when it has one. The platform API
    /// can set a ref name with no `:`, in which case there is no repo.
    fn repo(&self) -> Option<&str> {
        self.key.split_once(':').map(|(repo, _)| repo)
    }

    /// Ref name half of the ref key, or the whole key when it has no
    /// repository prefix.
    fn refname(&self) -> &str {
        self.key
            .split_once(':')
            .map_or(self.key.as_str(), |(_, r)| r)
    }

    fn body(&self) -> String {
        serde_json::json!({
            "format_version": 1,
            "event": "ref-landed",
            "repo": self.repo(),
            "ref": self.refname(),
            "ref_key": self.key,
            "old": self.old,
            "new": self.new,
            "seq": self.seq,
            "entry": self.entry,
            "actor": self.actor,
            "key_id": self.key_id,
        })
        .to_string()
    }
}

/// One line of the subscription file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Subscription {
    /// `<repo>:<refname>`, exact or with a single trailing `*`.
    pattern: String,
    url: String,
    secret: String,
    allow_private: bool,
}

impl Subscription {
    /// The `--protected-refs` rule, copied rather than reinvented: a
    /// trailing `*` is a prefix, anything else is exact.
    fn matches(&self, key: &str) -> bool {
        match self.pattern.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == self.pattern,
        }
    }
}

/// Parses one non-comment line.
fn parse_line(line: &str) -> Result<Subscription, String> {
    let mut fields = line.split_whitespace();
    let (Some(pattern), Some(url), Some(secret)) = (fields.next(), fields.next(), fields.next())
    else {
        return Err("each line is `<repo:refname> <url> <secret> [allow-private]`".to_string());
    };
    let mut allow_private = false;
    for extra in fields {
        match extra {
            "allow-private" => allow_private = true,
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    // The secret reaches curl through a quoted line of its stdin config,
    // so a quote, a backslash or a newline in it could grow a second
    // config directive. Refused here rather than escaped: an operator
    // mints these with `openssl rand -hex 32`.
    if secret.contains(['"', '\\']) || secret.chars().any(char::is_control) {
        return Err("a secret may not contain a quote, a backslash or a control character".into());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("a target url must start with http:// or https://".to_string());
    }
    Ok(Subscription {
        pattern: pattern.to_string(),
        url: url.to_string(),
        secret: secret.to_string(),
        allow_private,
    })
}

/// Reads the whole subscription file, or refuses all of it.
///
/// Partial parsing is the failure mode to avoid: half a subscription
/// file is a node that silently stops notifying somebody, which looks
/// exactly like a receiver that stopped caring.
fn load(path: &Path) -> Result<Vec<Subscription>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut subscriptions = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        subscriptions.push(parse_line(line).map_err(|e| format!("line {}: {e}", number + 1))?);
    }
    Ok(subscriptions)
}

/// A vetted target: where `curl` is allowed to connect, and to which
/// address specifically.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    host: String,
    port: u16,
    addr: IpAddr,
    https: bool,
}

/// Addresses a webhook may not reach without `allow-private`.
///
/// The list is explicit because `IpAddr::is_global` is unstable, and
/// because each entry is a documented SSRF target rather than a tidy
/// category: link-local is where cloud metadata services live, and
/// carrier-grade NAT space is routable-looking but internal.
fn is_private_address(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // 100.64.0.0/10, carrier-grade NAT.
                || (octets[0] == 100 && (64..128).contains(&octets[1]))
                // 0.0.0.0/8, "this network".
                || octets[0] == 0
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_address(IpAddr::V4(v4));
            }
            let segments = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7, unique local.
                || segments[0] & 0xfe00 == 0xfc00
                // fe80::/10, link local.
                || segments[0] & 0xffc0 == 0xfe80
        }
    }
}

/// Resolves `url` and decides whether this node may talk to it.
///
/// Every resolved address must pass, not merely the one that gets used:
/// a name that answers with one public and one internal address is the
/// interesting case, and picking the acceptable half of that answer
/// would be a check that congratulates itself.
fn vet(url: &str, allow_private: bool) -> Result<Target, String> {
    let (https, rest) = match url.split_once("://") {
        Some(("https", rest)) => (true, rest),
        Some(("http", rest)) => (false, rest),
        _ => return Err("a target url must start with http:// or https://".to_string()),
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    if authority.is_empty() {
        return Err("the target url names no host".to_string());
    }
    // Credentials in the url would have to be split back out before the
    // host could be vetted, and there is nothing they buy that the
    // subscription secret does not.
    if authority.contains('@') {
        return Err("a target url may not carry credentials".to_string());
    }
    let (host, port) = match authority.strip_prefix('[') {
        // Bracketed IPv6 literal, with or without a port.
        Some(bracketed) => {
            let (address, tail) = bracketed
                .split_once(']')
                .ok_or_else(|| "unterminated [ipv6] host".to_string())?;
            let port = match tail.strip_prefix(':') {
                Some(port) => port
                    .parse::<u16>()
                    .map_err(|_| "the target url has a bad port".to_string())?,
                None => default_port(https),
            };
            (address.to_string(), port)
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_string(),
                port.parse::<u16>()
                    .map_err(|_| "the target url has a bad port".to_string())?,
            ),
            None => (authority.clone(), default_port(https)),
        },
    };
    if host.is_empty() {
        return Err("the target url names no host".to_string());
    }
    let resolved: Vec<IpAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("{host} does not resolve: {e}"))?
        .map(|socket| socket.ip())
        .collect();
    let Some(addr) = resolved.first().copied() else {
        return Err(format!("{host} resolves to no address"));
    };
    if !allow_private {
        if let Some(refused) = resolved.iter().find(|ip| is_private_address(**ip)) {
            return Err(format!(
                "{host} resolves to {refused}, which is loopback, private, link-local or \
                 otherwise internal; add `allow-private` to this subscription if that is \
                 deliberate"
            ));
        }
    }
    // The secret is a bearer credential, so it may only cross a network
    // it cannot be read on. Loopback is the exception, not https.
    if !https && !addr.is_loopback() {
        return Err(format!(
            "{host} is not loopback, so this subscription must use https: an http delivery \
             carries the subscription secret in clear"
        ));
    }
    Ok(Target {
        host,
        port,
        addr,
        https,
    })
}

/// The address as `--resolve` spells it: an IPv6 literal is bracketed
/// there, an IPv4 one is bare.
fn resolve_form(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

fn default_port(https: bool) -> u16 {
    if https {
        443
    } else {
        80
    }
}

/// Handle held by the platform. Cloneable so the writer thread's copy
/// costs nothing; the delivery thread lives as long as the process.
#[derive(Clone)]
pub struct Hooks {
    tx: SyncSender<RefEvent>,
    dropped: Arc<AtomicU64>,
}

impl Hooks {
    /// Starts the delivery thread for the subscriptions in `config`,
    /// writing delivery records to `log`.
    ///
    /// # Errors
    ///
    /// Returns a message when the subscription file cannot be read or
    /// does not parse. Fatal at startup by design: an operator who asked
    /// for webhooks and got a typo should be told now, not by silence
    /// later.
    pub fn start(config: PathBuf, log: PathBuf) -> Result<Self, String> {
        let subscriptions = load(&config)?;
        let count = subscriptions.len();
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker = Worker {
            rx,
            mtime: std::fs::metadata(&config).and_then(|m| m.modified()).ok(),
            config,
            subscriptions,
            log,
            dropped: Arc::clone(&dropped),
            reported_drops: 0,
        };
        std::thread::Builder::new()
            .name("choir-hooks".to_string())
            .spawn(move || worker.run())
            .map_err(|e| format!("could not start the webhook delivery thread: {e}"))?;
        eprintln!("webhooks enabled ({count} subscriptions)");
        Ok(Self { tx, dropped })
    }

    /// Offers an event to the delivery thread. **Never blocks**: this
    /// runs on the sequencer's writer thread, where waiting on a
    /// receiver would put a stranger's latency in front of op admission
    /// (invariant 5). A full queue drops the event and counts it.
    pub fn offer(&self, event: RefEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Events dropped because the queue was full (or the delivery thread
    /// was gone). Also written to the delivery log; exposed for tests
    /// and for anything that wants the count without parsing it back.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

struct Worker {
    rx: Receiver<RefEvent>,
    config: PathBuf,
    mtime: Option<SystemTime>,
    subscriptions: Vec<Subscription>,
    log: PathBuf,
    dropped: Arc<AtomicU64>,
    reported_drops: u64,
}

impl Worker {
    fn run(mut self) {
        loop {
            match self.rx.recv_timeout(IDLE_POLL) {
                Ok(event) => {
                    self.refresh();
                    self.dispatch(&event);
                    self.report_drops();
                }
                Err(RecvTimeoutError::Timeout) => self.report_drops(),
                // Every sender is gone: the platform is shutting down.
                Err(RecvTimeoutError::Disconnected) => {
                    self.report_drops();
                    return;
                }
            }
        }
    }

    /// Rereads the subscription file when its mtime moved — the keys and
    /// ACL discipline, on the delivery thread rather than the accept
    /// loop, because that is where the file is used and nothing else may
    /// pay for reading it.
    ///
    /// A malformed edit keeps the previous subscriptions and complains
    /// once per edit, for the reason the ACL gives: half a policy file
    /// silently changes who is notified.
    fn refresh(&mut self) {
        let mtime = std::fs::metadata(&self.config)
            .and_then(|m| m.modified())
            .ok();
        if mtime.is_none() || mtime == self.mtime {
            return;
        }
        self.mtime = mtime;
        match load(&self.config) {
            Ok(subscriptions) => {
                eprintln!("webhooks: reloaded ({} subscriptions)", subscriptions.len());
                self.subscriptions = subscriptions;
            }
            Err(e) => eprintln!("webhooks: file unusable, keeping previous: {e}"),
        }
    }

    fn dispatch(&mut self, event: &RefEvent) {
        let matched: Vec<Subscription> = self
            .subscriptions
            .iter()
            .filter(|subscription| subscription.matches(&event.key))
            .cloned()
            .collect();
        for subscription in matched {
            self.deliver(event, &subscription);
        }
    }

    fn deliver(&mut self, event: &RefEvent, subscription: &Subscription) {
        let target = match vet(&subscription.url, subscription.allow_private) {
            Ok(target) => target,
            Err(reason) => {
                self.record(serde_json::json!({
                    "format_version": 1,
                    "event": "refused",
                    "seq": event.seq,
                    "entry": event.entry,
                    "ref_key": event.key,
                    "url": subscription.url,
                    "reason": reason,
                }));
                return;
            }
        };
        let body = event.body();
        for attempt in 1..=MAX_ATTEMPTS {
            let outcome = post(&target, subscription, &body, &self.log);
            let delivered = matches!(&outcome, Ok(status) if (200..300).contains(status));
            let record = match &outcome {
                Ok(status) if (200..300).contains(status) => serde_json::json!({
                    "format_version": 1,
                    "event": "delivered",
                    "seq": event.seq,
                    "entry": event.entry,
                    "ref_key": event.key,
                    "url": subscription.url,
                    "attempt": attempt,
                    "status": status,
                }),
                Ok(status) => serde_json::json!({
                    "format_version": 1,
                    "event": "failed",
                    "seq": event.seq,
                    "entry": event.entry,
                    "ref_key": event.key,
                    "url": subscription.url,
                    "attempt": attempt,
                    "status": status,
                    "final": attempt == MAX_ATTEMPTS,
                }),
                Err(error) => serde_json::json!({
                    "format_version": 1,
                    "event": "failed",
                    "seq": event.seq,
                    "entry": event.entry,
                    "ref_key": event.key,
                    "url": subscription.url,
                    "attempt": attempt,
                    "error": error,
                    "final": attempt == MAX_ATTEMPTS,
                }),
            };
            self.record(record);
            // Anything that fills the queue does it while this thread is
            // inside an attempt, so the count is flushed here rather than
            // only after the whole delivery: a receiver that never
            // answers must not also delay the news that it is costing
            // events.
            self.report_drops();
            if delivered {
                return;
            }
            if attempt < MAX_ATTEMPTS {
                std::thread::sleep(RETRY_BACKOFF * attempt);
            }
        }
    }

    /// Writes the queue-full count to the delivery log when it moves.
    ///
    /// The drop happens on the writer thread, which may not touch a
    /// file, so it leaves an atomic counter behind and this thread turns
    /// it into a record. Reported as a running total plus the increment,
    /// so a truncated log still says how many were lost overall.
    fn report_drops(&mut self) {
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped == self.reported_drops {
            return;
        }
        let since = dropped - self.reported_drops;
        self.reported_drops = dropped;
        self.record(serde_json::json!({
            "format_version": 1,
            "event": "dropped",
            "dropped": since,
            "dropped_total": dropped,
            "queue_capacity": QUEUE_CAPACITY,
        }));
        eprintln!(
            "webhooks: dropped {since} event(s) with a full queue ({dropped} total); a receiver \
             is slower than this node produces refs"
        );
    }

    fn record(&self, value: serde_json::Value) {
        if let Some(parent) = self.log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .and_then(|mut file| writeln!(file, "{value}"));
        if let Err(e) = appended {
            // Nowhere left to record it but the operator's console. A
            // delivery record that cannot be written is itself the
            // operational fact.
            eprintln!("webhooks: could not write {}: {e}", self.log.display());
        }
    }
}

/// One `curl` POST to a vetted target. Returns the HTTP status, or a
/// description of why there was none.
///
/// The secret goes in through curl's stdin config rather than argv,
/// because argv is readable by every process on the host; the body goes
/// through a 0600 temporary file for the same reason it does in the CLI:
/// a long argument list is not a place for content.
fn post(
    target: &Target,
    subscription: &Subscription,
    body: &str,
    log: &Path,
) -> Result<u16, String> {
    let scratch = log.parent().unwrap_or_else(|| Path::new("."));
    let body_path = scratch.join(format!("hook-delivery-{}.json", std::process::id()));
    write_private(&body_path, body).map_err(|e| format!("could not stage the payload: {e}"))?;
    let mut command = std::process::Command::new("curl");
    command
        .args([
            "-s",
            "-w",
            "\n%{http_code}",
            "-o",
            "/dev/null",
            "-X",
            "POST",
            // A redirect to somewhere the vetting refused is the standard
            // way past the vetting, so there are no redirects.
            "--max-redirs",
            "0",
            "--proto",
            "=http,https",
            "--max-time",
            &REQUEST_TIMEOUT_SECS.to_string(),
            // Connect to the address that was vetted, so a second DNS
            // answer cannot send this somewhere else.
            "--resolve",
            &format!(
                "{}:{}:{}",
                target.host,
                target.port,
                resolve_form(target.addr)
            ),
            "-H",
            "Content-Type: application/json",
            "--config",
            "-",
            "--data-binary",
        ])
        .arg(format!("@{}", body_path.display()))
        .arg("--url")
        .arg(&subscription.url)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // curl's diagnostics can name the operator's target host; the
        // delivery record already says which subscription this was.
        .stderr(std::process::Stdio::null());
    let result = run(&mut command, &subscription.secret);
    let _ = std::fs::remove_file(&body_path);
    result
}

fn run(command: &mut std::process::Command, secret: &str) -> Result<u16, String> {
    let mut child = command
        .spawn()
        .map_err(|_| "could not start curl".to_string())?;
    // parse_line refused quotes and backslashes in a secret, so this
    // quoted line cannot grow a second config directive.
    let config = format!("header = \"X-Choir-Hook-Secret: {secret}\"\n");
    child
        .stdin
        .take()
        .expect("piped curl stdin")
        .write_all(config.as_bytes())
        .map_err(|_| "could not configure curl".to_string())?;
    let output = child
        .wait_with_output()
        .map_err(|_| "could not wait for curl".to_string())?;
    if !output.status.success() {
        return Err(format!(
            "curl exited {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (_, status) = text
        .rsplit_once('\n')
        .ok_or_else(|| "curl returned no HTTP status".to_string())?;
    status
        .trim()
        .parse::<u16>()
        .map_err(|_| "curl returned an invalid HTTP status".to_string())
}

/// Writes `contents` readable by this user only. The payload names refs
/// and oids, and it sits beside the operator's state directory.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription(pattern: &str) -> Subscription {
        Subscription {
            pattern: pattern.to_string(),
            url: "https://example.invalid/hook".to_string(),
            secret: "s".to_string(),
            allow_private: false,
        }
    }

    #[test]
    fn a_trailing_star_is_a_prefix_and_anything_else_is_exact() {
        let exact = subscription("owner/repo:refs/heads/main");
        assert!(exact.matches("owner/repo:refs/heads/main"));
        assert!(!exact.matches("owner/repo:refs/heads/main-2"));
        assert!(!exact.matches("other/repo:refs/heads/main"));

        let prefix = subscription("owner/repo:refs/heads/*");
        assert!(prefix.matches("owner/repo:refs/heads/main"));
        assert!(prefix.matches("owner/repo:refs/heads/feature/x"));
        assert!(!prefix.matches("owner/repo:refs/tags/v1"));
        assert!(!prefix.matches("other/repo:refs/heads/main"));
    }

    #[test]
    fn a_line_needs_a_pattern_a_url_and_a_secret() {
        let parsed = parse_line("o/r:refs/heads/main https://example.invalid/h abc123").unwrap();
        assert_eq!(parsed.url, "https://example.invalid/h");
        assert_eq!(parsed.secret, "abc123");
        assert!(!parsed.allow_private);

        assert!(parse_line("o/r:refs/heads/main https://example.invalid/h").is_err());
        assert!(parse_line("o/r:refs/heads/main ftp://example.invalid/h s").is_err());
        assert!(parse_line("o/r:x https://example.invalid/h s allow-everything").is_err());
        assert!(parse_line("o/r:x https://example.invalid/h se\"cret").is_err());
    }

    #[test]
    fn allow_private_is_the_only_way_to_reach_an_internal_address() {
        assert!(
            parse_line("o/r:x http://localhost:1/h s allow-private")
                .unwrap()
                .allow_private
        );
        assert!(vet("http://127.0.0.1:9/hook", false).is_err());
        assert!(vet("http://127.0.0.1:9/hook", true).is_ok());
    }

    #[test]
    fn the_metadata_service_and_its_neighbours_are_internal() {
        // The addresses an SSRF is usually aimed at.
        for address in [
            "169.254.169.254", // cloud metadata
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1", // carrier-grade NAT
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1", // loopback wearing an IPv6 hat
        ] {
            let addr: IpAddr = address.parse().expect("test address parses");
            assert!(is_private_address(addr), "{address} should be internal");
        }
        for address in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            let addr: IpAddr = address.parse().expect("test address parses");
            assert!(!is_private_address(addr), "{address} should be reachable");
        }
    }

    #[test]
    fn a_non_loopback_target_may_not_carry_the_secret_in_clear() {
        // Private, allowed by the subscription, and still refused over
        // http because it is not loopback.
        let refused = vet("http://10.0.0.1/hook", true).unwrap_err();
        assert!(refused.contains("https"), "{refused}");
    }

    #[test]
    fn a_url_is_split_into_a_host_and_a_port_before_it_is_vetted() {
        let target = vet("http://127.0.0.1:8080/deep/path?query=1", true).unwrap();
        assert_eq!(target.host, "127.0.0.1");
        assert_eq!(target.port, 8080);
        assert!(!target.https);

        let bracketed = vet("http://[::1]:9000/hook", true).unwrap();
        assert_eq!(bracketed.host, "::1");
        assert_eq!(bracketed.port, 9000);

        assert!(vet("https://user:pass@example.invalid/h", false).is_err());
        assert!(vet("gopher://example.invalid/h", false).is_err());
    }
}
