//! `choir node tls` — the one step that needs root, and nothing else does.
//!
//! Obtaining a certificate is privileged: `certbot` writes
//! `/etc/letsencrypt`, and the renewal hook that keeps it working lives
//! under the same tree. Running a node is not privileged, and must not
//! become so. `scripts/flip/setup_tls.sh` states that split for the
//! operator's own dogfood host; this is the same split for somebody who
//! installed a binary and has no checkout to run a script out of.
//!
//! # Why this is a separate command rather than something `host` sudoes
//!
//! `choir host --domain <name>` prints one `sudo` line and stops. It
//! does not shell out to `sudo` itself, for two reasons that are the
//! same reason twice: a tool that escalates on your behalf has to be
//! trusted about *what* it escalated, and the only honest way to show
//! that is to make the privileged thing a command with a name, so it
//! appears in `--help`, in the shell history, and in `sudo`'s log as
//! itself rather than as an argument to something else.
//!
//! # What renewal does
//!
//! The daemon reads its certificate once, when it binds. There is no
//! reload; a rotated pair reaches the running node only through a
//! restart. So the deploy hook `certbot` runs after every successful
//! renewal is what closes the loop: it re-projects the pair into the
//! node user's state directory and restarts the user unit. Nothing here
//! runs on a timer of ours — `certbot`'s own timer is the schedule.
//!
//! # Examples
//!
//! ```
//! use choir_cli::tls::Plan;
//!
//! let plan = Plan::new("node.example", 8417, "choir", std::path::Path::new("/home/choir"));
//! // The marker the node reads is two lines under the node's own state
//! // directory, never a path into /etc/letsencrypt: the live directory
//! // is root-owned by design, and an unprivileged node that reads it
//! // works exactly until the first renewal rotates the files.
//! assert!(plan.marker.ends_with("tls.enabled"));
//! assert!(plan.cert.starts_with("/home/choir/.choir/tls"));
//! ```

use std::path::{Path, PathBuf};

/// Where `certbot`'s deploy hooks live. Fixed by certbot, not by us.
pub const HOOK_DIR: &str = "/etc/letsencrypt/renewal-hooks/deploy";

/// The name of the hook this installs, and the one `node uninstall`
/// names when it says what it left behind.
pub const HOOK_NAME: &str = "choir-tls";

/// Everything the certificate step will touch, built before anything is
/// done so a refusal can name all of it.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Plan {
    /// The name the certificate is for.
    pub domain: String,
    /// The port the node serves on. Part of the public URL, not of the
    /// certificate.
    pub port: u16,
    /// The unprivileged account the node runs as.
    pub user: String,
    /// That account's `~/.choir`.
    pub state: PathBuf,
    /// Where the readable copy of the pair is projected.
    pub tls_dir: PathBuf,
    /// The projected certificate.
    pub cert: PathBuf,
    /// The projected private key.
    pub key: PathBuf,
    /// The two-line file `choir node serve` reads.
    pub marker: PathBuf,
    /// The one-line file holding the URL people outside use.
    pub public_url: PathBuf,
    /// The deploy hook certbot runs after every renewal.
    pub hook: PathBuf,
}

impl Plan {
    /// The layout for one domain under one node account's home.
    #[must_use]
    pub fn new(domain: &str, port: u16, user: &str, home: &Path) -> Plan {
        let state = home.join(".choir");
        let tls_dir = state.join("tls");
        Plan {
            domain: domain.to_string(),
            port,
            user: user.to_string(),
            cert: tls_dir.join("fullchain.pem"),
            key: tls_dir.join("privkey.pem"),
            marker: state.join("tls.enabled"),
            public_url: state.join("public-url"),
            state,
            tls_dir,
            hook: PathBuf::from(HOOK_DIR).join(HOOK_NAME),
        }
    }

    /// The URL this node will be reachable at once the pair is in place.
    #[must_use]
    pub fn url(&self) -> String {
        format!("https://{}:{}", self.domain, self.port)
    }

