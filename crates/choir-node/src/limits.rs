//! Request accounting and admission control (D33): who did what, and how
//! much of it they may do.
//!
//! The node authenticates ([`crate::AuthTable`]) and authorizes per
//! repository ([`crate::acl`]), and until this module existed it did
//! nothing else. A second credential holder was invisible in the record —
//! an incident left no trace of which credential caused it — and unbounded
//! in consumption, because nothing counted requests at all.
//!
//! Two pieces, deliberately separate:
//!
//! - [`RequestLog`] writes one JSON object per served request. It is an
//!   observation about this node, not part of the ordered history anyone
//!   replays, so it lives beside the op log rather than in it — the same
//!   reasoning that keeps `lag.jsonl` separate.
//! - [`RateLimiter`] holds one token bucket per (user, [`Class`]) in
//!   memory and answers "may this request proceed, and if not, when should
//!   the caller come back".
//!
//! # What never reaches the log
//!
//! No header, no request body, no query string. [`Access::start`]
//! truncates the path at `?` before the struct is even built, so a token
//! smuggled into a query parameter cannot reach the file by any later
//! path. This is the property that lets the file be handed to whoever is
//! running an incident: a token that reaches a log is a token that has to
//! be rotated.
//!
//! # Lock discipline
//!
//! Both locks are leaf locks held for arithmetic or one `write_all`, never
//! across a socket write. [`Access::finish`] is called *after*
//! `Request::respond` has returned, so the log mutex is acquired when the
//! response is already on the wire and a slow reader cannot hold it. The
//! rate check runs on the request's own thread after authentication, never
//! on the accept loop, so a full bucket map cannot delay `accept`.
//!
//! # Examples
//!
//! ```
//! use choir_node::limits::{Class, RateLimiter};
//! use std::num::NonZeroU32;
//!
//! // Two API requests a minute, git unlimited.
//! let limiter = RateLimiter::new(NonZeroU32::new(2), None);
//! assert!(limiter.check("alice", Class::Api).is_none());
//! assert!(limiter.check("alice", Class::Api).is_none());
//!
//! // The third is refused, and says how many seconds to wait.
//! let retry = limiter.check("alice", Class::Api).expect("third is refused");
//! assert!(retry >= 1);
//!
//! // Buckets are per user, and an unset ceiling limits nothing.
//! assert!(limiter.check("bob", Class::Api).is_none());
//! assert!(limiter.check("alice", Class::Git).is_none());
//! ```
//!
//! The operator's guide to the ceilings this module enforces:
//!
#![doc = include_str!("../../../docs/operating/limits.md")]

use std::collections::HashMap;
use std::io::Write;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which ceiling a request is charged against.
///
/// Two classes rather than one because the costs are unrelated: a clone is
/// a single request that streams a whole pack, while a `POST
/// /api/submit-batch` is a single request carrying many operations. One
/// shared ceiling either throttles an ordinary fetch loop or leaves the
/// operation path effectively unlimited, so the operator sets each
/// independently and either may be left off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// Everything the node answers itself: the platform API, the browser
    /// page, the D30 repository-browsing pages, `llms.txt` and `sync.md`.
    ///
    /// Browsing pages shell out to `git` per request, so this bucket is
    /// not free even though it never reaches `git http-backend`. That is
    /// the reason to set a real number here rather than a huge one.
    Api,
    /// Git smart-HTTP: the requests handed to `git http-backend`.
    Git,
}

/// The class a URL is charged to, mirroring the accept loop's own routing
/// order (the API prefix is matched before the git fallback, so a path
/// that contains both spellings lands in the same bucket the router will
/// actually use).
#[must_use]
pub fn class_of(url: &str) -> Class {
    if url.starts_with("/api/") {
        return Class::Api;
    }
    if crate::repo_from_path(url).is_some() {
        Class::Git
    } else {
        Class::Api
    }
}

