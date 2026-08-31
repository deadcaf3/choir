//! The dogfood installer must not silently drop an explicitly enabled
//! review gate. Test the pure plist renderer rather than touching launchd.

pub(crate) fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn public_url_configuration_is_private_validated_and_wired_into_tls_setup() {
    use std::os::unix::fs::PermissionsExt;

    let home = std::env::temp_dir().join(format!("choir-public-url-{}", std::process::id()));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).expect("scratch home");
    let script = repo_root().join("scripts/flip/configure_public_url.sh");

    let configured = std::process::Command::new("sh")
        .arg(&script)
        .args(["node.example.test", "9443"])
        .env("HOME", &home)
        .output()
        .expect("configuration helper runs");
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    let marker = home.join(".choir-public-url");
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker readable"),
        "https://node.example.test:9443\n"
    );
    assert_eq!(
        std::fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
        0o600,
        "the route sits beside credential-bearing operator state"
    );

    for (domain, port) in [
        ("not-a-public-name", "8417"),
        ("bad/name.example", "8417"),
        ("node.example.test", "0"),
        ("node.example.test", "65536"),
    ] {
        let refused = std::process::Command::new("sh")
            .arg(&script)
            .args([domain, port])
            .env("HOME", &home)
            .output()
            .expect("configuration helper runs");
        assert!(!refused.status.success(), "accepted {domain}:{port}");
    }
    assert_eq!(
        std::fs::read_to_string(&marker).expect("valid marker survives refused input"),
        "https://node.example.test:9443\n"
    );

    let setup = std::fs::read_to_string(repo_root().join("scripts/flip/setup_tls.sh"))
        .expect("TLS setup source");
    let here = setup.find("HERE=").expect("TLS setup locates its helpers");
    let configure = setup
        .find("configure_public_url.sh")
        .expect("TLS setup configures the verified route");
    assert!(
        here < configure,
        "helper directory must be known before use"
    );

    for name in [
        "scripts/flip/install_node.sh",
        "scripts/flip/install_node_linux.sh",
    ] {
        let installer = std::fs::read_to_string(repo_root().join(name)).expect(name);
        assert!(
            installer.contains(".choir-public-url"),
            "{name} does not require the certificate-valid route when TLS is active"
        );
        assert!(
            installer.contains("--auth-file") && installer.contains("\\$(cat ~/.choir-public-url)"),
            "{name} does not recommend the CLI over verified HTTPS"
        );
        assert!(
            !installer.contains("view $PUBLIC_URL"),
            "{name} exposes the private route in installer output"
        );
        assert!(
            !installer.contains("curl -s -u choir:"),
            "{name} still prints a secret-bearing loopback curl command"
        );
    }

    std::fs::remove_dir_all(home).ok();
}

/// Writes the repos file both renderers read in place of the old single
/// positional repo. Unique per call: these tests run on parallel threads
/// inside one process, so a pid-keyed name would collide.
fn repos_file(entries: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("choir-repos-list-{}-{n}", std::process::id()));
    std::fs::write(&path, entries).unwrap();
    path
}

fn render_output(
    repos: &str,
    protected: Option<&str>,
    scope: bool,
    tls: Option<(&str, &str)>,
    acl: Option<&str>,
    accounts: Option<&str>,
    proxy: bool,
) -> std::process::Output {
    let script = repo_root().join("scripts/flip/render_node_plist.sh");
    let repos_path = repos_file(repos);
    let mut command = std::process::Command::new("sh");
    command.arg(script).args([
        "com.example.node",
        "/opt/choir-node",
        "/srv/repos",
        "8417",
        "/state/auth",
        "/state/keys",
        "/state/reviewers",
        "/state/node.log",
    ]);
    command.arg(&repos_path).args([
        "/state/newcomer-audit.jsonl",
        "/state/newcomer-adjudications.jsonl",
    ]);
    // Positional like the installers pass them: [protected-refs] then
    // [require-scope] then [tls-cert] [tls-key], an empty slot standing
    // in for an absent policy.
    if let Some(path) = protected {
        command.arg(path);
    } else if scope || tls.is_some() || acl.is_some() || accounts.is_some() || proxy {
        command.arg("");
    }
    if scope {
        command.arg("require-scope");
    } else if tls.is_some() || acl.is_some() || accounts.is_some() || proxy {
        command.arg("");
    }
    if let Some((cert, key)) = tls {
        command.args([cert, key]);
    } else if acl.is_some() || accounts.is_some() || proxy {
        command.args(["", ""]);
    }
    if let Some(path) = acl {
        command.arg(path);
    } else if accounts.is_some() || proxy {
        command.arg("");
    }
    if let Some(path) = accounts {
        command.arg(path);
    } else if proxy {
        command.arg("");
    }
    if proxy {
        command.arg("behind-tls-proxy");
    }
    let output = command.output().expect("render plist");
    std::fs::remove_file(repos_path).ok();
    output
}

fn render(protected: Option<&str>, scope: bool) -> String {
    render_tls(protected, scope, None, None, None, false)
}

fn render_tls(
    protected: Option<&str>,
    scope: bool,
    tls: Option<(&str, &str)>,
    acl: Option<&str>,
    accounts: Option<&str>,
    proxy: bool,
) -> String {
    let output = render_output(
        "owner/repo.git\n",
        protected,
        scope,
        tls,
        acl,
        accounts,
        proxy,
    );
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("UTF-8 plist")
}

#[test]
fn explicit_policy_renders_all_three_review_gates_or_none() {
    let open = render(None, false);
    for required in [
        "--newcomer-audit",
        "/state/newcomer-audit.jsonl",
        "--newcomer-adjudications",
        "/state/newcomer-adjudications.jsonl",
    ] {
        assert!(open.contains(required), "install omits {required}");
    }
    for flag in [
        "--require-assignment",
        "--protected-refs",
        "--require-review",
        "--require-scope",
    ] {
        assert!(!open.contains(flag), "ungated install contains {flag}");
    }

    let protected = render(Some("/state/protected-refs"), false);
    let positions: Vec<usize> = [
        "--require-assignment",
        "--protected-refs",
        "/state/protected-refs",
        "--require-review",
    ]
    .iter()
    .map(|needle| protected.find(needle).expect("policy argument"))
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "review policy arguments must stay complete and ordered"
    );

    let installer = std::fs::read_to_string(repo_root().join("scripts/flip/install_node.sh"))
        .expect("installer source");
    assert!(installer.contains("review-gates.enabled"));
    assert!(installer.contains("scope-required.enabled"));
    assert!(installer.contains("render_node_plist.sh"));
    assert!(installer.contains("validate_review_policy.sh"));
    let here = installer
        .find("HERE=")
        .expect("installer defines helper directory");
    for helper in ["validate_review_policy.sh", "render_node_plist.sh"] {
        assert!(
            here < installer.find(helper).expect("installer invokes helper"),
            "installer must define HERE before invoking {helper}"
        );
    }
}

/// The Linux sibling of [`render_output`], fed byte-identical arguments.
fn render_unit_output(
    repos: &str,
    protected: Option<&str>,
    scope: bool,
    tls: Option<(&str, &str)>,
    acl: Option<&str>,
    accounts: Option<&str>,
    proxy: bool,
) -> std::process::Output {
    let script = repo_root().join("scripts/flip/render_node_service.sh");
    let repos_path = repos_file(repos);
    let mut command = std::process::Command::new("sh");
    command.arg(script).args([
        "com.example.node",
        "/opt/choir-node",
        "/srv/repos",
        "8417",
        "/state/auth",
        "/state/keys",
        "/state/reviewers",
        "/state/node.log",
    ]);
    command.arg(&repos_path).args([
        "/state/newcomer-audit.jsonl",
        "/state/newcomer-adjudications.jsonl",
    ]);
    if let Some(path) = protected {
        command.arg(path);
    } else if scope || tls.is_some() || acl.is_some() || accounts.is_some() || proxy {
        command.arg("");
    }
    if scope {
        command.arg("require-scope");
    } else if tls.is_some() || acl.is_some() || accounts.is_some() || proxy {
        command.arg("");
    }
    if let Some((cert, key)) = tls {
        command.args([cert, key]);
    } else if acl.is_some() || accounts.is_some() || proxy {
        command.args(["", ""]);
    }
    if let Some(path) = acl {
        command.arg(path);
    } else if accounts.is_some() || proxy {
        command.arg("");
    }
    if let Some(path) = accounts {
        command.arg(path);
    } else if proxy {
        command.arg("");
    }
    if proxy {
        command.arg("behind-tls-proxy");
    }
    let output = command.output().expect("render unit");
    std::fs::remove_file(repos_path).ok();
    output
}

fn render_unit_tls(
    protected: Option<&str>,
    scope: bool,
    tls: Option<(&str, &str)>,
    acl: Option<&str>,
    accounts: Option<&str>,
    proxy: bool,
) -> String {
    let output = render_unit_output(
        "owner/repo.git\n",
        protected,
        scope,
        tls,
        acl,
        accounts,
        proxy,
    );
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("UTF-8 unit")
}

fn render_private_beta(protected: &str, scope: &str, acl: &str) -> std::process::Output {
    render_private_beta_full(protected, scope, acl, "/srv/choir/accounts.jsonl", "on")
}