    /// The two lines the marker holds.
    #[must_use]
    pub fn marker_body(&self) -> String {
        format!("{}\n{}\n", self.cert.display(), self.key.display())
    }
}

/// How the ACME challenge is answered.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Challenge {
    /// `--standalone` on port 80. Needs the port reachable from the
    /// internet now *and* at every renewal, because renewals rebind it.
    Http01,
    /// `--dns-cloudflare`, selected by the presence of a credentials
    /// file rather than by a flag — the same file-as-marker discipline
    /// as everything else here. Needs no inbound port and never
    /// publishes the origin address, which is why it is preferred behind
    /// a proxying CDN.
    Dns01Cloudflare,
}

impl Challenge {
    /// Which one this state directory selects.
    #[must_use]
    pub fn for_state(state: &Path) -> Challenge {
        match state.join("cloudflare.ini").exists() {
            true => Challenge::Dns01Cloudflare,
            false => Challenge::Http01,
        }
    }
}

/// How far to go.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Issuance {
    /// Ask the real directory for a real certificate.
    Live,
    /// `--dry-run`: exercise the whole path against the staging
    /// directory and write nothing. The way to find out that port 80 is
    /// firewalled without spending a rate-limit slot on finding out.
    DryRun,
    /// `--test-cert`: a real file, from the staging directory, that no
    /// client will trust. For proving the projection and the restart.
    Staging,
}

/// The `certbot` invocation, built but not run.
///
/// Returned as data so a test can assert on the exact argv without
/// certbot, root, a domain, or a rate-limit slot. The argv *is* the
/// contract: every flag omitted here is a prompt certbot would have
/// asked a script that cannot answer.
#[must_use]
pub fn certbot_argv(plan: &Plan, challenge: Challenge, issuance: Issuance) -> Vec<String> {
    let mut argv: Vec<String> = vec!["certbot".into(), "certonly".into()];
    match challenge {
        Challenge::Http01 => argv.push("--standalone".into()),
        Challenge::Dns01Cloudflare => {
            argv.push("--dns-cloudflare".into());
            argv.push("--dns-cloudflare-credentials".into());
            argv.push(plan.state.join("cloudflare.ini").display().to_string());
        }
    }
    argv.push("-d".into());
    argv.push(plan.domain.clone());
    argv.push("--non-interactive".into());
    argv.push("--agree-tos".into());
    // No email, on purpose, and this is the standing privacy rule rather
    // than an oversight: no personal identifier goes into
    // infrastructure. The cost is no expiry-warning mail, and the deploy
    // hook's automation is what actually answers that; `certbot renew
    // --dry-run` is the manual check.
    argv.push("--register-unsafely-without-email".into());
    match issuance {
        Issuance::Live => {}
        Issuance::DryRun => argv.push("--dry-run".into()),
        Issuance::Staging => argv.push("--test-cert".into()),
    }
    argv
}

/// The deploy hook certbot runs after every successful renewal.
///
/// It reads the marker at *renewal* time rather than being written for
/// whichever boundary was live on the day it was installed. Moving
/// termination from the node to a proxy in front of it is an operator
/// decision that must not require remembering to rewrite a hook: a
/// renewal that does not reach the live listener is a certificate that
/// expires while every file on disk says it was renewed.
#[must_use]
pub fn hook_script(plan: &Plan, uid: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Installed by `choir node tls`. Runs after every successful\n\
         # certbot renewal. Refreshes whatever is terminating TLS for\n\
         # this node -- the node itself, or a proxy in front of it.\n\
         #\n\
         # The daemon reads its certificate once, at bind. There is no\n\
         # reload, so the restart below is not a nicety: without it the\n\
         # renewed pair sits on disk and the running process keeps\n\
         # presenting the expired one.\n\
         set -eu\n\
         if systemctl is-active --quiet nginx 2>/dev/null; then\n\
         \x20 systemctl reload nginx\n\
         fi\n\
         [ -f {marker} ] || exit 0\n\
         install -o {user} -g {user} -m 600 \\\n\
         \x20 /etc/letsencrypt/live/{domain}/fullchain.pem {cert}\n\
         install -o {user} -g {user} -m 600 \\\n\
         \x20 /etc/letsencrypt/live/{domain}/privkey.pem {key}\n\
         sudo -u {user} XDG_RUNTIME_DIR=/run/user/{uid} \\\n\
         \x20 systemctl --user restart choir-node.service\n",
        marker = plan.marker.display(),
        user = plan.user,
        domain = plan.domain,
        cert = plan.cert.display(),
        key = plan.key.display(),
    )
}