/// Per-user token buckets, one per (user, [`Class`]).
///
/// # Algorithm
///
/// A classic token bucket, refilled continuously rather than on a timer:
/// capacity is one minute's allowance, tokens accrue at
/// `per_minute / 60` per second, and each admitted request spends one.
/// A bucket is refilled lazily when it is read, so there is no background
/// thread and no periodic sweep — which is what lets this stay
/// synchronous, allocation-light, and free on an idle node.
///
/// Capacity equal to a full minute means an agent may burst a minute's
/// worth at once and then proceeds at the sustained rate, which is the
/// shape real agent traffic has: a batch of work, then a wait.
///
/// # Bounds
///
/// The map holds one entry per (authenticated user, class) that has been
/// seen. Usernames come from the operator's `--auth-file`, and the caller
/// only consults the limiter for authenticated users, so the map is
/// bounded by the credential count times two and cannot be grown by an
/// unauthenticated caller.
pub struct RateLimiter {
    api: Option<NonZeroU32>,
    git: Option<NonZeroU32>,
    buckets: Mutex<HashMap<(String, Class), Bucket>>,
}

/// One user's allowance for one class.
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// A limiter with the given requests-per-minute ceilings. `None` for a
    /// class means that class is not limited.
    ///
    /// The ceilings are [`NonZeroU32`] so that "limit to zero" — a value
    /// that refuses every request forever and has no sensible
    /// `Retry-After` — is not representable.
    #[must_use]
    pub fn new(api_per_minute: Option<NonZeroU32>, git_per_minute: Option<NonZeroU32>) -> Self {
        Self {
            api: api_per_minute,
            git: git_per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Whether any class is limited at all.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.api.is_some() || self.git.is_some()
    }

    /// Spends one token for `user` in `class`.
    ///
    /// `None` admits the request. `Some(seconds)` refuses it and is the
    /// `Retry-After` the caller should answer with: the whole seconds
    /// until one token has accrued, never less than one.
    pub fn check(&self, user: &str, class: Class) -> Option<u64> {
        self.check_at(user, class, Instant::now())
    }

    /// [`RateLimiter::check`] against a caller-supplied clock reading, so
    /// refill can be proved without sleeping through it.
    pub fn check_at(&self, user: &str, class: Class, now: Instant) -> Option<u64> {
        let per_minute = match class {
            Class::Api => self.api,
            Class::Git => self.git,
        }?;
        let capacity = f64::from(per_minute.get());
        let per_second = capacity / 60.0;

        let mut buckets = self.buckets.lock().expect("rate bucket lock");
        let bucket = buckets.entry((user.to_string(), class)).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        // Saturating: a clock reading older than the last one (which
        // `Instant` forbids, but a caller-supplied one does not) refills
        // nothing rather than draining the bucket.
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * per_second).min(capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return None;
        }
        let wait = (1.0 - bucket.tokens) / per_second;
        // Round up, and never answer "retry immediately": a client that
        // obeys a zero would spin.
        Some((wait.ceil() as u64).max(1))
    }
}

/// The default rotation threshold: 32 MiB, so the two generations this
/// keeps cost at most 64 MiB of disk.
pub const DEFAULT_LOG_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// An append-only, size-bounded record of every request the node served.
///
/// # Format
///
/// One JSON object per line — the same shape as `ops.jsonl` and
/// `lag.jsonl`, so the same tools read it — carrying `format_version`, the
/// wall-clock time, the authenticated user, the method, the redacted path,
/// the status, the response body size and the elapsed microseconds.
///
/// # Rotation
///
/// Size-bounded and single-generation. When the file passes
/// `max_bytes` it is renamed to `<path>.1`, replacing any previous `.1`,
/// and a fresh file is started. Disk is therefore bounded at roughly twice
/// `max_bytes` with no timer, no cron entry and no external logrotate — a
/// node that runs unattended for a year cannot fill its disk with this.
/// The cost of that simplicity is stated plainly: exactly two generations
/// are kept, so an operator who needs deeper history copies the file out
/// on their own schedule.
///
/// # Durability
///
/// One unbuffered `write_all` per request, no `fsync`. An incident log
/// that loses its tail to a buffer is worthless, and an `fsync` per
/// request would put a disk round-trip in every response path — this is an
/// observation, not the durable record.
pub struct RequestLog {
    sink: Mutex<Sink>,
}

