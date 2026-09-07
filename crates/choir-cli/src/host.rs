//! `choir host` — a fresh machine to a running node, in one command.
//!
//! Everything this does was already possible: `choir init`, then a
//! certificate, then `choir node install`, then a wait, then `choir repo
//! create`, then `choir invite`. Each is small. The problem was never
//! any one of them — it was that the *order* differs by what the machine
//! is, and the machine's owner is the one person who cannot be expected
//! to know which order applies to them before they have run anything.
//!
//! So there are three shapes and one command:
//!
//! | you have | you run | you get |
//! |:--|:--|:--|
//! | a laptop | `choir host` | loopback, no certificate, seconds |
//! | a name pointing here | `choir host --domain <name>` | `https://<name>:8417` |
//! | a VPS and no name | `choir host --public` | `https://<ip>.sslip.io:8417` |
//!
//! # Why `choir host` and not `choir node host`
//!
//! `choir node …` is the family for a node that exists — serve it, stop
//! it, read its log. This is the command you run when there is no node,
//! which is the same reason `choir init` is not `choir node init`. It
//! sits beside `init` in "getting started", and `init` is what it calls.
//!
//! # Invariant 9 is the whole difficulty
//!
//! The daemon refuses a non-loopback bind without TLS. That is not a
//! setting; it is the privacy rule written as code. Which means mode 2
//! and mode 3 are not "the same thing with a different bind address" —
//! they are a certificate first and a node second, and this module's
//! real job is to make the certificate step legible rather than to hide
//! it. It never runs `sudo` on your behalf. It prints the one line and
//! stops.
//!
//! # Examples
//!
//! ```
//! use choir_cli::host::Exposure;
//!
//! // A magic-DNS name so a box with no domain can still be issued a
//! // certificate with zero DNS work. Dashes, not dots: one label under
//! // the registrable domain, which is the form the Public Suffix List
//! // entry covers without ambiguity.
//! assert_eq!(
//!     Exposure::Public("203.0.113.7".into()).name().as_deref(),
//!     Some("203-0-113-7.sslip.io")
//! );
//! assert_eq!(Exposure::Local.name(), None);
//! ```

use std::path::PathBuf;

/// The magic-DNS provider used when there is no domain.
///
/// `sslip.io` rather than `nip.io`: since 2025 both are operated by the
/// same maintainer and share one Let's Encrypt rate-limit pool, so there
/// is no availability to be gained by preferring one, and `sslip.io` is
/// the one whose own documentation demonstrates the issuance path. That
/// shared pool is also why `--public-name` exists — the limit has been
/// exhausted before, and a rate-limited third party must never be the
/// only way to stand a node up.
pub const MAGIC_DNS: &str = "sslip.io";

/// How this node will be reached.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Exposure {
    /// Loopback. No certificate, because none is needed and none is
    /// possible: nobody outside can reach it, which is the point.
    Local,
    /// A name the user already owns, pointing at this box.
    Domain(String),
    /// A public IPv4 address with no name, wearing a magic-DNS one.
    Public(String),
    /// A name the user named outright, for when the magic-DNS provider
    /// is rate-limited or unwanted.
    Named(String),
}

impl Exposure {
    /// The name a certificate would be issued for, if any.
    #[must_use]
    pub fn name(&self) -> Option<String> {
        match self {
            Exposure::Local => None,
            Exposure::Domain(name) | Exposure::Named(name) => Some(name.clone()),
            // Dashes rather than dots. Both forms resolve, and the
            // dashed one is a single label under the registrable domain
            // — the shape the Public Suffix List entry covers cleanly,
            // and the shape that cannot be mistaken for a subdomain
            // delegation by anything in the path.
            Exposure::Public(ip) => Some(format!("{}.{MAGIC_DNS}", ip.replace('.', "-"))),
        }
    }

    /// The URL people outside will use.
    #[must_use]
    pub fn url(&self, port: u16) -> String {
        match self.name() {
            Some(name) => format!("https://{name}:{port}"),
            None => format!("http://127.0.0.1:{port}"),
        }
    }

