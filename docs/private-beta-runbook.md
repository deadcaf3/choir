# Private single-node beta runbook

## Status and launch hold

Keep production DNS unpublished and the production firewall closed to
non-operator addresses until every go-live receipt below is attached to the
release record. Reach staging over the operator VPN or an allowlist.

The beta application is the server-rendered UI in `choir-node`. No separate
frontend.

## Product boundary

Enabled:

- Authenticated dashboard, repository browser, commits, diffs, review pages.
- Git smart HTTP clone and push through the reverse proxy.
- Signed CLI operations, operator-issued credentials, repository ACLs, scoped
  operations, assigned reviewers, protected-ref review gates.
- Authenticated health, readiness and Prometheus metrics endpoints.
- Invite-only self-service accounts (D36); the manifest carries
  `accounts=enabled`.
- Sign-in on the node's own page (D71, D74): username and password plus a
  passkey button; the session is an opaque in-memory token. A first password
  sign-in with no passkey lands on `/account`. Non-browser clients meet `401`
  with `WWW-Authenticate`. Passkeys are `--passkeys`; the renderer refuses
  `passkeys=enabled` without `accounts=enabled`. Browser writes stay off.

Out of scope: anonymous access, browser mutations, SSH, webhooks, the bridge,
a separate SPA, a consumer login.

Objectives: 99.5 percent monthly availability, one-hour RPO, four-hour RTO.
Ceiling: 25 beta users, 10 concurrent sessions. Per user: 512 MiB per push,
eight workspaces, 120 API and 60 Git requests per minute. Raise caps only
after a recorded staging load test.

## Host and network

1. One supported Linux VM with twice the forecast data, encrypted storage,
   inode and disk monitoring, and a system account such as `choir` with
   `/usr/sbin/nologin`.
2. Install the checksummed artifact under a versioned release directory and
   point `/opt/choir/current` at it. Never build on the production host.
3. Populate the service user's state directory, outside this repository,
   with `auth`, `keys`, `reviewers`, `protected-refs`, `repos.list`, `acl`,
   the three adjudication/audit JSONL files, and an exact copy of
   `scripts/flip/private-beta.manifest`. Directory 0700, files 0600.
4. Render the service with `scripts/flip/render_private_beta_service.sh`,
   which refuses missing ACL, scope, review gates, policy or manifest.
   Install as a system unit after review. The node binds `127.0.0.1` with
   the read-only browser, request and decision logging, limits, quotas, the
   readiness disk floor and systemd hardening.
5. Render the TLS proxy with `scripts/flip/render_beta_nginx.sh <domain>
   <node-port> <tls-cert> <tls-key>`; check with `nginx -t` before install.
   It redirects HTTP to HTTPS, sets HSTS and security headers, preserves
   `Authorization`, caps bodies at 1 MiB, and gives Git a 512 MiB streaming
   route with buffering off.

   Pass `--behind-tls-proxy`. Without it every absolute URL, including the
   join link, is written `http`.

   The proxy handles no client address (D59); pre-auth limiting is the
   node's node-wide ceiling. Diagnose from the node's `--request-log`.

   > [!WARNING]
   > Do not add a per-address limit zone and do not restore the proxy access
   > log.

   After install, send one failing request over TLS and one failed TLS
   handshake, then confirm no file under the proxy's log directory names an
   address.

Expose only 80 and 443 through the allowlist. Confirm the node port is
unreachable off loopback with `ss -lntp` on the host and a connection attempt
from another machine.

> [!WARNING]
> Do not use the legacy direct-TLS installer for this beta.

## Access and policy

Create credentials on an operator workstation and deliver through the
approved secret-sharing system. Grant each beta user only named repositories
in `acl`, no wildcard. Keep an operator credential with the node-wide audit
grant and the ownership a recovery needs. Test a denied and an allowed
repository from the public host.

Register every signing key in `keys` and every protected ref in
`protected-refs`. The reviewer pool needs two eligible operators with
registered keys; `read` is enough to review (D55). The service always
enables required assignment, review and scope; a missing or malformed policy
stops startup.

The browser must return 403 for `/account` and `/api/prepare`, and review
pages must show no mutation control.

## Backup and recovery

Run `pull_backup.sh` hourly to encrypted off-host storage. It publishes only
after checksum, append-only prefix, format version, sequence, parent-chain
and recomputed-hash verification. Contents and the secrets you supply:
[Restoring from a backup](runbook-restore.md).

Store the node identity key separately in the approved secret store. Retain
24 hourly, 14 daily and 8 weekly generations. Run `verify_backup.sh` daily;
alert if the last backup is older than 90 minutes or verification fails.

Before launch and once per release, restore onto a clean host with
`restore_from_backup.sh`, supplying the recovered node key and a new
operator auth file. Finish within four hours and prove the log chain,
policy files, ref attestation, GUI, clone and a canary push. A second
operator must rehearse once from this runbook.

## Delivery and rollback

CI uses Rust 1.97.1: formatting, workspace tests, Clippy and rustdoc with
warnings denied, the Phase-0 spike, generated-file freshness, RustSec audit.
`build_private_beta_artifact.sh` builds with `--locked`, stamps the commit,
and emits SHA-256 checksums, CycloneDX SBOMs and a versioned artifact.