/// The open file and what is known about it.
struct Sink {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
    max_bytes: u64,
    /// A broken log must not break the node, but it must be said once
    /// rather than once per request.
    complained: bool,
}

impl RequestLog {
    /// Opens `path` for append, creating it, and rotates at `max_bytes`.
    ///
    /// # Errors
    ///
    /// Propagates the failure to open the file. This is fatal at startup
    /// by design: an operator who asked for a request log and did not get
    /// one should find out before the node starts serving, not from the
    /// absence of evidence during an incident.
    pub fn open(path: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            sink: Mutex::new(Sink {
                path,
                file,
                written,
                max_bytes,
                complained: false,
            }),
        })
    }

    /// Appends one line, rotating first if the file is already over its
    /// bound. Never panics and never propagates: a request that was served
    /// is not un-served by a log failure.
    fn append(&self, line: &str) {
        let mut sink = self.sink.lock().expect("request log lock");
        if sink.written >= sink.max_bytes {
            let mut rotated = sink.path.clone().into_os_string();
            rotated.push(".1");
            // Rename then reopen. A failure at either step leaves the
            // current file in place and is reported like any other write
            // failure, so the worst case is an oversized log rather than a
            // lost one.
            match std::fs::rename(&sink.path, PathBuf::from(rotated)).and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&sink.path)
            }) {
                Ok(file) => {
                    sink.file = file;
                    sink.written = 0;
                }
                Err(e) => complain(&mut sink, &e),
            }
        }
        match sink.file.write_all(line.as_bytes()) {
            Ok(()) => sink.written += line.len() as u64,
            Err(e) => complain(&mut sink, &e),
        }
    }
}

/// Says once that the request log is not working. The reason, never the
/// path: that string names the operator's directories.
fn complain(sink: &mut Sink, error: &std::io::Error) {
    if !sink.complained {
        sink.complained = true;
        eprintln!("request log: writes are failing, so requests are going unrecorded: {error}");
    }
}

/// One request in flight: what it was, and when it started.
///
/// Built at the top of the request's thread and consumed by
/// [`Access::finish`] after the response has been written, so the recorded
/// duration covers the whole of the node's work including the socket
/// write.
pub struct Access {
    started: Instant,
    method: String,
    path: String,
}

impl Access {
    /// Captures the method and the path with its query string already
    /// removed.
    ///
    /// The truncation happens here rather than at write time on purpose:
    /// a query string that never enters the struct cannot leave it, so no
    /// later edit to the formatting can leak a token that was passed as a
    /// query parameter.
    #[must_use]
    pub fn start(request: &tiny_http::Request) -> Self {
        let url = request.url();
        Self {
            started: Instant::now(),
            method: request.method().as_str().to_string(),
            path: url.split('?').next().unwrap_or(url).to_string(),
        }
    }

    /// The path this access will record, query string already stripped.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Records the finished request. `outcome` is the status and response
    /// body size when the response was written, or the I/O error that
    /// stopped it — a truncated response is exactly the thing an incident
    /// needs recorded, so it is logged rather than dropped.
    ///
    /// A `None` log makes this a no-op, which is what a node started
    /// without `--request-log` pays.
    pub fn finish(
        self,
        log: Option<&RequestLog>,
        user: &str,
        outcome: &std::io::Result<(u16, u64)>,
    ) {
        let Some(log) = log else {
            return;
        };
        let elapsed = self.started.elapsed();
        let at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis();
        let mut entry = serde_json::json!({
            "format_version": 1,
            "at_unix_ms": at_unix_ms,
            "user": user,
            "method": self.method,
            "path": self.path,
            "us": u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        });
        match outcome {
            Ok((status, bytes)) => {
                entry["status"] = (*status).into();
                entry["bytes"] = (*bytes).into();
            }
            Err(e) => {
                // Status 0 = no complete response reached the client. The
                // error *kind* only: the message of an I/O error is not
                // guaranteed to be free of paths.
                entry["status"] = 0.into();
                entry["bytes"] = 0.into();
                entry["write_error"] = format!("{:?}", e.kind()).into();
            }
        }
        log.append(&format!("{entry}\n"));
    }
}