fn render_private_beta_full(
    protected: &str,
    scope: &str,
    acl: &str,
    accounts: &str,
    webauthn: &str,
) -> std::process::Output {
    let script = repo_root().join("scripts/flip/render_node_service.sh");
    let repos_path = repos_file("owner/repo.git\n");
    let output = std::process::Command::new("sh")
        .arg(script)
        .args([
            "choir-node",
            "/opt/choir-node",
            "/srv/choir/repos",
            "8417",
            "/srv/choir/auth",
            "/srv/choir/keys",
            "/srv/choir/reviewers",
            "/var/log/choir/node.log",
        ])
        .arg(&repos_path)
        .args([
            "/srv/choir/newcomer-audit.jsonl",
            "/srv/choir/newcomer-adjudications.jsonl",
            protected,
            scope,
            "",
            "",
            acl,
            accounts,
            "behind-tls-proxy",
            webauthn,
            "choir",
        ])
        .output()
        .expect("render private beta unit");
    std::fs::remove_file(repos_path).ok();
    output
}

#[test]
fn private_beta_service_is_loopback_only_fail_closed_and_hardened() {
    for (protected, scope, acl) in [
        ("", "require-scope", "/srv/choir/acl"),
        ("/srv/choir/protected-refs", "", "/srv/choir/acl"),
        ("/srv/choir/protected-refs", "require-scope", ""),
    ] {
        assert!(
            !render_private_beta(protected, scope, acl).status.success(),
            "private beta rendered without every fail-closed policy input"
        );
    }

    let output = render_private_beta(
        "/srv/choir/protected-refs",
        "require-scope",
        "/srv/choir/acl",
    );
    assert!(output.status.success());
    let unit = String::from_utf8(output.stdout).expect("UTF-8 unit");
    for required in [
        "--bind 127.0.0.1",
        "--acl-file /srv/choir/acl",
        "--require-assignment",
        "--require-review",
        "--require-scope",
        "--read-only-browser",
        "--request-log",
        "--journal",
        "--rate-limit-api 120",
        "--rate-limit-git 60",
        "--quota-push-bytes 536870912",
        "--quota-workspaces 8",
        "--api-body-limit 1048576",
        "--batch-limit 256",
        "--ready-min-free-bytes 1073741824",
        "User=choir",
        "Group=choir",
        "NoNewPrivileges=true",
        "PrivateTmp=true",
        "ProtectSystem=strict",
        "TasksMax=128",
        "MemoryMax=1G",
        "WantedBy=multi-user.target",
    ] {
        assert!(
            unit.contains(required),
            "private beta unit dropped {required}:\n{unit}"
        );
    }
    assert!(
        !unit.contains("--tls-cert"),
        "TLS belongs at the reverse proxy"
    );
    // Invites are how a beta user is given a credential, which the runbook
    // documents and the landing page advertises; the manifest carries
    // `accounts=enabled` and render_private_beta_service.sh reads it from
    // there rather than deciding for itself.
    assert!(
        unit.contains("--accounts-file"),
        "invite-only self-service is how a beta user gets a credential"
    );
    // Passkeys are the separate switch, and stay off for this beta. Without
    // its own flag this assertion could not be written: enrolment and the
    // browser write path both sit behind the accounts store, so the line
    // above would have turned them on too.
    // The manifest now asks for WebAuthn, and the renderer reads it from
    // there. Still its own slot and its own flag: a beta that turns it
    // off again changes one line in the manifest, not the meaning of the
    // accounts file.
    assert!(
        unit.contains("--passkeys"),
        "the manifest asks for WebAuthn and the unit must carry it"
    );
    // The other half of "TLS belongs at the reverse proxy": a node that
    // refuses its own TLS because something in front terminates it must
    // write its absolute URLs as the scheme that thing speaks. An invite
    // is a bearer credential carried in a URL, so getting this wrong puts
    // the credential on the wire in cleartext for one request.
    assert!(
        unit.contains("--behind-tls-proxy"),
        "a beta node behind the proxy must know it is behind one"
    );
    assert!(!unit.contains("--hooks-file"), "webhooks stay disabled");
    assert!(!unit.contains("--ssh-handoff"), "SSH stays disabled");
}

/// The rendered private-beta proxy configuration.
fn beta_proxy_config() -> String {
    let output = std::process::Command::new("sh")
        .arg(repo_root().join("scripts/flip/render_beta_nginx.sh"))
        .args([
            "beta.example.invalid",
            "8417",
            "/etc/ssl/choir/fullchain.pem",
            "/etc/ssl/choir/privkey.pem",
        ])
        .output()
        .expect("render nginx config");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("UTF-8 nginx config")
}

/// The proxy is the only part of the deployment that can see a client
/// address, so it is the only place this can be checked (D59). Written
/// as a forbidden list rather than a shape assertion: the failure mode
/// is somebody restoring one directive, and a list names each one.
#[test]
fn private_beta_proxy_handles_no_client_address() {
    let config = beta_proxy_config();
    for forbidden in [
        "$remote_addr",
        "$binary_remote_addr",
        "$proxy_add_x_forwarded_for",
        "limit_req_zone",
        "limit_conn_zone",
        "log_format",
    ] {
        assert!(
            !config.contains(forbidden),
            "proxy config reintroduced {forbidden}:\n{config}"
        );
    }
    for required in [
        "access_log off;",
        "error_log /dev/null crit;",
        // Set empty, not omitted: an omitted directive forwards
        // whatever the caller sent under that name.
        "proxy_set_header X-Forwarded-For \"\";",
    ] {
        assert!(
            config.contains(required),
            "proxy config dropped {required}:\n{config}"
        );
    }
    assert_eq!(
        config.matches("proxy_set_header X-Forwarded-For").count(),
        config.matches("proxy_pass").count(),
        "every proxied location must clear the forwarded-address header"
    );
}

/// The node sends a content policy per page and deliberately loosens it
/// on the two passkey pages (D39). `add_header` appends rather than
/// replaces, so anything set at the proxy is enforced *alongside* that one
/// at its strictest, and a blanket policy here revokes the exception on
/// exactly the pages that needed it. Nothing else in the suite would
/// notice, because the beta manifest disables passkeys.
#[test]
fn private_beta_proxy_leaves_the_content_policy_to_the_node() {
    let config = beta_proxy_config();
    // Directives only. The comment explaining the absence names the header,
    // and a bare `contains` over the whole file matches that prose.
    let directives: String = config
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !directives.contains("Content-Security-Policy"),
        "the proxy set a node-wide content policy over the node's per-page one:\n{config}"
    );
    for required in [
        "add_header Strict-Transport-Security",
        "add_header X-Frame-Options DENY",
        "add_header Referrer-Policy no-referrer",
        "add_header Permissions-Policy",
        "add_header X-Content-Type-Options nosniff",
    ] {
        assert!(
            config.contains(required),
            "proxy config dropped {required}, which the node does not send:\n{config}"
        );
    }
}

#[test]
fn private_beta_proxy_terminates_tls_and_separates_api_from_git_limits() {
    let config = beta_proxy_config();
    for required in [
        "server 127.0.0.1:8417",
        "return 308 https://$host$request_uri",
        "Strict-Transport-Security",
        "client_max_body_size 1m",
        "client_max_body_size 512m",
        "proxy_request_buffering off",
        "proxy_buffering off",
        "proxy_set_header Authorization $http_authorization",
        "client_header_timeout 10s",
    ] {
        assert!(
            config.contains(required),
            "proxy config dropped {required}:\n{config}"
        );
    }
    let small = config
        .find("client_max_body_size 1m")
        .expect("small default limit");
    let default_location = config
        .rfind("location / {")
        .expect("default proxy location");
    assert!(
        small < default_location,
        "the small default limit must cover GUI/API routes"
    );
}

/// The `ProgramArguments` array only — `StandardOutPath` and `Label` are
/// `<string>` elements too, and counting them would compare the plist's
/// supervision settings against the unit's argument list.
fn plist_argv(plist: &str) -> Vec<String> {
    let array = plist
        .split_once("<array>")
        .and_then(|(_, rest)| rest.split_once("</array>"))
        .map(|(inner, _)| inner)
        .expect("ProgramArguments array");
    let mut argv = Vec::new();
    let mut rest = array;
    while let Some(start) = rest.find("<string>") {
        let after = &rest[start + "<string>".len()..];
        let (value, tail) = after.split_once("</string>").expect("closed <string>");
        argv.push(value.to_string());
        rest = tail;
    }
    argv
}

/// Split on a single space deliberately: a double space yields an empty
/// element, which is how the empty-policy splice is caught below.
fn unit_argv(unit: &str) -> Vec<String> {
    let line = unit
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .expect("unit defines ExecStart");
    line["ExecStart=".len()..]
        .split(' ')
        .map(str::to_string)
        .collect()
}