    /// Whether a certificate has to exist before the node may bind.
    #[must_use]
    pub fn needs_certificate(&self) -> bool {
        self.name().is_some()
    }
}

/// What `choir host` was asked to do.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Options {
    /// Loopback, a domain, or a magic-DNS name.
    pub exposure: Exposure,
    /// The port to serve on.
    pub port: u16,
    /// The state directory, `~/.choir` unless given.
    pub state: PathBuf,
    /// A first repository to create, if one was named.
    pub repo: Option<String>,
    /// A first person to invite, if one was named.
    pub invite: Option<String>,
    /// Do not stop to ask about linger; warn and carry on.
    pub yes: bool,
    /// Stop after the certificate would have been requested, having
    /// requested nothing. `certbot --dry-run` underneath.
    pub dry_run: bool,
    /// Do not hand the node to a service manager; become it.
    ///
    /// For a container, where the runtime is the supervisor and PID 1
    /// should be the daemon. Everything up to and including the address
    /// still happens; then this `exec`s `choir node serve`, so the
    /// process the runtime watches is `choir-node` itself.
    pub foreground: bool,
    /// Daemon flags, passed through after `--`.
    pub extra: Vec<String>,
}

/// Parses `choir host`'s arguments.
///
/// # Errors
///
/// Returns a usage sentence naming the flag that was wrong. Combinations
/// that contradict each other — two exposures, a public address on a
/// machine that has none — are refused here rather than half-applied,
/// because the first thing this command does is write files.
pub fn parse(rest: &[&str], default_state: PathBuf) -> Result<Options, String> {
    let (mine, extra) = match rest.iter().position(|a| *a == "--") {
        Some(at) => (&rest[..at], &rest[at + 1..]),
        None => (rest, &rest[rest.len()..]),
    };
    let mut domain: Option<String> = None;
    let mut named: Option<String> = None;
    let mut ip: Option<String> = None;
    let mut public = false;
    let mut port: Option<u16> = None;
    let mut state: Option<PathBuf> = None;
    let mut repo: Option<String> = None;
    let mut invite: Option<String> = None;
    let mut yes = false;
    let mut dry_run = false;
    let mut foreground = false;

    let mut i = 0;
    while i < mine.len() {
        let name = mine[i];
        let value = || -> Result<String, String> {
            mine.get(i + 1)
                .map(|v| (*v).to_string())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match name {
            "--public" => {
                public = true;
                i += 1;
                continue;
            }
            "--yes" => {
                yes = true;
                i += 1;
                continue;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
                continue;
            }
            "--foreground" => {
                foreground = true;
                i += 1;
                continue;
            }
            "--domain" => domain = Some(value()?),
            "--public-name" => named = Some(value()?),
            "--ip" => ip = Some(value()?),
            "--repo" => repo = Some(value()?),
            "--invite" => invite = Some(value()?),
            "--state" => state = Some(PathBuf::from(value()?)),
            "--port" => {
                let raw = value()?;
                port = Some(
                    raw.parse()
                        .map_err(|_| format!("--port needs a port number, not {raw:?}"))?,
                );
            }
            other => {
                return Err(format!(
                    "unknown option {other:?}\n\n  \
                     daemon flags go after `--`: choir host -- {other} ..."
                ));
            }
        }
        i += 2;
    }

    let chosen = [domain.is_some(), named.is_some(), public || ip.is_some()]
        .iter()
        .filter(|c| **c)
        .count();
    if chosen > 1 {
        return Err(
            "--domain, --public and --public-name are three answers to one question:\n  \
             what name does a certificate go on. Give one."
                .to_string(),
        );
    }

    let exposure = if let Some(name) = domain {
        Exposure::Domain(name)
    } else if let Some(name) = named {
        Exposure::Named(name)
    } else if public || ip.is_some() {
        let given = ip.is_some();
        let address = match ip {
            Some(given) => given,
            None => detect_address()?,
        };
        if !is_public_v4(&address) {
            // Two different mistakes, and the same sentence for both
            // would be wrong for one of them. A detected private address
            // means this box is behind NAT and genuinely does not know
            // the address the world uses; a *given* one means the
            // address handed over is not routable, and repeating the
            // NAT explanation would send the reader looking for a
            // problem they have already been told the answer to.
            return Err(match given {
                true => format!(
                    "{address} is not routable on the public internet, so no certificate \
                     can be issued for it.\n\n  \
                     private, loopback, link-local, carrier-grade NAT (100.64/10, which is \
                     also\n  every Tailscale address) and the documentation ranges are all \
                     refused."
                ),
                false => format!(
                    "{address} is this box's own address and it is not a public one, so no \
                     certificate\n  can be issued for it.\n\n  \
                     behind NAT, the address the world uses is not one this box can see. \
                     Find it:\n\n    \
                     curl -fsS https://api.ipify.org\n\n  \
                     then: choir host --public --ip <that address>"
                ),
            });
        }
        Exposure::Public(address)
    } else {
        Exposure::Local
    };

    // `--foreground` becomes the daemon, so there is no "afterwards" in
    // this process for either of these to happen in. Refused rather than
    // silently dropped: a flag that is accepted and does nothing is
    // worse than one that is not accepted.
    if foreground && (repo.is_some() || invite.is_some()) {
        return Err(
            "--foreground execs the daemon, so nothing runs after it — --repo and\n  \
             --invite would never happen. Run them against the node once it is up:\n\n    \
             choir repo create <api> <owner/name.git>\n    \
             choir invite <api> <name> <owner/name.git>"
                .to_string(),
        );
    }
    if invite.is_some() && repo.is_none() {
        return Err(
            "--invite needs --repo: an invite carries a grant, and a grant names a\n  \
             repository. Without one the link would let somebody in to nothing.\n\n    \
             choir host --repo me/thing.git --invite <their name>"
                .to_string(),
        );
    }
    if dry_run && !exposure.needs_certificate() {
        return Err(
            "--dry-run is about the certificate, and a local node has none.\n  \
             Use it with --domain or --public."
                .to_string(),
        );
    }

    Ok(Options {
        exposure,
        port: port.unwrap_or(8417),
        state: absolute(state.unwrap_or(default_state)),
        repo,
        invite,
        yes,
        dry_run,
        foreground,
        extra: extra.iter().map(|a| (*a).to_string()).collect(),
    })
}

/// Resolves a state directory against the working directory.
///
/// A relative `--state` is a perfectly reasonable thing to type and a
/// broken thing to record: the supervision file this ends up in is read
/// by launchd or systemd, neither of which is standing where you were.
/// The node then starts against a root that does not exist, the unit
/// restarts it every two seconds, and the only sign is an empty log at a
/// path that is itself relative.
///
/// `std::path::absolute` rather than `canonicalize`: the directory does
/// not exist yet on a first run, and `canonicalize` fails on a path that
/// is not there.
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// This machine's own IPv4 address on the route to the outside.
///
/// A connectionless UDP socket, not `ifconfig` parsing and not a call to
/// somebody's what-is-my-ip service. `connect` on a UDP socket sends
/// nothing; it only makes the kernel pick the source address it *would*
/// use, which is exactly the question. The address it names is
/// `192.0.2.1` — TEST-NET-1, reserved for documentation and routed
/// nowhere — so even a stray packet would reach no one.
///
/// Behind NAT this answers with the private address, which is the honest
/// answer: this box genuinely does not know its public one, and guessing
/// would produce a certificate request for a name that resolves
/// somewhere else.
///
/// # Errors
///
/// Returns a description when no route to the outside can be found.
pub fn detect_address() -> Result<String, String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| format!("could not open a socket to ask which address this box uses: {e}"))?;
    socket
        .connect("192.0.2.1:80")
        .map_err(|e| format!("no route to the outside from this box: {e}"))?;
    let local = socket
        .local_addr()
        .map_err(|e| format!("could not read this box's own address: {e}"))?;
    Ok(local.ip().to_string())
}