/// How many peer buckets [`PublicLimiter`] keeps. A power of two so the
/// index is a mask rather than a division, and small enough that the
/// whole table is one cache-friendly array rather than a map.
const PUBLIC_BUCKETS: usize = 256;

/// The window both counters are measured over.
const PUBLIC_WINDOW: Duration = Duration::from_secs(60);

/// Admission control for the routes that answer *before* a credential is
/// checked (the invite link and the public landing page).
///
/// [`RateLimiter`] cannot do this job, and the reason is written into its
/// own bounds note: its map holds one entry per key it has seen, which is
/// safe only because its keys come from the operator's auth file. Keyed on
/// something an anonymous caller chooses — an address, a header — the same
/// map is an unbounded allocation driven by whoever is calling, which is
/// the attack rather than the defence against it.
///
/// So this is a fixed table. `PUBLIC_BUCKETS` counters, indexed by a hash
/// of the peer address, plus one counter for the whole node; a fixed
/// window rather than a token bucket, because the state has to be a
/// number in a slot rather than a `(tokens, last)` pair per caller. Memory
/// is constant at startup and no caller can grow it.
///
/// The cost of that choice, stated rather than hidden: **peers collide**.
/// Two addresses landing in the same slot share an allowance, so a busy
/// neighbour can spend yours. That is acceptable here and would not be on
/// an authenticated route, because these routes exist to hand a stranger
/// one page and one redemption. Someone refused a join page retries in a
/// minute; someone refused a `git push` has a broken workflow.
///
/// The global counter is the one that matters under a real flood: it
/// bounds the node's total pre-auth work no matter how many addresses the
/// traffic arrives from, which is exactly the case per-peer limiting
/// cannot see.
///
/// It is also the only counter that applies behind a reverse proxy, which
/// is the supported deployment. There, every request arrives from
/// `127.0.0.1` and this node genuinely cannot tell two callers apart, so
/// it stops pretending to rather than putting the internet in one bucket
/// — see [`PublicLimiter::check_at`].
pub struct PublicLimiter {
    /// Requests one peer bucket may spend per window.
    per_peer: u32,
    /// Requests the whole pre-auth surface may spend per window.
    global: u32,
    counters: Mutex<PublicWindow>,
}

/// The counters for the window currently open.
struct PublicWindow {
    /// When the open window began. Reaching `PUBLIC_WINDOW` past this
    /// resets every counter at once.
    opened: Instant,
    peers: [u32; PUBLIC_BUCKETS],
    total: u32,
}

impl PublicLimiter {
    /// A limiter allowing `per_peer` requests per peer bucket and
    /// `global` across the whole pre-auth surface, each per minute.
    #[must_use]
    pub fn new(per_peer: u32, global: u32) -> Self {
        Self {
            per_peer,
            global,
            counters: Mutex::new(PublicWindow {
                opened: Instant::now(),
                peers: [0; PUBLIC_BUCKETS],
                total: 0,
            }),
        }
    }

    /// Spends one request for `peer`.
    ///
    /// `None` admits it. `Some(seconds)` refuses it and is the
    /// `Retry-After` to answer with: whole seconds until the window rolls,
    /// never less than one, so a client obeying it does not spin.
    pub fn check(&self, peer: &str) -> Option<u64> {
        self.check_at(peer, Instant::now())
    }