#[test]
fn both_supervisors_launch_the_node_with_the_same_arguments() {
    for protected in [None, Some("/state/protected-refs")] {
        for scope in [false, true] {
            for tls in [
                None,
                Some(("/state/tls/fullchain.pem", "/state/tls/privkey.pem")),
            ] {
                for acl in [None, Some("/state/acl")] {
                    for accounts in [None, Some("/state/accounts.jsonl")] {
                        for proxy in [false, true] {
                            let plist = plist_argv(&render_tls(
                                protected, scope, tls, acl, accounts, proxy,
                            ));
                            let unit = unit_argv(&render_unit_tls(
                                protected, scope, tls, acl, accounts, proxy,
                            ));

                            // Without this the whole test passes vacuously when a renderer
                            // rejects its arguments and prints usage to stderr — which is
                            // exactly how the first version of this check reported success
                            // while comparing nothing to nothing.
                            assert!(
                                plist.len() >= 10,
                                "extracted {} arguments; the renderer did not run",
                                plist.len()
                            );
                            assert!(
                                !unit.iter().any(String::is_empty),
                                "unit ExecStart carries an empty argument (a spliced-in empty \
                 policy leaves a double space): {unit:?}"
                            );
                            // Equality alone passes when both renderers drop the flag, so
                            // its presence is pinned to the input, not to the sibling.
                            assert_eq!(
                                plist.iter().any(|arg| arg == "--require-scope"),
                                scope,
                                "--require-scope must appear exactly when the scope slot is set"
                            );
                            // The TLS pair and the bind are one decision: a public bind
                            // must carry the cert pair, loopback must carry neither.
                            assert_eq!(
                                plist.iter().any(|arg| arg == "--tls-cert"),
                                tls.is_some(),
                                "--tls-cert must appear exactly when the tls slots are set"
                            );
                            let bind = plist
                                .windows(2)
                                .find(|pair| pair[0] == "--bind")
                                .map(|pair| pair[1].clone())
                                .expect("--bind is always rendered");
                            assert_eq!(
                                bind,
                                if tls.is_some() {
                                    "0.0.0.0"
                                } else {
                                    "127.0.0.1"
                                },
                                "the bind must flip with the TLS pair and only with it"
                            );
                            // The gate that decides which repositories a credential can
                            // reach, pinned to its slot rather than to the sibling
                            // renderer, so both dropping it cannot read as agreement.
                            assert_eq!(
                                plist.iter().any(|arg| arg == "--acl-file"),
                                acl.is_some(),
                                "--acl-file must appear exactly when the acl slot is set"
                            );
                            // D36 invite-only self-service. Pinned to its slot for the
                            // same reason as the others, and worth its own line because
                            // the deployed node ran for a day with this flag missing
                            // from both renderers while its public landing page told
                            // visitors to open the invite they were sent.
                            assert_eq!(
                                plist.iter().any(|arg| arg == "--accounts-file"),
                                accounts.is_some(),
                                "--accounts-file must appear exactly when the accounts slot is set"
                            );
                            // Whether the node knows a proxy is in front decides how it
                            // spells every absolute URL it mints, an invite link among
                            // them, so it is pinned to its slot like the rest.
                            assert_eq!(
                                plist.iter().any(|arg| arg == "--behind-tls-proxy"),
                                proxy,
                                "--behind-tls-proxy must appear exactly when the proxy slot is set"
                            );
                            assert_eq!(
                                plist, unit,
                                "launchd and systemd must start the node with identical \
                 arguments; a flag added to one supervisor and not the other \
                 is a node running without the gate its operator configured"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Half a TLS pair is a configuration error, not a default: the node
/// would refuse the non-loopback bind anyway (invariant 9), and the
/// renderer refusing first is what keeps that from becoming a unit that
/// crash-loops under supervision.
#[test]
fn a_half_tls_pair_is_refused_by_both_renderers() {
    for (cert, key) in [
        ("/state/tls/fullchain.pem", ""),
        ("", "/state/tls/privkey.pem"),
    ] {
        let plist = render_output(
            "owner/repo.git\n",
            None,
            false,
            Some((cert, key)),
            None,
            None,
            false,
        );
        assert!(
            !plist.status.success(),
            "plist renderer accepted half a TLS pair"
        );
        let unit = render_unit_output(
            "owner/repo.git\n",
            None,
            false,
            Some((cert, key)),
            None,
            None,
            false,
        );
        assert!(
            !unit.status.success(),
            "unit renderer accepted half a TLS pair"
        );
    }
}

/// The repos file is what lets a new repo land as an appended line plus
/// a reinstall instead of a renderer signature change. Every entry must
/// reach both supervisors, identically ordered — and an empty list must
/// refuse to render, because a node with no `--create` installs no
/// pre-receive hook and its pushes are silently never sequenced.
#[test]
fn choirctl_runs_its_coloured_path_under_zsh() {
    // `choirctl` is zsh, and zsh reads `"$ESC[32m"` as a subscript on
    // ESC rather than as text after it -- `invalid subscript` -- where
    // the identical line in the POSIX-sh `gate` is fine. That shipped,
    // because the colour branch only runs on a terminal and every test
    // and every run in development captured its output instead.
    //
    // `zsh -n` does not catch it: the broken file parsed clean. Only
    // executing the branch does, so this executes it. No arguments, so
    // the script prints its usage and touches nothing, while the style
    // block at the top -- which is what breaks -- runs either way.
    let out = std::process::Command::new("zsh")
        .arg(repo_root().join("choirctl"))
        .env("FORCE_COLOR", "1")
        .output()
        .expect("zsh runs choirctl");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "choirctl fails on a colour-capable terminal: {said}"
    );
    assert!(
        !said.contains("invalid subscript"),
        "a zsh parameter expansion broke on the colour path: {said}"
    );
    assert!(
        said.contains("install-cli"),
        "choirctl printed no usage, so it did not get as far as its commands: {said}"
    );
}

#[test]
fn the_repos_file_renders_every_entry_and_refuses_an_empty_list() {
    let repos = "# comment\n\nowner/repo.git\nsecond/other.git\n";
    let plist_out = render_output(
        repos,
        Some("/state/protected-refs"),
        false,
        None,
        None,
        None,
        false,
    );
    assert!(plist_out.status.success());
    let unit_out = render_unit_output(
        repos,
        Some("/state/protected-refs"),
        false,
        None,
        None,
        None,
        false,
    );
    assert!(unit_out.status.success());

    let plist = plist_argv(&String::from_utf8(plist_out.stdout).expect("UTF-8 plist"));
    let unit = unit_argv(&String::from_utf8(unit_out.stdout).expect("UTF-8 unit"));
    assert_eq!(
        plist, unit,
        "a multi-repo list must reach both supervisors identically"
    );
    let created: Vec<&str> = plist
        .windows(2)
        .filter(|pair| pair[0] == "--create")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(
        created,
        ["owner/repo.git", "second/other.git"],
        "comments and blank lines are skipped; entry order is preserved"
    );

    for empty in ["", "# only a comment\n"] {
        assert!(
            !render_output(empty, None, false, None, None, None, false)
                .status
                .success(),
            "the plist renderer must refuse a repos list with no entries"
        );
        assert!(
            !render_unit_output(empty, None, false, None, None, None, false)
                .status
                .success(),
            "the unit renderer must refuse a repos list with no entries"
        );
    }

    // Both installers own the file's lifecycle: seed it once, and append
    // only a repo named explicitly, as an exact whole line — a bare
    // re-run must never resurrect a line the operator deleted.
    for name in [
        "scripts/flip/install_node.sh",
        "scripts/flip/install_node_linux.sh",
    ] {
        let installer = std::fs::read_to_string(repo_root().join(name)).expect(name);
        assert!(
            installer.contains("repos.list"),
            "{name} must wire the repos list"
        );
        assert!(
            installer.contains("grep -qxF"),
            "{name} must append only an exact missing line"
        );
    }
}

/// The follower feed must mirror every repo the node serves, not just
/// the canonical one: after the first imported repo, the node host was
/// the only holder of that repo's git objects. The remote command is
/// extracted from choirctl and executed here with real git against a
/// scratch HOME, because a loop that is merely grepped for could still
/// skip everything and read like success.
#[test]
fn the_follower_feed_pushes_every_listed_repo_and_names_the_unmirrored() {
    let driver = std::fs::read_to_string(repo_root().join("choirctl")).expect("choirctl source");

    // Both remote-mode call sites go through the one function; a stray
    // hardcoded single-repo push would silently shrink the follower.
    assert!(
        driver.matches("follower_feed").count() >= 3,
        "choirctl must define follower_feed and call it from mirror and sync"
    );
    assert!(
        !driver.contains("repos/choir/choir.git\" push"),
        "a hardcoded single-repo follower push survives in choirctl"
    );
    // D21 ordering inside the remote sync branch: canonical, follower,
    // then the backup pull.
    let sync = driver.find("sync)").expect("sync branch");
    let canonical = driver[sync..]
        .find("push_canonical.sh")
        .expect("canonical leg")
        + sync;
    let follower = driver[canonical..]
        .find("follower_feed")
        .expect("follower leg")
        + canonical;
    let backup = driver[follower..]
        .find("pull_backup.sh")
        .expect("backup leg")
        + follower;
    assert!(canonical < follower && follower < backup);

    // The command that actually runs on the node host.
    let def = driver
        .find("follower_feed()")
        .expect("follower_feed defined");
    let body = &driver[def..];
    let start = body.find("node_ssh '").expect("one remote command") + "node_ssh '".len();
    let end = body[start..].find("'\n}").expect("remote command closes") + start;
    let remote = &body[start..end];

    let home = std::env::temp_dir().join(format!("choir-follower-{}", std::process::id()));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(home.join(".choir/repos/agents")).unwrap();
    let sh = |cmd: &str| {
        std::process::Command::new("sh")
            .args(["-c", cmd])
            .env("HOME", &home)
            .output()
            .expect("sh runs")
    };

    // No repos.list: refuse, loudly.
    let out = sh(remote);
    assert!(!out.status.success(), "must refuse without a repos.list");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no repos.list"));

    // A served repo with no forgejo remote is named but not fatal, and
    // comments are skipped.
    std::fs::write(
        home.join(".choir/repos.list"),
        "# comment\nagents/demo.git\n",
    )
    .unwrap();
    let git = |cmd: &str| assert!(sh(cmd).status.success(), "fixture git failed: {cmd}");
    git("git init -q --bare \"$HOME/.choir/repos/agents/demo.git\"");
    let out = sh(remote);
    assert!(
        out.status.success(),
        "an unconfigured follower must not fail the run"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("NO forgejo remote"));

    // Configured: the push happens for real, into a second bare repo.
    git("git init -q \"$HOME/work\" && cd \"$HOME/work\" \
         && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m one \
         && git push -q \"$HOME/.choir/repos/agents/demo.git\" HEAD:refs/heads/main");
    git("git init -q --bare \"$HOME/follower.git\" \
         && git --git-dir \"$HOME/.choir/repos/agents/demo.git\" remote add forgejo \"$HOME/follower.git\"");
    let out = sh(remote);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("follower updated: agents/demo.git"));
    let shown = sh("git --git-dir \"$HOME/follower.git\" rev-parse refs/heads/main");
    assert!(
        shown.status.success(),
        "the follower never received the ref"
    );

    // A configured push that fails is the one thing that fails the run.
    std::fs::write(
        home.join(".choir/repos.list"),
        "agents/demo.git\nagents/bad.git\n",
    )
    .unwrap();
    git("git init -q --bare \"$HOME/.choir/repos/agents/bad.git\" \
         && git --git-dir \"$HOME/.choir/repos/agents/bad.git\" remote add forgejo \"$HOME/absent.git\"");
    let out = sh(remote);
    assert!(
        !out.status.success(),
        "a failed configured push must fail the run"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("push FAILED for agents/bad.git"));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("follower updated: agents/demo.git"),
        "one bad repo must not stop the others from being pushed"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Three papercuts from the first operating day, pinned so they stay
/// fixed: landing rounds must open the tunnel themselves (the raw CLI
/// at a dead forward burned three retries), the Linux binary swap must
/// survive ETXTBSY (cp straight over a running daemon fails), and the
/// backup pull must be schedulable — with the schedule refusing on a
/// node host, where pulling from yourself backs up nothing.
#[test]
fn the_landing_round_opens_the_tunnel_and_the_binary_swap_survives_etxtbsy() {
    let driver = std::fs::read_to_string(repo_root().join("choirctl")).expect("choirctl source");
    for case in ["\nreview)", "\nverdict)", "\nschedule-backup)"] {
        assert!(
            driver.contains(case),
            "choirctl lacks the {} command",
            case.trim()
        );
    }
    for start in [
        driver.find("\nreview)").unwrap(),
        driver.find("\nverdict)").unwrap(),
    ] {
        let case = &driver[start..start + driver[start..].find(";;").expect("case closes")];
        assert!(
            case.contains("tunnel_up"),
            "a landing seat does not open the tunnel first: {case}"
        );
        assert!(
            case.contains("choir_bin"),
            "a landing seat guesses at the CLI binary instead of refusing: {case}"
        );
    }

    let installer = std::fs::read_to_string(repo_root().join("scripts/flip/install_node_linux.sh"))
        .expect("linux installer source");
    assert!(
        installer.contains("scope-required.enabled"),
        "the linux installer must honour the scope marker like the macOS one"
    );
    let copy = installer
        .find("$BIN_DIR/$f.new\"")
        .expect("installer must copy to .new, not straight over the running binary");
    let rename = installer
        .find("mv \"$BIN_DIR/$f.new\" \"$BIN_DIR/$f\"")
        .expect("installer must rename the copy into place");
    assert!(copy < rename, "the rename must follow the copy");

    // The timer's refusal path runs for real; it exits before launchd.
    let home = std::env::temp_dir().join(format!("choir-timer-{}", std::process::id()));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(&home).unwrap();
    let out = std::process::Command::new("sh")
        .arg(repo_root().join("scripts/flip/install_pull_timer.sh"))
        .env("HOME", &home)
        .output()
        .expect("timer installer runs");
    assert!(
        !out.status.success(),
        "the timer must refuse where there is no node-remote marker"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("no ~/.choir/node-remote"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn the_linux_installer_carries_the_same_policy_wiring() {
    let installer = std::fs::read_to_string(repo_root().join("scripts/flip/install_node_linux.sh"))
        .expect("linux installer source");
    assert!(installer.contains("review-gates.enabled"));
    assert!(installer.contains("render_node_service.sh"));
    assert!(installer.contains("validate_review_policy.sh"));
    let here = installer
        .find("HERE=")
        .expect("installer defines helper directory");
    for helper in ["validate_review_policy.sh", "render_node_service.sh"] {
        assert!(
            here < installer.find(helper).expect("installer invokes helper"),
            "installer must define HERE before invoking {helper}"
        );
    }
    // The macOS installer builds in place; this one must not, because the
    // host it targets cannot compile the workspace.
    assert!(
        !installer.contains("cargo build"),
        "the Linux installer must not build on the node host"
    );
    // Both installers must find the D29 ACL file and pass its slot. The
    // renderer comparison test drives the renderers directly, so it
    // cannot see an installer that reads the file and then forgets to
    // hand it over — which would leave one platform serving every
    // repository to every credential while its ACL sits there looking
    // configured.
    for path in [
        "scripts/flip/install_node.sh",
        "scripts/flip/install_node_linux.sh",
    ] {
        let source = std::fs::read_to_string(repo_root().join(path)).expect("installer source");
        assert!(
            source.contains("$STATE/acl"),
            "{path} never reads the ACL file"
        );
        // Matched against the renderer call, not against `$ACL` anywhere:
        // the variable also appears in the branch that sets it, so the
        // looser check passed with the argument deleted.
        assert!(
            source.contains("\"$TLS_KEY\" \"$ACL\""),
            "{path} never passes the ACL slot to its renderer"
        );
        // D71's marker, held to the same standard and for the same reason
        // the ACL is: an installer that reads a marker and forgets to pass
        // it produces a node the operator believes has the feature on.
        assert!(
            source.contains("passkeys.enabled"),
            "{path} never reads the WebAuthn marker"
        );
        assert!(
            source.contains("\"$BEHIND_TLS_PROXY\" \"$WEBAUTHN\""),
            "{path} never passes the WebAuthn slot to its renderer"
        );
        // And it refuses rather than quietly dropping the switch when the
        // accounts file it depends on is missing. A marker an installer
        // ignores is worse than one it rejects: the operator has said what
        // they want and been told nothing.
        assert!(
            source.contains("passkeys are enrolled on issued accounts"),
            "{path} accepts the WebAuthn marker without an accounts file"
        );
    }
}

/// Both installers stop the running node before installing the new one,
/// so both must prove a binary exists first. The Linux one always has.
/// The macOS one did not, and because cargo resolves `target-dir` from
/// the working directory rather than from `--manifest-path`, running it
/// from a worktree built into `shared-target` and pointed launchd at a
/// path that was never written — taking the canonical node down with no
/// binary for KeepAlive to restart.
#[test]
fn both_installers_refuse_before_stopping_a_running_node() {
    for (name, stop_verb) in [
        ("scripts/flip/install_node.sh", "launchctl bootout"),
        (
            "scripts/flip/install_node_linux.sh",
            "systemctl --user restart",
        ),
    ] {
        let raw = std::fs::read_to_string(repo_root().join(name)).expect(name);
        // Comments mention both the guard and the stop verb, and a
        // comment above the guard explaining what it protects would
        // otherwise read as the stop happening first.
        let src: String = raw
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let guard = src
            .find("-x \"$BIN\"")
            .or_else(|| src.find("-x \"$f\""))
            .unwrap_or_else(|| panic!("{name} must test the binary is executable"));
        let stop = src
            .find(stop_verb)
            .unwrap_or_else(|| panic!("{name} must stop the service"));
        assert!(
            guard < stop,
            "{name} runs `{stop_verb}` before proving a binary exists; \
             that is a node stopped with nothing to restart it with"
        );
    }

    // The macOS installer additionally must not assume the target dir.
    let mac = std::fs::read_to_string(repo_root().join("scripts/flip/install_node.sh"))
        .expect("macos installer");
    assert!(
        mac.contains("cargo metadata") && mac.contains("target_directory"),
        "the macOS installer must ask cargo where it built, not assume $REPO_DIR/target"
    );
    let code: String = mac
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("$REPO_DIR/target/release"),
        "the macOS installer still hardcodes $REPO_DIR/target/release somewhere"
    );
}

fn validate(keys: &str, reviewers: &str, protected: &str) -> std::process::ExitStatus {
    validate_with_acl(keys, reviewers, protected, None)
}

fn validate_with_acl(
    keys: &str,
    reviewers: &str,
    protected: &str,
    acl: Option<&str>,
) -> std::process::ExitStatus {
    // The pid alone no longer separates two of these: the merged harness
    // runs its modules as threads of one process, so a second caller
    // would delete the first one's directory mid-run.
    let work = std::env::temp_dir().join(format!(
        "choir-policy-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("keys"), keys).unwrap();
    std::fs::write(work.join("reviewers"), reviewers).unwrap();
    std::fs::write(work.join("protected"), protected).unwrap();
    let mut args = vec![
        work.join("keys"),
        work.join("reviewers"),
        work.join("protected"),
    ];
    if let Some(acl) = acl {
        std::fs::write(work.join("acl"), acl).unwrap();
        args.push(work.join("acl"));
    }
    let status = std::process::Command::new("sh")
        .arg(repo_root().join("scripts/flip/validate_review_policy.sh"))
        .args(&args)
        .stderr(std::process::Stdio::null())
        .status()
        .expect("validate review policy");
    std::fs::remove_dir_all(work).ok();
    status
}

#[test]
fn review_policy_validation_fails_closed() {
    let keys = "operator/writer aa\nreview-a/agent bb\nreview-a/second dd\n\
                review-b/agent cc\nreview-c/agent ee\n";
    let protected = "owner/repo.git:refs/heads/main\n";

    // Three distinct operators is the smallest drawable quorum: an
    // approval needs two, and the author's own operator is never drawn.
    assert!(validate(
        keys,
        "review-a/agent\nreview-b/agent\nreview-c/agent\n",
        protected
    )
    .success());
    // Two used to pass here, and rendered a unit whose reviews could be
    // opened and assigned but never reach weight two.
    assert!(!validate(keys, "review-a/agent\nreview-b/agent\n", protected).success());
    assert!(!validate(keys, "review-a/agent\nreview-a/second\n", protected).success());
    assert!(!validate(keys, "review-a/agent\nreview-b/missing\n", protected).success());
    assert!(!validate(keys, "review-a/agent\nreview-b/agent\nreview-c/agent\n", "").success());
}

/// D42: a repository with an owner is landed on by that owner without a
/// reviewer ever being drawn, so demanding a pool for it refuses a
/// configuration the daemon supports — the one a person working alone on
/// their own repository has.
#[test]
fn an_owned_repository_needs_no_reviewer_pool() {
    let keys = "operator/writer aa\n";
    let protected = "owner/repo.git:refs/heads/main\n";
    let empty_pool = "# nobody\n";

    assert!(validate_with_acl(
        keys,
        empty_pool,
        protected,
        Some("solo owner/repo.git own\n")
    )
    .success());
    // `*` is how `Effective::has_owner` covers every repository.
    assert!(validate_with_acl(keys, empty_pool, protected, Some("solo * own\n")).success());

    // Weaker grants are not the owner basis, and neither is an owner of
    // some other repository.
    assert!(!validate_with_acl(
        keys,
        empty_pool,
        protected,
        Some("solo owner/repo.git write\n")
    )
    .success());
    assert!(!validate_with_acl(
        keys,
        empty_pool,
        protected,
        Some("solo other/repo.git own\n")
    )
    .success());

    // One owned repository does not license an unowned one: every
    // protected repository has to clear, or the pool rule applies.
    assert!(!validate_with_acl(
        keys,
        empty_pool,
        "owner/repo.git:refs/heads/main\nowner/other.git:refs/heads/main\n",
        Some("solo owner/repo.git own\n")
    )
    .success());

    // A named reviewer still has to have a bound key, owner or not.
    assert!(!validate_with_acl(
        keys,
        "review-a/agent\n",
        protected,
        Some("solo owner/repo.git own\n")
    )
    .success());
}

/// The mirror push makes two round trips to a VM ~275 ms away, and a
/// fresh SSH handshake to it measured 3.78 s against 0.55 s on a reused
/// connection. Losing the multiplexing options silently triples the
/// cost of `choirctl sync` — nothing fails, it just gets slow again,
/// which is exactly the kind of regression no other check would catch.
#[test]
fn the_mirror_push_reuses_one_ssh_connection() {
    let script = std::fs::read_to_string(repo_root().join("scripts/push_mirror.sh"))
        .expect("mirror push source");

    for option in [
        "ControlMaster=auto",
        "ControlPath=",
        // 60s was too short to ever hit: syncs are minutes apart, so
        // the master had always expired and every sync paid a full
        // handshake anyway. The window has to span a working session.
        "ControlPersist=600",
        // Without this ssh offers the agent key first and the server
        // refuses it: one wasted round trip before the real key.
        "IdentitiesOnly=yes",
        // The leg runs detached now. Without these, an ssh that meets a
        // prompt or a black-holed route waits forever, no receipt is
        // ever written, and that is indistinguishable from a run still
        // in progress.
        "BatchMode=yes",
        "ConnectTimeout=",
    ] {
        assert!(
            script.contains(option),
            "mirror push dropped {option}; every VM round trip pays a full handshake again"
        );
    }

    // The stage timings are the point: this script was once tuned
    // against a model of where its time went, the model was wrong, and
    // nothing in the output could have revealed that. A run that
    // reports connect/transfer/push separately settles it.
    for stage in [
        "connect %.1fs",
        "git push %.1fs",
        "box-local push %.1fs",
        "oplog %.1fs",
        "total %.1fs",
    ] {
        assert!(
            script.contains(stage),
            "mirror push stopped reporting {stage}; the next slowdown gets guessed at again"
        );
    }

    // choirctl runs this as `sh push_mirror.sh`, so the #!/bin/zsh line
    // is never consulted and /bin/sh on macOS is bash 3.2. A zsh-only
    // builtin therefore fails at runtime, and under `set -e` that means
    // the mirror push is skipped while the canonical half still reports
    // success — which is how a sync once landed on the node and silently
    // never reached the mirror. `sh -n` cannot catch it: the syntax is
    // fine, the command just does not exist.
    let driver = std::fs::read_to_string(repo_root().join("choirctl")).expect("choirctl source");
    assert!(
        driver.contains("sh \"$HERE/scripts/push_mirror.sh\""),
        "choirctl no longer runs the mirror push with sh; revisit the shell assumptions below"
    );
    for line in script.lines().filter(|l| !l.trim_start().starts_with('#')) {
        for zshism in ["zmodload", "EPOCHREALTIME", "setopt", "autoload"] {
            assert!(
                !line.contains(zshism),
                "mirror push uses the zsh-only {zshism} but is run with sh: {line}"
            );
        }
    }

    // Proving it parses is not proving it runs. Execute the script's own
    // clock under the shell that actually invokes it.
    let now_def = script
        .lines()
        .find(|line| line.starts_with("now()"))
        .expect("mirror push defines now()");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{now_def}; now"))
        .output()
        .expect("run now() under sh");
    let stamp = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        stamp.parse::<f64>().map(|t| t > 1.0e9).unwrap_or(false),
        "now() did not produce a unix timestamp under sh, got {stamp:?} (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );

    // main and tags go to Forgejo in one push, not two. The repo has no
    // tags at all, so the second push was a round trip to a shared-core
    // VM to say nothing: 0.93s for the pair against 0.46s combined,
    // measured on the mirror.
    assert!(
        script.contains("git push -q mirror main --tags"),
        "the box-local push split main and tags into two Forgejo round trips again"
    );

    // rsync walked 707 files and 37.5 MB to move a delta git already
    // knows how to compute — 1.74 s even with nothing changed, because
    // almost every one of those files is an immutable content-addressed
    // object. Going back to it also resurrects two workarounds it
    // needed: recreating the mirror remote after .git/config was
    // clobbered, and a .gitignore filter to keep local-only files off
    // the VM, which git gives for free by only pushing commits.
    // Non-comment lines only: the comments above explain why rsync went
    // away and would otherwise match themselves.
    for line in script.lines().filter(|l| !l.trim_start().starts_with('#')) {
        assert!(
            !line.contains("rsync "),
            "the mirror transfer went back to rsync; git sends only the missing objects: {line}"
        );
    }

    // Two syncs in quick succession would otherwise push into the same
    // repo concurrently. flock is not stock on macOS, so mkdir is the
    // atomic primitive; it must wait rather than skip, or the newest
    // commit is the one that gets dropped.
    assert!(
        script.contains("mkdir \"$LOCK\""),
        "mirror push lost its lock; concurrent syncs race on the receiving repo"
    );

    // Both trips must go through the same option set, or the second one
    // opens its own connection and the multiplexing buys nothing.
    let rsync = script
        .lines()
        .find(|line| line.trim_start().starts_with("export GIT_SSH_COMMAND"))
        .expect("mirror push sends objects over the shared ssh connection");
    let ssh = script
        .lines()
        .find(|line| line.trim_start().starts_with("ssh \""))
        .expect("mirror push runs the box-local push over ssh");
    assert!(
        rsync.contains("ssh_opts") && ssh.contains("ssh_opts"),
        "the git transfer and the box-local push must share one option set:\n  {rsync}\n  {ssh}"
    );
}

/// The mirror leg runs detached, so nothing blocks on it — which means
/// its failures are invisible unless the receipt is both written and
/// read. Grepping for the reporting code would only prove it exists;
/// this runs it, because the interesting failure is a receipt-reader
/// that returns the wrong verdict rather than one that is missing.
#[test]
fn the_mirror_receipt_is_read_not_merely_written() {
    let driver = std::fs::read_to_string(repo_root().join("choirctl")).expect("choirctl source");

    // Detached, and only after the canonical push returns: D21 ordering
    // survives backgrounding precisely because `set -e` stops before
    // this line when the canonical half fails.
    assert!(
        driver.contains("nohup sh \"$HERE/scripts/push_mirror.sh\""),
        "choirctl sync no longer backgrounds the mirror leg"
    );
    let sync = driver
        .find("sync)")
        .and_then(|start| driver[start..].find("nohup").map(|n| start + n))
        .expect("sync backgrounds the mirror");
    let canonical = driver[..sync]
        .rfind("push_canonical.sh")
        .expect("sync pushes canonical first");
    assert!(
        canonical < sync,
        "the mirror leg must start after the canonical push, or the follower can lead"
    );
    assert!(
        driver.contains("mirror_line"),
        "nothing surfaces the receipt; a detached failure would be invisible"
    );

    // Execute the verdict function itself, under the shell that runs it.
    let body: String = driver
        .lines()
        .skip_while(|l| !l.starts_with("mirror_outcome()"))
        .take_while(|l| *l != "}")
        .chain(std::iter::once("}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.starts_with("mirror_outcome()"),
        "choirctl no longer defines mirror_outcome"
    );

    let work = std::env::temp_dir().join(format!("choir-receipt-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let receipt = work.join("mirror.receipt");

    let verdict = |case: &str| -> String {
        let script = format!(
            "STATE={state}; RECEIPT={receipt}; {body}; mirror_outcome",
            state = work.display(),
            receipt = receipt.display(),
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    assert_eq!(verdict("absent"), "NONE");
    std::fs::write(&receipt, "started 1 abc\nmirror: ...\nmirror updated\n").unwrap();
    assert_eq!(verdict("complete"), "OK");
    // A truncated receipt is the shape a killed or timed-out run leaves.
    std::fs::write(&receipt, "started 1 abc\nssh: connect timed out\n").unwrap();
    assert_eq!(verdict("truncated"), "FAILED");
    // The lock, not the receipt, is what separates running from dead.
    std::fs::create_dir_all(work.join("mirror.lock")).unwrap();
    assert_eq!(verdict("in flight"), "RUNNING");

    std::fs::remove_dir_all(work).ok();
}

/// Writing a backup and being able to restore one are different claims,
/// and only the second matters. This pins the checks that separate them.
#[test]
fn the_backup_is_verified_by_pulling_it_back_not_by_having_written_it() {
    let script = std::fs::read_to_string(repo_root().join("scripts/verify_backup.sh"))
        .expect("scripts/verify_backup.sh");
    let code: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    // A restore that boots needs all five; the reviewers file is the one
    // whose absence stops the daemon outright.
    for needed in [
        "reviewers",
        "protected-refs",
        "newcomer-audit.jsonl",
        "review-adjudications.jsonl",
        "acl",
        "private-beta.manifest",
    ] {
        assert!(
            code.contains(needed),
            "verify-backup stopped checking for {needed}; a restore can stop booting again"
        );
    }
    // An assertion about the backup itself, so it holds even if someone
    // copies a file up by hand rather than through push_mirror.sh.
    assert!(
        code.contains("SECRETS IN THE BACKUP"),
        "verify-backup must fail loudly if a token or key reached the backup"
    );
    // Prefix, not equality: the live log grows between syncs.
    assert!(
        code.contains("prefix") && code.contains("cmp"),
        "verify-backup must compare the backup as a prefix of the live log"
    );
    assert!(
        code.contains("--verify-log") && code.contains("format/sequence/parent/hash"),
        "verify-backup must use the release verifier for format, sequence, parent, and hash checks"
    );

    let driver = std::fs::read_to_string(repo_root().join("choirctl")).expect("choirctl");
    assert!(
        driver.contains("verify-backup)") && driver.contains("verify_backup.sh"),
        "choirctl must expose verify-backup, or nothing ever runs it"
    );

    let syntax = std::process::Command::new("sh")
        .arg("-n")
        .arg(repo_root().join("scripts/verify_backup.sh"))
        .output()
        .expect("run sh -n");
    assert!(
        syntax.status.success(),
        "verify_backup.sh is not valid sh: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );
}

/// The same properties, pinned on the pair that actually runs.
///
/// There are two backup families in this repository — `scripts/` and
/// `scripts/flip/` — and every test above this one reads the first while
/// the pull leg runs the second. That gap is not theoretical: the flip
/// pull shipped no `refs.snapshot` for its whole life, so the restore's
/// D25 attestation check silently skipped on every backup an operator
/// has ever taken, and nothing here noticed. A property is only pinned
/// on the implementation that runs.
///
/// Which is now split. The pull is still shell, because it ssh's to a
/// specific host. The checks and the restore are `choir backup verify`
/// and `choir backup restore`, so their half of this test reads the
/// Rust — asserting against `scripts/flip/verify_backup.sh` would pin
/// the properties on a file nothing calls, which is the exact failure
/// this test was written to prevent.
#[test]
fn the_backup_pair_choirctl_runs_is_pinned_too() {
    let strip = |path: &str| -> String {
        std::fs::read_to_string(repo_root().join(path))
            .unwrap_or_else(|_| panic!("{path}"))
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let pull = strip("scripts/flip/pull_backup.sh");
    // The log, its pin, and the attestation the restore checks the
    // replayed view against. Without the third, that check is skipped
    // and the restore says nothing about it either way.
    //
    // Both ends of each file's journey, not the bare name: a script that
    // merely mentions `refs.snapshot` in an error string passes a
    // contains-check while pulling nothing, which is what the first
    // version of this assertion did.
    for needed in ["ops.jsonl", "node.fingerprint", "refs.snapshot"] {
        assert!(
            pull.contains(&format!("$INCOMING/{needed}"))
                && pull.contains(&format!("$DEST/{needed}")),
            "the pull stopped carrying {needed} into the promoted backup"
        );
    }
    assert!(
        !pull.contains("node.key"),
        "the pulled backup must never carry node.key off the node that owns it"
    );
    assert!(
        pull.contains("checksum mismatch") && pull.contains("--verify-log"),
        "the pull must compare checksums and run the release verifier over what arrived"
    );
    // The check no single-file checksum can make: an append-only log
    // that shrank or was rewritten still checksums fine on its own.
    assert!(
        pull.contains("prefix"),
        "the pull stopped requiring the held copy to be a prefix of the node's log"
    );
    assert!(
        pull.contains("REFUSING"),
        "the pull must refuse a policy tar that gained a credential"
    );
    // All nine names, because the three this leg once left behind --
    // adjudications, ownership, beta limits -- are exactly the ones
    // whose absence makes a restored node enforce less than the node it
    // replaces, while booting and serving normally.
    for needed in [
        "keys",
        "reviewers",
        "protected-refs",
        "newcomer-audit.jsonl",
        "newcomer-adjudications.jsonl",
        "review-adjudications.jsonl",
        "acl",
        "private-beta.manifest",
        "repos.list",
    ] {
        assert!(
            pull.contains(needed),
            "the pull stopped asking the node for {needed}"
        );
    }

    // Three that stop a restore booting, six that leave it enforcing
    // less than the node it replaces. Both sets are reported; only the
    // first fails, because a node that protects no ref has no
    // protected-refs file to back up.
    let checked: Vec<&str> = choir_cli::backup::REQUIRED_POLICY
        .iter()
        .chain(choir_cli::backup::OPTIONAL_POLICY)
        .copied()
        .collect();
    for needed in [
        "keys",
        "reviewers",
        "repos.list",
        "protected-refs",
        "newcomer-audit.jsonl",
        "newcomer-adjudications.jsonl",
        "review-adjudications.jsonl",
        "acl",
        "private-beta.manifest",
    ] {
        assert!(
            checked.contains(&needed),
            "backup verify stopped looking for {needed}; a restore can enforce less without saying so"
        );
    }
    // The same nine, demanded again of the restore, which is the one
    // that has to boot a daemon with them. Compared as sets rather than
    // counted: two lists of nine that disagree about *which* nine is
    // exactly the drift this pins, and a length check would miss it.
    let mut restored: Vec<&str> = choir_cli::restore::REQUIRED
        .iter()
        .chain(choir_cli::restore::OPTIONAL)
        .copied()
        .collect();
    let mut checked_sorted = checked.clone();
    restored.sort_unstable();
    checked_sorted.sort_unstable();
    assert_eq!(
        checked_sorted, restored,
        "verify and restore disagree about what a backup holds"
    );
    assert!(
        choir_cli::backup::is_secret("auth") && choir_cli::backup::is_secret("node.key"),
        "backup verify must fail a backup holding a credential"
    );
    // A skipped proof reads like a passed one, so the attestation is
    // reported either way rather than checked only when present.
    assert!(
        choir_cli::backup::verify(std::path::Path::new("/nonexistent"), None)
            .iter()
            .all(|c| c.status == choir_cli::doctor::Status::Fail),
        "a directory that is not a backup must not pass anything"
    );

    let driver = std::fs::read_to_string(repo_root().join("choirctl")).expect("choirctl");
    for wired in ["scripts/flip/pull_backup.sh", "scripts/push_mirror.sh"] {
        assert!(
            driver.contains(wired),
            "choirctl no longer names {wired}, so nothing runs it"
        );
    }
    // The two that moved. `choirctl` delegates rather than keeping a
    // second implementation, so a reader with the old muscle memory
    // reaches the code the release actually ships.
    for wired in ["backup verify", "backup restore"] {
        assert!(
            driver.contains(wired),
            "choirctl no longer delegates to `choir {wired}`"
        );
    }

    for script in [
        "scripts/flip/pull_backup.sh",
        "scripts/flip/verify_backup.sh",
        "scripts/restore_from_backup.sh",
    ] {
        let syntax = std::process::Command::new("sh")
            .arg("-n")
            .arg(repo_root().join(script))
            .output()
            .expect("run sh -n");
        assert!(
            syntax.status.success(),
            "{script} is not valid sh: {}",
            String::from_utf8_lossy(&syntax.stderr)
        );
    }
}

/// The op log is the only state in the system with exactly one copy:
/// git bundles carry commits, and `ops.jsonl` has never been a git
/// object. This asserts the backup leg exists, that it verifies rather
/// than assumes, and — the property that actually matters — that it
/// never carries the signing key off the node that owns it.
#[test]
fn the_oplog_backup_carries_the_log_and_the_pin_but_never_the_key() {
    let script = std::fs::read_to_string(repo_root().join("scripts/push_mirror.sh"))
        .expect("scripts/push_mirror.sh");
    let code: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("ops.jsonl"),
        "the mirror stopped backing up the op log; it exists in exactly one place again"
    );
    assert!(
        code.contains("node.fingerprint"),
        "the pin must travel with the log, or a restore silently appends under a new identity"
    );
    // The whole point of splitting key from log. A backup holding the
    // key lets whoever holds the backup keep signing as this node.
    assert!(
        !code.contains("node.key"),
        "the op-log backup must never carry node.key off the node that owns it"
    );
    // Verified, not hoped: a silent truncation reads exactly like a
    // successful backup until the day it is restored.
    assert!(
        code.contains("OP LOG BACKUP MISMATCH"),
        "the backup stopped comparing checksums; a truncated copy now looks like a good one"
    );
    // Atomic publish: an interrupted transfer must leave the previous
    // good backup, not a half-written log that still parses.
    assert!(
        code.contains("ops.jsonl.part") && code.contains("mv "),
        "the backup stopped writing .part then renaming; an interrupted run truncates the backup"
    );

    // Rehearsing the restore showed the log alone is not enough: a node
    // rebuilt from ops.jsonl refused to boot without --reviewers-file,
    // so the policy files have to travel or the backup restores a
    // ledger onto a node that will not start.
    let list = code
        .lines()
        .find(|l| l.contains("for f in") && l.contains("reviewers"))
        .expect("the policy-file backup list");
    for needed in [
        "keys",
        "reviewers",
        "protected-refs",
        "newcomer-audit.jsonl",
        "review-adjudications.jsonl",
        "acl",
        "private-beta.manifest",
    ] {
        assert!(
            list.contains(needed),
            "the policy backup dropped {needed}; a restore stops booting again"
        );
    }
    // Same rule as the key: a token or a private key in the backup turns
    // an availability measure into a credential-distribution channel.
    for forbidden in ["auth", ".pem", ".key"] {
        assert!(
            !list.contains(forbidden),
            "the policy backup list names {forbidden}; secrets must not travel with it"
        );
    }
    assert!(
        code.contains("policy.part"),
        "the policy backup stopped staging into .part; a failed extract leaves a partial policy set"
    );

    // The script is run as `sh`, never as the zsh in its shebang, and a
    // runtime-only failure here would skip the backup while the receipt
    // still ended in success. `sh -n` catches at least the syntax half.
    let syntax = std::process::Command::new("sh")
        .arg("-n")
        .arg(repo_root().join("scripts/push_mirror.sh"))
        .output()
        .expect("run sh -n");
    assert!(
        syntax.status.success(),
        "push_mirror.sh is not valid sh: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );
}

/// After the D20 host move the backup direction inverts: the node host
/// holds the live log and this machine pulls the offsite copy. Same
/// non-negotiables as the push direction, pinned the same way: the log,
/// the pin, and the policy travel; the signing key and the token never
/// do; and every copy is verified rather than hoped.
#[test]
fn the_pulled_backup_carries_the_log_and_the_pin_but_never_the_key() {
    let script = std::fs::read_to_string(repo_root().join("scripts/pull_backup.sh"))
        .expect("scripts/pull_backup.sh");
    let code: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("ops.jsonl") && code.contains("node.fingerprint"),
        "the pull must carry the log and its pin, or a restore appends under a new identity"
    );
    assert!(
        !code.contains("node.key"),
        "the pulled backup must never carry node.key off the node that owns it"
    );
    assert!(
        code.contains("OP LOG BACKUP MISMATCH"),
        "the pull stopped comparing checksums; a truncated copy now looks like a good one"
    );
    assert!(
        code.contains("ops.jsonl.part") && code.contains("mv "),
        "the pull stopped writing .part then renaming; an interrupted run truncates the backup"
    );
    // Append-only is the backup's integrity model: each pull must extend
    // the previous one, never rewrite it.
    assert!(
        code.contains("cmp") && code.contains("prefix"),
        "the pull stopped checking the previous copy is a prefix of the new one"
    );
    assert!(
        code.contains("--verify-log") && code.contains("format/sequence/parent/hash"),
        "the pull must use the release verifier for format, sequence, parent, and hash checks"
    );
    // The objects leg: the op log carries ref history, and the objects
    // those refs name must land off-host too — the on-box follower is
    // the same failure domain. One full bundle per served repo, verified
    // as complete before it replaces the previous copy, and repos.list
    // itself travels with policy or a restore retracts every ref of a
    // repo the installer did not seed.
    for required in [
        "repos.list",
        "bundle create",
        "bundle verify",
        "complete history",
        "$bundle.part",
    ] {
        assert!(
            code.contains(required),
            "the pull's objects leg lost `{required}`; the git objects have no off-host copy"
        );
    }
    // Pull by explicit name, never by directory: a directory inherits
    // whatever lands in it, including a key copied there by accident.
    let list = code
        .lines()
        .find(|l| {
            (l.contains("for f in") || l.contains("POLICY_FILES=")) && l.contains("reviewers")
        })
        .expect("the policy-file pull list");
    for needed in [
        "keys",
        "reviewers",
        "protected-refs",
        "newcomer-audit.jsonl",
        "review-adjudications.jsonl",
        "acl",
        "private-beta.manifest",
    ] {
        assert!(
            list.contains(needed),
            "the policy pull dropped {needed}; a restore stops booting again"
        );
    }
    for forbidden in ["auth", ".pem", ".key"] {
        assert!(
            !list.contains(forbidden),
            "the policy pull list names {forbidden}; secrets must not travel with it"
        );
    }
    // Direction guard: run where the live log lives, a pull would
    // overwrite the real backup relation with a vacuous self-copy.
    assert!(
        code.contains("node-remote"),
        "the pull lost its direction guard; run on the node host it clobbers the backup"
    );

    let driver = std::fs::read_to_string(repo_root().join("choirctl")).expect("choirctl");
    assert!(
        driver.contains("pull-backup)") && driver.contains("pull_backup.sh"),
        "choirctl must expose pull-backup, or nothing ever runs it"
    );
    // And the old direction must refuse to run after the move: its oplog
    // leg would overwrite the historical backup with a frozen stale log.
    let push = std::fs::read_to_string(repo_root().join("scripts/push_mirror.sh"))
        .expect("scripts/push_mirror.sh");
    assert!(
        push.contains("node-remote"),
        "push_mirror.sh lost its direction guard; run after the host move it clobbers the backup"
    );

    let syntax = std::process::Command::new("sh")
        .arg("-n")
        .arg(repo_root().join("scripts/pull_backup.sh"))
        .output()
        .expect("run sh -n");
    assert!(
        syntax.status.success(),
        "pull_backup.sh is not valid sh: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );
}

/// D47's three outcomes, run rather than grepped.
///
/// The rule this encodes came from a live failure: an imported
/// repository has no attested ref-state, so comparing its bundle
/// against one compares four real refs with an empty set, and every
/// hourly backup exits nonzero forever. The repair is not to silence
/// the check — for that repository the op log genuinely is not the
/// authority on ref state — it is to report it as unverified and keep
/// failing only on the case the check exists for.
///
/// Asserted by executing the script with fixture files, because a test
/// that greps `pull_backup.sh` for a message is a test of how the
/// message is spelled today.
#[test]
fn an_unattested_repo_is_reported_unverified_and_divergence_still_fails() {
    let work = std::env::temp_dir().join(format!("choir-attest-check-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let script = repo_root().join("scripts/attest_check.sh");

    let refs = "aaaa refs/heads/main\nbbbb refs/tags/v1\n";
    let attested = work.join("attested");
    let bundled = work.join("bundled");
    let empty = work.join("empty");
    std::fs::write(&empty, "").expect("empty fixture");

    let run = |a: &std::path::Path, b: &std::path::Path| -> (bool, String) {
        let out = std::process::Command::new("sh")
            .arg(&script)
            .arg("some/repo.git")
            .arg(a)
            .arg(b)
            .output()
            .expect("attest_check runs");
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text)
    };

    // Attested and matching: the ordinary case, and it must stay quiet
    // about being anything else.
    std::fs::write(&attested, refs).expect("attested fixture");
    std::fs::write(&bundled, refs).expect("bundle fixture");
    let (ok, said) = run(&attested, &bundled);
    assert!(ok, "a matching bundle failed: {said}");
    assert!(said.contains("verified"), "{said}");
    assert!(
        !said.contains("UNVERIFIED"),
        "a matching bundle read as unverified: {said}"
    );

    // Attested and different: divergence, and it must still stop the
    // run. This is the whole reason the check exists, and the failure
    // mode of getting D47 wrong is turning this into a warning.
    std::fs::write(&bundled, "cccc refs/heads/main\n").expect("bundle fixture");
    let (ok, said) = run(&attested, &bundled);
    assert!(!ok, "divergence was tolerated: {said}");
    assert!(said.contains("divergence"), "{said}");

    // No attested rows: reported by name, and the run continues.
    std::fs::write(&bundled, refs).expect("bundle fixture");
    let (ok, said) = run(&empty, &bundled);
    assert!(ok, "an unattested repo failed the run: {said}");
    assert!(
        said.contains("UNVERIFIED"),
        "the unverified state was not announced: {said}"
    );
    assert!(
        said.contains("some/repo.git"),
        "the repo was not named: {said}"
    );
    assert!(
        said.contains("never sequenced"),
        "the reason is missing, so a reader cannot tell this from divergence: {said}"
    );

    // ...and the caller counts them, so the last line of a run can never
    // read as full verification when it was not.
    let pull = std::fs::read_to_string(repo_root().join("scripts/pull_backup.sh"))
        .expect("scripts/pull_backup.sh");
    assert!(
        pull.contains("attest_check.sh") && pull.contains("unverified"),
        "pull_backup.sh does not use the checker or does not count its unverified repos"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// The installer's progress line must name what cargo is compiling.
///
/// It did not, for its whole first life: the extraction was a BSD-sed
/// basic regular expression using `\|` alternation, which BSD sed does
/// not have. It matched nothing, silently, so every build showed the
/// fallback word and was exactly as uninformative as the static `...`
/// the spinner had replaced -- which is the complaint the spinner was
/// built to answer.
///
/// The expression is lifted out of `choirctl` and run here on cargo
/// output captured from a real `cargo build --release -p choir-cli`,
/// not on a line written by hand: a hand-written sample would have the
/// leading whitespace and field order I *believe* cargo uses, and it is
/// precisely that belief the last version got wrong.
#[test]
fn the_installers_progress_line_names_the_crate_and_not_the_path() {
    let driver = std::fs::read_to_string(repo_root().join("choirctl")).expect("choirctl source");
    let open = driver
        .find("SPIN_AWK='")
        .expect("the progress extraction is a named awk program")
        + "SPIN_AWK='".len();
    let end = driver[open..].find('\'').expect("the awk program closes") + open;
    let program = &driver[open..end];

    // Captured verbatim from `cargo build --release -p choir-cli`,
    // three leading spaces and all. The path is the workspace's, with
    // the home directory replaced -- what matters about it here is that
    // the extraction drops it.
    let cargo_output = "   Compiling choir-hash v0.0.1 (/home/<user>/choir/crates/choir-hash)\n\
         Compiling choir-node v0.0.1 (/home/<user>/choir/crates/choir-node)\n   \
         Compiling choir-cli v0.0.1 (/home/<user>/choir/crates/choir-cli)\n";
    let input = std::env::temp_dir().join(format!("choir-progress-{}", std::process::id()));
    std::fs::write(&input, cargo_output).expect("scratch input");
    let out = std::process::Command::new("awk")
        .arg(program)
        .arg(&input)
        .output()
        .expect("awk runs");
    let label = String::from_utf8_lossy(&out.stdout).trim().to_string();
    std::fs::remove_file(&input).ok();

    assert_eq!(
        label, "Compiling choir-cli",
        "the progress line does not name the crate cargo is on"
    );
    assert!(
        !label.contains('/'),
        "a filesystem path reached the progress line: {label}"
    );
}

/// Runs `validate_beta_acl.sh` over `acl` and returns its exit success
/// together with everything it said on stderr.
fn validate_beta_acl(tag: &str, acl: &str) -> (bool, String) {
    let path = std::env::temp_dir().join(format!(
        "choir-beta-acl-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::write(&path, acl).expect("acl fixture");
    let output = std::process::Command::new("sh")
        .arg(repo_root().join("scripts/flip/validate_beta_acl.sh"))
        .arg(&path)
        .output()
        .expect("run validate_beta_acl.sh");
    std::fs::remove_file(&path).ok();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// BETA-04. The private beta's ACL gives no beta user more than the
/// repositories it names.
///
/// The rule is narrower than "no wildcards", and the narrowness is the
/// whole of it. `choir_node::acl::Scope` has three forms and two are
/// wide: `*` is every repository, `@node` is the node itself -- the op
/// log, the attestation, ops naming no repository -- and `*` never
/// matches `@node`. The runbook *requires* the second, for the operator
/// credential holding the node-wide audit grant a recovery needs. So the
/// two are already distinguishable in the file, and only `*` has no
/// legitimate use in a beta: it reaches every repository including other
/// beta users', and names none of them, so nobody reading the file sees
/// who was exposed.
#[test]
fn a_private_beta_acl_grants_no_beta_user_every_repository() {
    let (ok, stderr) = validate_beta_acl(
        "named",
        "# the shape the runbook asks for\n\
         alice agents/one write\n\
         bob agents/two write\n\
         ops @node auditor\n",
    );
    assert!(ok, "a correctly narrow ACL was refused: {stderr}");

    let (ok, stderr) = validate_beta_acl(
        "wildcard",
        "alice agents/one write\n\
         mallory * write\n",
    );
    assert!(!ok, "an ACL granting every repository was accepted");
    assert!(
        stderr.contains("mallory") && stderr.contains("2"),
        "the refusal must name the line and the user: {stderr}"
    );

    // A deadline is a fourth column (D66), so a wildcard wearing one is
    // still in the scope column and still refused. An expiring grant to
    // every repository is a grant to every repository.
    let (ok, _) = validate_beta_acl("wildcard-until", "mallory * write until=99999999999\n");
    assert!(!ok, "a deadline does not narrow a wildcard");

    // Comments run to end of line, so a `*` in one is not a grant.
    let (ok, stderr) = validate_beta_acl(
        "commented",
        "alice agents/one write   # not * every repository\n",
    );
    assert!(ok, "a `*` inside a comment was read as a grant: {stderr}");

    // And the private-beta renderer must actually call it. The renderer
    // itself cannot run here -- it reads Linux file modes with `stat -c`
    // before it reaches any policy check -- so this is the same
    // source-level assertion `the_linux_installer_carries_the_same_policy_wiring`
    // makes, including the ordering that would leave `$HERE` unset.
    let renderer =
        std::fs::read_to_string(repo_root().join("scripts/flip/render_private_beta_service.sh"))
            .expect("private beta renderer source");
    let here = renderer.find("HERE=").expect("renderer defines HERE");
    let call = renderer
        .find("validate_beta_acl.sh")
        .expect("the private beta renderer does not validate its ACL");
    assert!(here < call, "renderer invokes the validator before HERE");
}

/// The private-beta renderer takes the accounts decision from the
/// manifest, not from its own opinion (D36).
///
/// The unit assertions above drive `render_node_service.sh` with the
/// slots filled in by hand, so they say what the unit renderer does with
/// a given argument and nothing about where that argument comes from.
/// The path from `accounts=enabled` in the manifest to `--accounts-file`
/// in a running node's ExecStart was, until this test, a claim in a
/// comment. That is the exact shape of the four cache holes this
/// repository spent a day closing: a statement nobody re-derived after
/// the thing it described moved.
#[test]
fn the_beta_renderer_reads_the_accounts_decision_from_the_manifest() {
    use std::os::unix::fs::PermissionsExt;

    let state = std::env::temp_dir().join(format!("choir-beta-manifest-{}", std::process::id()));
    std::fs::remove_dir_all(&state).ok();
    std::fs::create_dir_all(&state).expect("state dir");

    let private = |name: &str, body: &str| {
        let path = state.join(name);
        std::fs::write(&path, body).expect("state file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("0600");
    };
    private("auth", "alice:token\n");
    // Two fields exactly: the validator binds a reviewer to a key by
    // `$1 == name && NF == 2`.
    private(
        "keys",
        "alice/laptop AAAA\nbob/laptop BBBB\ncarol/laptop CCCC\n",
    );
    // Three operators, because this node has no owner for the protected
    // repository and so lands on the quorum basis: an approval needs two
    // distinct operators and the author's own is never drawn.
    private("reviewers", "alice/laptop\nbob/laptop\ncarol/laptop\n");
    private("protected-refs", "owner/repo.git:refs/heads/main\n");
    private("acl", "alice @node write\nalice owner/repo.git write\n");
    for name in [
        "newcomer-audit.jsonl",
        "newcomer-adjudications.jsonl",
        "review-adjudications.jsonl",
        "accounts.jsonl",
    ] {
        std::fs::write(state.join(name), "").expect("state file");
    }
    std::fs::write(state.join("repos.list"), "owner/repo.git\n").expect("repos.list");
    // Byte-for-byte this tree's manifest, because the renderer compares
    // them and refuses a copy that has drifted. That refusal is why this
    // test cannot also drive the disabled case: a manifest saying
    // something else is, correctly, not a manifest this tree will render.
    std::fs::copy(
        repo_root().join("scripts/flip/private-beta.manifest"),
        state.join("private-beta.manifest"),
    )
    .expect("manifest copy");

    let bin = state.join("choir-node");
    std::fs::write(&bin, "#!/bin/sh\n").expect("fake binary");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("executable");

    let output = std::process::Command::new("sh")
        .arg(repo_root().join("scripts/flip/render_private_beta_service.sh"))
        .args([
            "choir",
            bin.to_str().expect("path"),
            "/srv/choir/repos",
            "8417",
            state.to_str().expect("path"),
            "/var/log/choir/node.log",
        ])
        .output()
        .expect("render the private beta unit");
    assert!(
        output.status.success(),
        "renderer refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let unit = String::from_utf8(output.stdout).expect("UTF-8 unit");

    let manifest = std::fs::read_to_string(state.join("private-beta.manifest")).expect("manifest");
    let enabled = manifest.lines().any(|line| line == "accounts=enabled");
    assert!(
        enabled,
        "this test is about the manifest deciding; if the beta turns accounts \
         off again, flip the expectation here rather than deleting the test"
    );
    assert!(
        unit.contains(&format!(
            "--accounts-file {}",
            state.join("accounts.jsonl").display()
        )),
        "the manifest says accounts=enabled and the unit must carry the flag:\n{unit}"
    );
    let webauthn = manifest.lines().any(|line| line == "passkeys=enabled");
    assert_eq!(
        unit.contains("--passkeys"),
        webauthn,
        "the unit must carry WebAuthn exactly when the manifest asks for it:\n{unit}"
    );

    std::fs::remove_dir_all(&state).ok();
}