/// Whether an address is one the public internet can route to.
///
/// Only IPv4: `sslip.io` encodes v6 too, but a v6-only box that reaches
/// this path deserves to be told so rather than handed a name built by a
/// rule this has not been exercised against.
#[must_use]
pub fn is_public_v4(address: &str) -> bool {
    let Ok(std::net::IpAddr::V4(ip)) = address.parse::<std::net::IpAddr>() else {
        return false;
    };
    let [a, b, ..] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        // Carrier-grade NAT, 100.64.0.0/10. Common on cheap VPS and on
        // every Tailscale interface, and a certificate for it would be
        // issued for a name the world resolves to somebody else's box.
        || (a == 100 && (64..128).contains(&b))
        || a >= 240)
}

/// The firewall this machine runs, and the one line that opens a port.
///
/// Detected and printed, never run. Opening a port is a change to how
/// much of this machine the internet can reach, and that decision is not
/// one a convenience command gets to make silently — even when it is
/// obviously the right one, which it usually is.
#[must_use]
pub fn firewall_hint(port: u16, needs_http01: bool) -> Option<String> {
    if crate::doctor::on_path("ufw").is_some() {
        let mut line = format!("sudo ufw allow {port}/tcp");
        if needs_http01 {
            line.push_str("  &&  sudo ufw allow 80/tcp");
        }
        return Some(line);
    }
    if crate::doctor::on_path("firewall-cmd").is_some() {
        let mut line = format!("sudo firewall-cmd --permanent --add-port={port}/tcp");
        if needs_http01 {
            line.push_str("  &&  sudo firewall-cmd --permanent --add-port=80/tcp");
        }
        line.push_str("  &&  sudo firewall-cmd --reload");
        return Some(line);
    }
    None
}