    /// [`PublicLimiter::check`] against a caller-supplied clock, so the
    /// window roll can be proved without sleeping through it.
    pub fn check_at(&self, peer: &str, now: Instant) -> Option<u64> {
        let mut window = self.counters.lock().expect("public limiter lock");
        // Saturating, because a caller-supplied reading may predate the
        // one before it; `Instant` forbids that but this signature does
        // not, and an underflow here would roll the window every call.
        if now.saturating_duration_since(window.opened) >= PUBLIC_WINDOW {
            window.opened = now;
            window.peers = [0; PUBLIC_BUCKETS];
            window.total = 0;
        }
        let remaining = PUBLIC_WINDOW.saturating_sub(now.saturating_duration_since(window.opened));
        // Round up and never say "retry immediately".
        let retry_after = u64::from(remaining.subsec_nanos() > 0) + remaining.as_secs();
        // Checked before either counter moves: a refused request must not
        // spend the allowance it was refused, or a caller hammering a full
        // bucket would hold it full forever and never see it drain.
        if window.total >= self.global {
            return Some(retry_after.max(1));
        }
        // A loopback peer is not a peer. The supported deployment binds
        // this node to `127.0.0.1` behind a TLS proxy, so every request
        // in the world arrives with the same address, and counting them
        // per address would put the entire internet in one bucket: the
        // per-peer ceiling would become a far *lower* global one, and a
        // single crawler would lock out every real invitee. That is a
        // protection turning into an outage.
        //
        // So when the node cannot tell callers apart it does not pretend
        // to. Per-client limiting belongs to whoever can see the client,
        // and in that deployment the proxy already does it
        // (`limit_req_zone $binary_remote_addr` in the rendered nginx
        // config). What stays is the node-wide ceiling, which is the
        // bound that still means something behind a proxy.
        //
        // A node bound directly to a public address sees real peers and
        // gets the per-peer limit as written.
        if !is_loopback(peer) {
            let slot = peer_slot(peer);
            if window.peers[slot] >= self.per_peer {
                return Some(retry_after.max(1));
            }
            window.peers[slot] = window.peers[slot].saturating_add(1);
        }
        window.total = window.total.saturating_add(1);
        None
    }
}

/// Whether an address is this machine talking to itself.
///
/// Compared as text because that is what the caller holds, and the two
/// spellings that matter are the two a `SocketAddr` produces. A hostname
/// never reaches here: [`peer_key`] renders an IP.
fn is_loopback(peer: &str) -> bool {
    peer == "127.0.0.1" || peer == "::1" || peer.starts_with("127.")
}

/// Which bucket a peer address falls in.
///
/// FNV-1a, hand-rolled for the same reason the tests hand-roll an
/// xorshift: this needs to spread addresses across a table, not to resist
/// anybody, and the standard `DefaultHasher` is explicitly not stable
/// across releases. A dependency for eight lines of arithmetic would be
/// the wrong trade.
///
/// Note what it hashes: whatever string the caller passes. The caller
/// passes the peer *address without its port*, because a port changes on
/// every connection and hashing it would give one client a fresh bucket
/// per request, which is a limiter that limits nothing.
fn peer_slot(peer: &str) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in peer.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // `PUBLIC_BUCKETS` is a power of two, so this is the low bits.
    (hash as usize) & (PUBLIC_BUCKETS - 1)
}