/// One step of the certificate run, for printing as it happens.
pub struct Step {
    /// What was attempted, in the imperative.
    pub what: String,
    /// How it came out.
    pub outcome: Result<String, String>,
}

/// Everything that must be true before the first privileged byte.
///
/// Checked in one pass and reported together: a tool that fails on the
/// first missing thing, is fixed, then fails on the second is a tool
/// that gets run four times, and each of those runs asked for root.
///
/// # Errors
///
/// Returns every unmet precondition, one per line.
pub fn preflight(plan: &Plan, challenge: Challenge, uid: &str) -> Result<(), String> {
    let mut problems: Vec<String> = Vec::new();
    if uid != "0" {
        problems.push(format!(
            "this needs root — certbot writes /etc/letsencrypt:\n      \
             sudo choir node tls {} --user {}",
            plan.domain, plan.user
        ));
    }
    if crate::doctor::on_path("certbot").is_none() {
        problems.push(
            "certbot is not installed:\n      \
             sudo apt-get install -y certbot   (Debian/Ubuntu)\n      \
             sudo dnf install -y certbot       (Fedora/RHEL)"
                .to_string(),
        );
    }
    if !plan.state.exists() {
        problems.push(format!(
            "{} does not exist, so there is no node here to enable TLS for:\n      \
             run `choir host` as {} first",
            plan.state.display(),
            plan.user
        ));
    }
    if challenge == Challenge::Dns01Cloudflare {
        let ini = plan.state.join("cloudflare.ini");
        if !mode_is_private(&ini) {
            problems.push(format!(
                "{} must be mode 0600; it holds an API token:\n      chmod 600 {}",
                ini.display(),
                ini.display()
            ));
        }
    }
    match problems.is_empty() {
        true => Ok(()),
        false => Err(problems.join("\n\n  ")),
    }
}

/// Whether a file exists and nothing outside its owner can read it.
#[cfg(unix)]
fn mode_is_private(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 == 0)
}

#[cfg(not(unix))]
fn mode_is_private(path: &Path) -> bool {
    path.exists()
}