/// Whether this user's services survive a logout.
///
/// `None` on anything that is not `systemd --user` — a launchd agent has
/// no equivalent question, and answering one that was not asked is how a
/// report gets ignored.
#[must_use]
pub fn linger(user: &str) -> Option<bool> {
    if std::env::consts::OS != "linux" {
        return None;
    }
    let out = std::process::Command::new("loginctl")
        .args(["show-user", user])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line.trim() == "Linger=yes"),
    )
}

/// This process's login name.
#[must_use]
pub fn username() -> String {
    std::process::Command::new("id")
        .arg("-un")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// How to let somebody else reach a loopback node.
///
/// A loopback node is not reachable from another machine, and that is
/// invariant 9 doing its job rather than a limitation to route around.
/// Both answers here keep it that way: an ssh tunnel and `tailscale
/// serve` each terminate somewhere else and forward to `127.0.0.1`, so
/// the node never binds an address the internet can see.
#[must_use]
pub fn share_hint(port: u16) -> String {
    if crate::doctor::on_path("tailscale").is_some() {
        return format!(
            "tailscale serve --bg http://localhost:{port}\n      \
             (HTTPS inside your tailnet; the node itself stays on loopback)"
        );
    }
    format!(
        "ssh -N -L {port}:127.0.0.1:{port} <you>@<this-machine>\n      \
         then the node is at http://127.0.0.1:{port} on the other end"
    )
}

/// Waits for the node to answer `/healthz`.
///
/// Polled rather than assumed, because everything after this point —
/// creating a repository, minting an invite — is a request to a daemon a
/// service manager has only just been asked to start, and the failure
/// when it is not up yet is a connection refused that reads like a
/// broken install.
///
/// # Errors
///
/// Returns what the last attempt saw, after `seconds`.
pub fn wait_healthy(client: &crate::mcp::HttpClient, seconds: u64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        let last = match client.get("/healthz") {
            // 401 counts: something is serving and it is asking who we
            // are, which answers the only question being asked here.
            Ok((200 | 401, _)) => return Ok(()),
            Ok((503, body)) => format!("503, its own durability check failed: {body}"),
            Ok((code, _)) => format!("answered {code}"),
            Err(error) => error,
        };
        if std::time::Instant::now() >= deadline {
            return Err(last);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}