/// The peer address a [`PublicLimiter`] should key on, port stripped.
///
/// A request that carries no remote address at all is keyed as `"unknown"`
/// rather than admitted: an address the server could not read is not a
/// reason to stop counting.
#[must_use]
pub fn peer_key(request: &tiny_http::Request) -> String {
    request
        .remote_addr()
        .map_or_else(|| "unknown".to_string(), |addr| addr.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_admits_its_capacity_then_refuses() {
        let limiter = RateLimiter::new(NonZeroU32::new(3), None);
        let now = Instant::now();
        for _ in 0..3 {
            assert_eq!(limiter.check_at("alice", Class::Api, now), None);
        }
        let retry = limiter
            .check_at("alice", Class::Api, now)
            .expect("the fourth request in the same instant is refused");
        // 3/minute is one token every 20 seconds.
        assert_eq!(retry, 20);
    }

    #[test]
    fn tokens_come_back_at_the_sustained_rate() {
        let limiter = RateLimiter::new(NonZeroU32::new(60), None);
        let start = Instant::now();
        for _ in 0..60 {
            assert_eq!(limiter.check_at("alice", Class::Api, start), None);
        }
        assert!(limiter.check_at("alice", Class::Api, start).is_some());
        // 60/minute is one token a second; two seconds buys two requests
        // and no more.
        let later = start + Duration::from_secs(2);
        assert_eq!(limiter.check_at("alice", Class::Api, later), None);
        assert_eq!(limiter.check_at("alice", Class::Api, later), None);
        assert!(limiter.check_at("alice", Class::Api, later).is_some());
    }

    #[test]
    fn a_bucket_never_refills_past_a_minutes_allowance() {
        let limiter = RateLimiter::new(NonZeroU32::new(5), None);
        let start = Instant::now();
        assert_eq!(limiter.check_at("alice", Class::Api, start), None);
        // An hour of idleness does not buy an hour of burst.
        let later = start + Duration::from_secs(3600);
        for _ in 0..5 {
            assert_eq!(limiter.check_at("alice", Class::Api, later), None);
        }
        assert!(limiter.check_at("alice", Class::Api, later).is_some());
    }

    #[test]
    fn users_and_classes_do_not_share_a_bucket() {
        let limiter = RateLimiter::new(NonZeroU32::new(1), NonZeroU32::new(1));
        let now = Instant::now();
        assert_eq!(limiter.check_at("alice", Class::Api, now), None);
        assert!(limiter.check_at("alice", Class::Api, now).is_some());
        // Alice's exhausted API bucket says nothing about her git bucket,
        // and nothing at all about bob.
        assert_eq!(limiter.check_at("alice", Class::Git, now), None);
        assert_eq!(limiter.check_at("bob", Class::Api, now), None);
    }

    #[test]
    fn an_unset_class_is_not_limited() {
        let limiter = RateLimiter::new(None, NonZeroU32::new(1));
        let now = Instant::now();
        for _ in 0..1000 {
            assert_eq!(limiter.check_at("alice", Class::Api, now), None);
        }
        assert!(!RateLimiter::new(None, None).is_active());
        assert!(limiter.is_active());
    }

    #[test]
    fn urls_are_charged_the_way_the_router_routes_them() {
        assert_eq!(
            class_of("/owner/repo.git/info/refs?service=git-upload-pack"),
            Class::Git
        );
        assert_eq!(class_of("/owner/repo.git/git-receive-pack"), Class::Git);
        assert_eq!(class_of("/api/view"), Class::Api);
        assert_eq!(class_of("/api/log?from=0"), Class::Api);
        assert_eq!(class_of("/"), Class::Api);
        assert_eq!(class_of("/r/owner/repo/tree/main"), Class::Api);
        assert_eq!(class_of("/llms.txt"), Class::Api);
        // The API prefix wins, because the router matches it first.
        assert_eq!(class_of("/api/x.git/y"), Class::Api);
    }

    #[test]
    fn a_query_string_never_enters_the_record() {
        // `Access` cannot be built without a `tiny_http::Request`, so the
        // redaction is proved end to end in `tests/limits.rs` against a
        // served node. What is checkable here is that the two agree on
        // where the cut falls.
        let url = "/api/log?from=0&token=SECRET";
        assert_eq!(url.split('?').next(), Some("/api/log"));
    }

    #[test]
    fn a_public_peer_spends_its_window_and_is_told_when_to_return() {
        let limiter = PublicLimiter::new(3, 1000);
        let now = Instant::now();
        for _ in 0..3 {
            assert_eq!(limiter.check_at("198.51.100.7", now), None);
        }
        let retry = limiter
            .check_at("198.51.100.7", now)
            .expect("the fourth request in a window of three is refused");
        assert!(
            (1..=60).contains(&retry),
            "a refusal must name a wait inside the window, said {retry}"
        );
    }

    #[test]
    fn a_refused_request_does_not_spend_the_allowance_it_was_refused() {
        // Otherwise a caller hammering a full bucket keeps it full and it
        // never drains, which turns a one-minute limit into a permanent
        // ban held open by the traffic it is refusing.
        let limiter = PublicLimiter::new(1, 1000);
        let start = Instant::now();
        assert_eq!(limiter.check_at("203.0.113.9", start), None);
        for _ in 0..50 {
            assert!(limiter.check_at("203.0.113.9", start).is_some());
        }
        let next = start + PUBLIC_WINDOW;
        assert_eq!(
            limiter.check_at("203.0.113.9", next),
            None,
            "the window rolled and the peer is still refused"
        );
    }

    #[test]
    fn the_global_counter_bounds_a_flood_arriving_from_many_addresses() {
        // The case per-peer limiting cannot see: every request from a
        // different address, each one well inside its own allowance.
        let limiter = PublicLimiter::new(1000, 5);
        let now = Instant::now();
        for n in 0..5 {
            assert_eq!(limiter.check_at(&format!("192.0.2.{n}"), now), None);
        }
        assert!(
            limiter.check_at("192.0.2.99", now).is_some(),
            "a fresh address was admitted past the node-wide ceiling"
        );
    }

    #[test]
    fn the_port_is_not_part_of_the_key() {
        // A client gets a new source port per connection. Keying on it
        // would hand every request its own bucket, and the limiter would
        // admit everything while appearing to work.
        let one: std::net::SocketAddr = "198.51.100.7:41000".parse().expect("addr");
        let two: std::net::SocketAddr = "198.51.100.7:41001".parse().expect("addr");
        assert_eq!(
            peer_slot(&one.ip().to_string()),
            peer_slot(&two.ip().to_string())
        );
    }

    /// The deployment this node actually ships into binds to `127.0.0.1`
    /// behind a TLS proxy, so every request in the world arrives with one
    /// address. Counting those per address would put the whole internet
    /// in a single bucket and turn a 30-per-minute peer ceiling into a
    /// 30-per-minute *global* one — one crawler locking out every real
    /// invitee. The node-wide ceiling is what still means something
    /// there, and per-client limiting belongs to the proxy, which can
    /// see the client.
    #[test]
    fn a_proxied_node_is_not_rate_limited_into_an_outage() {
        let limiter = PublicLimiter::new(3, 1000);
        let now = Instant::now();
        for n in 0..500 {
            assert_eq!(
                limiter.check_at("127.0.0.1", now),
                None,
                "loopback request {n} was refused, so every client behind a proxy \
                 shares one tiny bucket"
            );
        }
        assert_eq!(limiter.check_at("::1", now), None);
    }

    /// ...but the node-wide ceiling still bounds it, so "not per-peer"
    /// does not mean "unlimited".
    #[test]
    fn a_proxied_node_is_still_bounded_node_wide() {
        let limiter = PublicLimiter::new(1000, 5);
        let now = Instant::now();
        for _ in 0..5 {
            assert_eq!(limiter.check_at("127.0.0.1", now), None);
        }
        assert!(
            limiter.check_at("127.0.0.1", now).is_some(),
            "a proxied node has no ceiling at all"
        );
    }

    #[test]
    fn the_slot_hash_spreads_addresses_across_the_table() {
        // A hash that returned a constant would still pass every test
        // above, and would silently make the per-peer limit a global one.
        let mut seen = std::collections::HashSet::new();
        for n in 0..=255u8 {
            seen.insert(peer_slot(&format!("198.51.100.{n}")));
        }
        assert!(
            seen.len() > 128,
            "256 addresses reached only {} of {PUBLIC_BUCKETS} slots",
            seen.len()
        );
    }
}