`private-beta-release.yml` is manual: it repeats the gate, deploys to
staging, and runs the smoke tests. Production promotion is a separate
protected environment approval.

`smoke_private_beta.sh` issues receipt 3:

```bash
smoke_private_beta.sh <https-base> <auth-file> <owner/repo.git> \
  [--push-canary] [--denied <owner/repo.git>]
```

Anonymously, `/healthz`, `/readyz`, `/metrics`, `/api/schema`, `/api/view`,
the repository page and a git fetch must answer 401. With credentials it
repeats the reachable ones, sends one byte over the API body ceiling
expecting 413, and clones. `--denied` covers the denied half of the ACL.
`smoke_script::a_node_serving_anonymously_fails_the_smoke_script` runs it
against a real node over TLS.

Read five receipt-3 items off the node: scope policy, review policy, quotas,
request logging, and oversized git requests (512 MiB ceiling). The suite
covers all five locally; the hostname adds only that the proxy does not
alter them.

`deploy_private_beta.sh` installs into a new release directory, switches the
symlink atomically, and restores the old target if the restart fails.
`rollback_private_beta.sh` is the one-command rollback. Rehearse both in
staging, recording the active commit before and after.

## Monitoring and deliberate alert tests

Probe `/healthz`, `/readyz` and `/metrics` with an operator credential.
Readiness checks log format and chain, sequencer durability, a real storage
write-and-sync, free disk, and repository/ref agreement.

> [!WARNING]
> Never make these Choir routes anonymous.

Nine alerts are critical; [Observability](operating/observability.md) names
each. Revise the unmeasured thresholds in
`scripts/flip/choir-alerts.rules.yml` after the first fortnight.

Trigger each alert in staging before launch: an invalid credential for 401,
a staging-only low rate limit for 429, the impossible readiness disk floor,
a stopped backup timer, and the supervisor fixture for a durability exit.

> [!CAUTION]
> Do not fill a production filesystem to test disk alerts.

## The BETA-0n tests

All run in the ordinary suite.

| Test | Claim | Where |
|---|---|---|
| BETA-01 | A review gate that cannot enforce anything is refused at startup, all three ways | `crates/choir-node/tests/it/review_gate_config.rs` |
| BETA-02 | Under `--read-only-browser` no page renders a mutation control and `/api/prepare` is refused; sign-in, passkey enrolment and access requests still work (D73) | `crates/choir-node/tests/it/browse.rs`, `a_read_only_browser_renders_no_mutation_control_anywhere`; `crates/choir-node/tests/it/passkeys.rs`, `the_enrolment_page_works_under_a_read_only_browser` |
| BETA-03 | `/healthz`, `/readyz` and `/metrics` refuse an anonymous request | `crates/choir-node/tests/limits.rs`, `authenticated_health_readiness_and_metrics_report_independent_checks` |
| BETA-04 | A private-beta ACL grants no beta user every repository | `scripts/flip/validate_beta_acl.sh`, tested in `crates/choir-cli/tests/it/install_policy.rs` |
| BETA-05 | The manifest's ceilings are the unit's flags are the numbers the daemon parses | `crates/choir-node/tests/it/beta_limits.rs` |
| BETA-06 | Each readiness sub-check fails on its own | `crates/choir-node/tests/limits.rs`, the three `_alone_makes_the_node_unready` tests |

BETA-04 refuses `*` alone; `@node` stays legitimate for the operator
credential, and `*` never matches `@node`.
`render_private_beta_service.sh` calls `validate_beta_acl.sh` first.

## Go-live receipts

Production stays network-closed until the release record contains:

1. The BETA-0n tests and the full CI gate green for the artifact commit.
2. Host-local and remote evidence that the node listens only on loopback
   and all access crosses the TLS proxy.
3. Public-hostname smoke receipts: authentication, allowed and denied ACL
   access, scope and review policy, quotas, request logging, oversized API
   and Git requests, GUI, clone, canary push.
4. An off-host backup and clean-host restore within the RPO and RTO, with
   policy equivalence and the node identity recovered from the secret store.
5. CI rejection, staging-first promotion, every critical alert, atomic
   deployment and one-command rollback each exercised.

## Inviting somebody (D57)

Mint an invite as the operator; the username and grants are frozen:

```sh
curl -u <operator> -X POST \
  -d '{"user":"<their-name>","grants":["<owner>/<repo>.git write"]}' \
  https://<host>/api/accounts/invite
```

Send the response's `join_url` and nothing else.

| Property | Handling |
|---|---|
| Preview-safe | Chat clients may fetch the link; only the button spends it. |
| Single use, 24 hours by default | `expires_in_secs` shortens it. |
| Bearer credential | Anyone reading the channel can redeem it. `POST /api/accounts/revoke` removes the account and any outstanding invite. |
| Never `@node` | The store refuses node scope; node-wide authority stays in the hand-edited ACL (D36). |

The request log records a redemption attempt under the invite id as `user`.