/// Performs the privileged half, in the order that survives a failure at
/// any point.
///
/// Marker before the hook runs, hook installed before the first
/// projection: the hook reads the marker to decide whether the node
/// needs the pair at all, so running it first would skip the projection
/// and leave a marker naming two files that are not there. Running the
/// hook *is* the first projection — proving the renewal path today,
/// rather than at a renewal two months from now when nobody is watching.
///
/// # Errors
///
/// Returns the steps completed so far alongside the failure, so the
/// caller can print what was done as well as what stopped.
pub fn apply(
    plan: &Plan,
    challenge: Challenge,
    issuance: Issuance,
    uid: &str,
) -> Result<Vec<Step>, (Vec<Step>, String)> {
    let mut steps: Vec<Step> = Vec::new();
    let push = |steps: &mut Vec<Step>, what: &str, done: String, r: Result<(), String>| {
        let failed = r.as_ref().err().cloned();
        steps.push(Step {
            what: what.to_string(),
            outcome: match &failed {
                None => Ok(done),
                Some(error) => Err(error.clone()),
            },
        });
        failed
    };

    let argv = certbot_argv(plan, challenge, issuance);
    let issued = match issuance {
        Issuance::Live => "issued".to_string(),
        Issuance::DryRun => "dry run passed; nothing was written".to_string(),
        Issuance::Staging => "issued from staging; no client will trust it".to_string(),
    };
    if let Some(error) = push(
        &mut steps,
        &format!("certificate for {}", plan.domain),
        issued,
        run(&argv),
    ) {
        return Err((steps, error));
    }
    // A dry run proves reachability and stops. Writing a marker for a
    // certificate that was deliberately not issued would hand the node a
    // public bind and two paths that do not exist.
    if issuance == Issuance::DryRun {
        return Ok(steps);
    }

    let projected = plan.tls_dir.display().to_string();
    for (what, done, result) in [
        (
            "readable copy",
            projected.clone(),
            std::fs::create_dir_all(&plan.tls_dir)
                .map_err(|e| format!("create {}: {e}", plan.tls_dir.display())),
        ),
        (
            "renewal hook",
            plan.hook.display().to_string(),
            write_hook(plan, uid),
        ),
        (
            "marker",
            plan.marker.display().to_string(),
            write_as_node(plan, &plan.marker, &plan.marker_body()),
        ),
        (
            "public url",
            plan.url(),
            write_as_node(plan, &plan.public_url, &format!("{}\n", plan.url())),
        ),
        (
            "first projection",
            projected,
            run(&[plan.hook.display().to_string()]),
        ),
    ] {
        if let Some(error) = push(&mut steps, what, done, result) {
            return Err((steps, error));
        }
    }
    Ok(steps)
}

/// Writes the deploy hook, executable, owned by root by virtue of who is
/// running.
fn write_hook(plan: &Plan, uid: &str) -> Result<(), String> {
    let dir = Path::new(HOOK_DIR);
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    choir_fs::write_atomic(&plan.hook, hook_script(plan, uid))
        .map_err(|e| format!("write {}: {e}", plan.hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&plan.hook, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", plan.hook.display()))?;
    }
    Ok(())
}

/// Writes a file into the node's state directory and hands it to the
/// node's account.
///
/// This process is root, so a file it creates belongs to root — and the
/// node deliberately does not. A marker the node cannot read is a node
/// that quietly starts on loopback and never says why.
fn write_as_node(plan: &Plan, path: &Path, body: &str) -> Result<(), String> {
    choir_fs::write_atomic_private(path, body)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    run(&[
        "chown".to_string(),
        format!("{}:{}", plan.user, plan.user),
        path.display().to_string(),
    ])
}

/// Runs one command, returning its last line of stderr on failure.
///
/// The last line rather than all of it: `certbot` narrates, and the
/// sentence a reader needs — the challenge that failed, the port that
/// was not reachable — is the one it ends on.
fn run(argv: &[String]) -> Result<(), String> {
    let Some((program, args)) = argv.split_first() else {
        return Ok(());
    };
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("could not run `{program}`: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stderr);
    let detail = text.trim().lines().last().unwrap_or("failed").to_string();
    Err(format!("`{}` failed: {detail}", argv.join(" ")))
}

/// This process's user id, as a string, for the root check and the hook.
///
/// `id -u` rather than a libc call: the workspace adds dependencies
/// reluctantly and this is asked once per command, not per request.
#[must_use]
pub fn uid() -> String {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The expiry date `openssl` reads out of a certificate.
///
/// `openssl x509`, not a parser: the workspace already requires
/// `openssl` for everything key-shaped and adding an X.509 crate to read
/// one field would be a supply-chain edge bought for a date.
///
/// # Errors
///
/// Returns a description when the file cannot be read or is not a
/// certificate.
pub fn expiry(cert: &Path) -> Result<String, String> {
    let out = std::process::Command::new("openssl")
        .args(["x509", "-enddate", "-noout", "-in"])
        .arg(cert)
        .output()
        .map_err(|e| format!("could not run `openssl`: {e}"))?;
    if !out.status.success() {
        return Err(format!("{} is not a certificate", cert.display()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .strip_prefix("notAfter=")
        .map(str::to_string)
        .ok_or_else(|| format!("`openssl x509` said {:?}", text.trim()))
}
