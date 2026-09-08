//! `choir host` and `choir node tls`: the decisions, without a machine.
//!
//! Almost nothing here can be exercised end to end on a developer's
//! laptop — the certificate path needs root, a public address, port 80
//! and a Let's Encrypt rate-limit slot, and the linger path needs a
//! Linux that logs out. What *can* be pinned is every decision made
//! before any of that: which name a certificate goes on, the exact argv
//! `certbot` is handed, what the renewal hook does, and which flags the
//! marker files turn into. Those are the parts that fail silently.
//!
//! The argv assertions are the load-bearing ones. Every flag missing
//! from a `certbot certonly` line is a prompt certbot would ask a script
//! that cannot answer, and the failure is a command that hangs on a VPS
//! nobody is watching.

use choir_cli::host::{self, Exposure};
use choir_cli::serve::{plan, Layout};
use choir_cli::tls::{self, Challenge, Issuance, Plan};
use std::path::PathBuf;

fn state(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("choir-cli-host-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("state dir");
    std::fs::write(root.join("auth"), "choir:token\n").expect("auth");
    std::fs::write(root.join("keys"), "op ab\n").expect("keys");
    root
}

fn parse(args: &[&str]) -> Result<choir_cli::host::Options, String> {
    host::parse(args, PathBuf::from("/tmp/choir-host-default"))
}

// -- which name a certificate goes on ------------------------------------

#[test]
fn the_three_modes_produce_three_names() {
    assert_eq!(Exposure::Local.name(), None);
    assert_eq!(
        Exposure::Domain("node.example".into()).name().as_deref(),
        Some("node.example")
    );
    // Dashes, one label under the registrable domain.
    assert_eq!(
        Exposure::Public("203.0.113.7".into()).name().as_deref(),
        Some("203-0-113-7.sslip.io")
    );
}

#[test]
fn only_a_named_node_gets_an_https_url() {
    assert_eq!(Exposure::Local.url(8417), "http://127.0.0.1:8417");
    assert_eq!(
        Exposure::Domain("node.example".into()).url(9000),
        "https://node.example:9000"
    );
    assert!(!Exposure::Local.needs_certificate());
    assert!(Exposure::Public("203.0.113.7".into()).needs_certificate());
}

/// The addresses a certificate must never be requested for.
///
/// Every one of these resolves to somebody else's machine, or to no
/// machine at all. Carrier-grade NAT is the one that looks public and is
/// not, and it is also every Tailscale address — so a box on a tailnet
/// asking for `--public` would otherwise be handed a name for an address
/// it shares with a hundred thousand other hosts.
#[test]
fn private_and_shared_addresses_are_not_public() {
    for address in [
        "127.0.0.1",
        "10.0.0.4",
        "192.168.1.20",
        "172.16.9.9",
        "169.254.10.1",
        "100.101.102.103",
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "203.0.113.7", // TEST-NET-3, documentation
        "not-an-address",
        "2001:db8::1",
    ] {
        assert!(
            !host::is_public_v4(address),
            "{address} was treated as a routable public address"
        );
    }
    for address in ["8.8.8.8", "51.15.4.9", "185.199.108.153"] {
        assert!(host::is_public_v4(address), "{address} is public");
    }
}

// -- argument parsing ----------------------------------------------------

/// A relative `--state` is reasonable to type and broken to record: the
/// supervision file it lands in is read by launchd or systemd, neither
/// of which is standing where you were.
#[test]
fn a_relative_state_directory_is_made_absolute() {
    let options = parse(&["--state", "state"]).expect("parses");
    assert!(
        options.state.is_absolute(),
        "a unit naming {} starts against a root that does not exist",
        options.state.display()
    );
    assert!(options.state.ends_with("state"), "{:?}", options.state);
}

#[test]
fn bare_host_is_local() {
    let options = parse(&[]).expect("bare host parses");
    assert_eq!(options.exposure, Exposure::Local);
    assert_eq!(options.port, 8417);
    assert!(!options.yes);
}

#[test]
fn two_exposures_are_refused_rather_than_ranked() {
    let error = parse(&["--domain", "a.example", "--public"]).expect_err("refused");
    assert!(error.contains("one question"), "{error}");
}

/// An invite carries a grant, and a grant names a repository.
#[test]
fn an_invite_needs_something_to_invite_someone_to() {
    let error = parse(&["--invite", "Ada"]).expect_err("refused");
    assert!(error.contains("--repo"), "{error}");
    parse(&["--repo", "me/thing.git", "--invite", "Ada"]).expect("together they parse");
}

#[test]
fn a_dry_run_of_nothing_is_refused() {
    let error = parse(&["--dry-run"]).expect_err("refused");
    assert!(error.contains("certificate"), "{error}");
    parse(&["--domain", "a.example", "--dry-run"]).expect("with a name it parses");
}

/// `--foreground` execs the daemon, so nothing runs after it. A flag
/// that is accepted and then silently does nothing is worse than one
/// that is refused.
#[test]
fn foreground_refuses_the_steps_that_would_never_run() {
    let error = parse(&["--foreground", "--repo", "me/thing.git"]).expect_err("refused");
    assert!(error.contains("never happen"), "{error}");
    let options = parse(&["--foreground"]).expect("alone it parses");
    assert!(options.foreground);
}

#[test]
fn daemon_flags_pass_through_after_the_separator() {
    let options = parse(&["--port", "9001", "--", "--rate-limit-api", "60"]).expect("parses");
    assert_eq!(options.port, 9001);
    assert_eq!(options.extra, vec!["--rate-limit-api", "60"]);
}

/// Two different mistakes, and one sentence would be wrong for one of
/// them: a *detected* private address means this box is behind NAT and
/// cannot see the address the world uses; a *given* one means what was
/// handed over is not routable.
#[test]
fn a_private_address_cannot_wear_a_magic_dns_name() {
    let given = parse(&["--public", "--ip", "10.1.2.3"]).expect_err("refused");
    assert!(given.contains("not routable"), "{given}");
    assert!(
        !given.contains("api.ipify.org"),
        "an address the caller supplied does not send them looking for it again: {given}"
    );
}

// -- the certbot invocation ---------------------------------------------

fn tls_plan() -> Plan {
    Plan::new(
        "node.example",
        8417,
        "choir",
        std::path::Path::new("/home/choir"),
    )
}

#[test]
fn http01_certbot_argv_answers_every_prompt() {
    let argv = tls::certbot_argv(&tls_plan(), Challenge::Http01, Issuance::Live);
    assert_eq!(
        argv,
        vec![
            "certbot",
            "certonly",
            "--standalone",
            "-d",
            "node.example",
            "--non-interactive",
            "--agree-tos",
            "--register-unsafely-without-email",
        ]
    );
}

/// No email, ever, on any path. The standing privacy rule, asserted
/// rather than left to a comment: an `--email` flag added here would put
/// a personal identifier into infrastructure and nothing else would
/// notice.
#[test]
fn no_certbot_invocation_carries_an_address() {
    for challenge in [Challenge::Http01, Challenge::Dns01Cloudflare] {
        for issuance in [Issuance::Live, Issuance::DryRun, Issuance::Staging] {
            let argv = tls::certbot_argv(&tls_plan(), challenge, issuance);
            assert!(
                argv.iter()
                    .any(|a| a == "--register-unsafely-without-email"),
                "{argv:?}"
            );
            assert!(
                !argv.iter().any(|a| a == "--email" || a.contains('@')),
                "{argv:?}"
            );
        }
    }
}

#[test]
fn dns01_is_selected_by_a_file_and_carries_its_path() {
    let argv = tls::certbot_argv(&tls_plan(), Challenge::Dns01Cloudflare, Issuance::Live);
    assert!(argv.iter().any(|a| a == "--dns-cloudflare"), "{argv:?}");
    assert!(
        argv.iter().any(|a| a.ends_with("cloudflare.ini")),
        "the credentials file is named: {argv:?}"
    );
    assert!(!argv.iter().any(|a| a == "--standalone"), "{argv:?}");
}

#[test]
fn a_dry_run_and_the_staging_directory_are_different_flags() {
    let dry = tls::certbot_argv(&tls_plan(), Challenge::Http01, Issuance::DryRun);
    assert!(dry.iter().any(|a| a == "--dry-run"), "{dry:?}");
    let staging = tls::certbot_argv(&tls_plan(), Challenge::Http01, Issuance::Staging);
    assert!(staging.iter().any(|a| a == "--test-cert"), "{staging:?}");
}

#[test]
fn the_challenge_is_chosen_by_a_file_not_a_flag() {
    let root = state("challenge");
    assert_eq!(Challenge::for_state(&root), Challenge::Http01);
    std::fs::write(
        root.join("cloudflare.ini"),
        "dns_cloudflare_api_token = x\n",
    )
    .expect("ini");
    assert_eq!(Challenge::for_state(&root), Challenge::Dns01Cloudflare);
}

// -- the renewal hook ----------------------------------------------------

/// The daemon reads its certificate once, at bind. Without the restart
/// the renewed pair sits on disk while the running process keeps
/// presenting the expired one — and every file on disk says it renewed.
#[test]
fn the_deploy_hook_restarts_the_node() {
    let plan = tls_plan();
    let hook = tls::hook_script(&plan, "1000");
    assert!(hook.starts_with("#!/bin/sh\n"), "{hook}");
    assert!(
        hook.contains("systemctl --user restart choir-node.service"),
        "{hook}"
    );
    assert!(hook.contains("XDG_RUNTIME_DIR=/run/user/1000"), "{hook}");
    // Both boundaries, decided at renewal time rather than baked in on
    // the day it was installed.
    assert!(hook.contains("systemctl reload nginx"), "{hook}");
    // And it re-projects the pair into the node's own directory.
    assert!(
        hook.contains("/etc/letsencrypt/live/node.example/fullchain.pem"),
        "{hook}"
    );
    assert!(
        hook.contains("/home/choir/.choir/tls/fullchain.pem"),
        "{hook}"
    );
    assert!(hook.contains("-o choir -g choir -m 600"), "{hook}");
    // Reads the marker to decide whether the node needs the pair at all.
    assert!(hook.contains("/home/choir/.choir/tls.enabled"), "{hook}");
}

#[test]
fn the_marker_names_the_projected_pair_and_never_letsencrypt() {
    let plan = tls_plan();
    let body = plan.marker_body();
    assert_eq!(
        body,
        "/home/choir/.choir/tls/fullchain.pem\n/home/choir/.choir/tls/privkey.pem\n"
    );
    assert!(!body.contains("/etc/letsencrypt"), "{body}");
    assert_eq!(plan.url(), "https://node.example:8417");
}

// -- what the markers turn into ------------------------------------------

#[test]
fn no_marker_means_loopback_and_no_tls_flags() {
    let root = state("no-marker");
    let layout = Layout::new(&root, 8417);
    assert_eq!(layout.tls(), None);
    assert_eq!(layout.bind(), "127.0.0.1");
    let argv = plan(PathBuf::from("choir-node"), &layout, &[], &[]).expect("plans");
    assert!(
        !argv.args.iter().any(|a| a == "--bind" || a == "--tls-cert"),
        "{:?}",
        argv.args
    );
}

#[test]
fn a_marker_turns_into_a_public_bind_and_the_pair() {
    let root = state("marker");
    let cert = root.join("fullchain.pem");
    let key = root.join("privkey.pem");
    std::fs::write(&cert, "cert").expect("cert");
    std::fs::write(&key, "key").expect("key");
    std::fs::write(
        root.join("tls.enabled"),
        format!("{}\n{}\n", cert.display(), key.display()),
    )
    .expect("marker");

    let layout = Layout::new(&root, 8417);
    assert_eq!(layout.tls(), Some((cert.clone(), key.clone())));
    assert_eq!(layout.bind(), "0.0.0.0");
    let argv = plan(PathBuf::from("choir-node"), &layout, &[], &[]).expect("plans");
    let joined = argv.args.join(" ");
    assert!(joined.contains("--bind 0.0.0.0"), "{joined}");
    assert!(
        joined.contains(&format!("--tls-cert {}", cert.display())),
        "{joined}"
    );
    assert!(
        joined.contains(&format!("--tls-key {}", key.display())),
        "{joined}"
    );
}

/// Invariant 9 by another route: a marker missing its second line must
/// never become a public bind with no certificate.
#[test]
fn a_half_written_marker_is_no_marker() {
    let root = state("half-marker");
    std::fs::write(root.join("tls.enabled"), "/only/one/line.pem\n").expect("marker");
    let layout = Layout::new(&root, 8417);
    assert_eq!(layout.tls(), None);
    assert_eq!(layout.bind(), "127.0.0.1");
}

/// A marker naming files this user cannot read refuses to plan, rather
/// than installing a unit that crash-loops on startup.
#[test]
fn a_marker_naming_absent_files_refuses_to_plan() {
    let root = state("absent-pair");
    std::fs::write(
        root.join("tls.enabled"),
        "/no/such/cert.pem\n/no/such/key.pem\n",
    )
    .expect("marker");
    let layout = Layout::new(&root, 8417);
    let error = plan(PathBuf::from("choir-node"), &layout, &[], &[]).expect_err("refused");
    assert!(error.contains("cannot read"), "{error}");
    assert!(error.contains("choir node tls"), "names the fix: {error}");
}

/// The daemon exits at startup on `--accounts-file` without
/// `--acl-file`, which a supervisor turns into a restart loop. Caught
/// here instead, where it can be a sentence.
#[test]
fn an_accounts_file_without_an_acl_is_refused_before_the_daemon_sees_it() {
    let root = state("accounts-no-acl");
    std::fs::write(root.join("accounts.jsonl"), "").expect("accounts");
    let layout = Layout::new(&root, 8417);
    let error = plan(PathBuf::from("choir-node"), &layout, &[], &[]).expect_err("refused");
    assert!(error.contains("grant to every"), "{error}");

    std::fs::write(root.join("acl"), "choir * own\n").expect("acl");
    let argv = plan(PathBuf::from("choir-node"), &layout, &[], &[]).expect("with an acl it plans");
    let joined = argv.args.join(" ");
    assert!(joined.contains("--acl-file"), "{joined}");
    assert!(joined.contains("--accounts-file"), "{joined}");
}

#[test]
fn the_public_url_is_read_back_from_the_state_directory() {
    let root = state("public-url");
    let layout = Layout::new(&root, 8417);
    assert_eq!(layout.public(), None);
    std::fs::write(root.join("public-url"), "https://node.example:8417\n").expect("url");
    assert_eq!(
        Layout::new(&root, 8417).public().as_deref(),
        Some("https://node.example:8417")
    );
}

// -- the lines a person is asked to paste --------------------------------

/// Detected and printed, never run: opening a port changes how much of
/// this machine the internet reaches, and that is not a decision a
/// convenience command makes silently.
#[test]
fn a_firewall_hint_names_both_ports_or_is_absent() {
    match host::firewall_hint(8417, true) {
        None => {} // no ufw and no firewalld on this machine
        Some(line) => {
            assert!(line.starts_with("sudo "), "{line}");
            assert!(line.contains("8417"), "{line}");
            assert!(line.contains("80"), "HTTP-01 renewals need it too: {line}");
        }
    }
}

/// Both share hints keep the node on loopback. Neither is a way around
/// invariant 9; they terminate elsewhere and forward to `127.0.0.1`.
#[test]
fn sharing_a_local_node_never_binds_it_wider() {
    let hint = host::share_hint(8417);
    assert!(hint.contains("8417"), "{hint}");
    assert!(
        hint.contains("127.0.0.1") || hint.contains("localhost"),
        "the far end still reaches loopback: {hint}"
    );
}

#[test]
fn linger_is_only_a_question_on_linux() {
    let answer = host::linger(&host::username());
    if std::env::consts::OS != "linux" {
        assert_eq!(answer, None, "a launchd machine has no linger to report");
    }
}
