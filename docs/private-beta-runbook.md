# Private single-node beta runbook

## Status and launch hold

Keep production DNS unpublished and the production firewall closed to all
non-operator source addresses until every go-live receipt below is attached to
the release record. Reach staging only over the operator VPN or an allowlist.

The beta application is the server-rendered UI in `choir-node`. Deploy no
separate frontend; an anonymous marketing site needs its own host and origin.

## Product boundary

Enabled:

- Authenticated dashboard, repository browser, commits, diffs, review pages.
- Git smart HTTP clone and push through the reverse proxy.
- Signed CLI operations, operator-issued credentials, repository ACLs, scoped
  operations, assigned reviewers, and protected-ref review gates.
- Authenticated health, readiness, and Prometheus-format metrics endpoints.
- Invite-only self-service accounts (D36) are how a beta user is given a
  credential; the manifest carries `accounts=enabled` and the renderer reads it.
- Sign-in on the node's own page (D71, D74): an unauthenticated browser gets a
  username and password form plus a passkey button, and the session is an
  opaque token held in memory. A first password sign-in by an account with no
  passkey lands on `/account`, where one is enrolled. Non-browser clients (git,
  `curl`, the CLI) meet `401` and a `WWW-Authenticate` challenge. Passkeys are
  their own switch (`--passkeys`); the renderer refuses `passkeys=enabled`
  without `accounts=enabled`. Browser writes remain off.

Out of scope: anonymous access, browser mutations, SSH, webhooks, the bridge,
a separate SPA, and a consumer login experience.

Launch objectives: 99.5 percent monthly availability, a one-hour RPO, a
four-hour RTO. The initial ceiling is 25 provisioned beta users and 10
concurrent interactive sessions. Per-user limits are 512 MiB per Git push,
eight workspaces, 120 API requests per minute, and 60 Git requests per minute.
Raise the user and concurrency caps only after a recorded staging load test.

## Host and network

1. Provision one supported Linux VM with at least twice the forecast beta data,
   encrypted persistent storage, inode and disk monitoring, and a dedicated
   system account such as `choir` with `/usr/sbin/nologin`. Keep repository
   root, policy state, logs, release archive and backup staging monitored.
2. Install the checksummed artifact under a versioned release directory and
   point `/opt/choir/current` at it. Never build on the production host.
3. Populate the service user's state directory outside this repository with
   `auth`, `keys`, `reviewers`, `protected-refs`, `repos.list`, `acl`, the
   three adjudication/audit JSONL files, and an exact copy of
   `scripts/flip/private-beta.manifest`. Directory mode 0700, its files 0600.
4. Render the system service with
   `scripts/flip/render_private_beta_service.sh`, which refuses missing ACL,
   scope, review gates, policy, or the beta manifest. Install the output as a
   system unit only after review. The generated node binds `127.0.0.1` and
   enables the read-only browser boundary, request and decision logging,
   limits, quotas, the readiness disk floor, and systemd hardening.
5. Render the TLS proxy with `scripts/flip/render_beta_nginx.sh <domain>
   <node-port> <tls-cert> <tls-key>` and review it with `nginx -t` before
   installation. It redirects HTTP to HTTPS, sets HSTS and security headers,
   preserves `Authorization`, uses a 1 MiB default body ceiling, and gives Git
   a separate 512 MiB streaming route with request buffering disabled.

   Pass `--behind-tls-proxy`, set by the beta renderer and derived by the
   installers from a public https route. Without it every absolute URL the node
   mints is written `http`, including the join link carrying the invite secret.

   The proxy handles no client address (D59), so pre-auth rate limiting is the
   node's node-wide ceiling alone. Diagnose incidents from the node's
   `--request-log`, which carries the authenticated user.

   > [!WARNING]
   > Do not add a per-address limit zone and do not restore the proxy access
   > log.

   Prove that after installation; `nginx -t` checks syntax only. Send one
   failing request over TLS and one failed TLS handshake, logged on a different
   path, then confirm no file under the proxy's log directory names an address.

Expose only 80 and 443 through the beta allowlist during pre-launch. Confirm
the node port is unreachable off loopback, with `ss -lntp` on the host and a
connection attempt from a separate machine.

> [!WARNING]
> Do not use the legacy direct-TLS installer for this beta.

## Access and policy

Create credentials on an operator workstation, never in the checkout, and
deliver each through the approved secret-sharing system. Give each beta user
only their named repositories in `acl`, with no wildcard grant, and keep an
operator credential with the node-wide audit grant and repository ownership a
recovery needs. Test a denied and an allowed repository from the public host.

Register every signed-operation key in `keys` and every protected ref in
`protected-refs`. The reviewer pool needs at least two eligible operators with
registered keys; `read` is enough for a reviewer (D55), so do not grant `write`
to draw somebody into a review. The service always enables required assignment,
review and scope, and a missing or malformed policy stops rendering or startup.

The browser must return 403 for `/account` and `/api/prepare`, and review pages
must show no mutation control. Perform mutations with the signed CLI.

## Backup and recovery

Run `pull_backup.sh` hourly to encrypted off-host storage. It publishes a new
copy only after transport checksum, append-only prefix, format version,
sequence, parent-chain, and recomputed-hash verification. For its contents and
the secrets you supply, see [Restoring from a backup](runbook-restore.md).

Store the node identity key separately in the approved encrypted secret store.
Retain at least 24 hourly, 14 daily, and 8 weekly generations. Run
`verify_backup.sh` daily and alert if the last successful backup is older than
90 minutes or any verification fails.

Once before launch and once per release cycle, restore onto a clean host with
`restore_from_backup.sh`, supplying the recovered node key and a newly issued
operator auth file. The rehearsal must finish within four hours and prove the
full log chain, policy files, ref attestation, GUI, clone, and a canary push.
A second operator must rehearse once from this runbook and the secret store.

## Delivery and rollback

CI uses Rust 1.97.1 and must pass formatting, workspace tests, Clippy and
rustdoc with warnings denied, the Phase-0 spike, generated-file freshness, and
RustSec audit. `build_private_beta_artifact.sh` builds with `--locked`, stamps
the commit, and emits SHA-256 checksums, CycloneDX SBOMs and a versioned
artifact.

`private-beta-release.yml` is manual: it repeats the fail-closed gate, deploys
the same artifact to staging, and runs the smoke tests. Production promotion is
a separate protected environment approval; configure its required reviewers
before enabling the workflow.

`smoke_private_beta.sh` issues receipt 3:

```bash
smoke_private_beta.sh <https-base> <auth-file> <owner/repo.git> \
  [--push-canary] [--denied <owner/repo.git>]
```

It probes anonymously first: `/healthz`, `/readyz`, `/metrics`, `/api/schema`,
`/api/view`, the repository page and a git fetch must all answer 401. It then
repeats the reachable ones with credentials, sends one byte over the API body
ceiling expecting 413, and clones. Pass `--denied` a repository the credential
must not reach to cover the denied half of the ACL; the clone covers the
allowed half. `smoke_script::a_node_serving_anonymously_fails_the_smoke_script`
in `crates/choir-node/tests/it/` runs it against a real node over real TLS.

Read five of receipt 3's items off the node rather than the public hostname:
scope policy, review policy, quotas, request logging, and oversized git
requests, whose ceiling is 512 MiB per request. The test suite covers all five
against a local node; the hostname adds only that the proxy does not alter them.

`deploy_private_beta.sh` installs into a new release directory, archives the
current symlink target, switches atomically, and restores the old target if the
restart fails. `rollback_private_beta.sh` is the one-command rollback. Rehearse
both in staging, recording the node's active commit before and after rollback.

## Monitoring and deliberate alert tests

Probe `/healthz`, `/readyz`, and `/metrics` with an operator credential.
Readiness checks the live log format and chain, sequencer durability, a real
write-and-sync storage probe, free disk, and repository/ref agreement.

> [!WARNING]
> Never make these Choir routes anonymous.

Nine alerts are critical; [Observability](operating/observability.md) names
each and says where to source it. Revise the unmeasured thresholds in
`scripts/flip/choir-alerts.rules.yml` against the first fortnight of traffic.

Before launch, deliberately trigger each alert in staging: an invalid
credential for 401, a staging-only low rate limit for 429, the impossible
readiness disk floor for readiness, a stopped backup timer for stale backup,
and the supervisor test fixture for a durability exit.

> [!CAUTION]
> Do not fill a production filesystem to test disk alerts.

## The BETA-0n tests

Receipt 1 below names five focused tests. All five run in the ordinary suite.

| Test | Claim | Where |
|---|---|---|
| BETA-01 | A review gate that cannot enforce anything is refused at startup, all three ways to configure one | `crates/choir-node/tests/it/review_gate_config.rs` |
| BETA-02 | Under `--read-only-browser` no page renders a mutation control, the review page included, and `/api/prepare` is refused. The flag withholds authorship and not credentials (D73): signing in, enrolling a passkey and answering an access request still work | `crates/choir-node/tests/it/browse.rs`, `a_read_only_browser_renders_no_mutation_control_anywhere`; `crates/choir-node/tests/it/passkeys.rs`, `the_enrolment_page_works_under_a_read_only_browser` |
| BETA-03 | `/healthz`, `/readyz` and `/metrics` each refuse an anonymous request | `crates/choir-node/tests/limits.rs`, `authenticated_health_readiness_and_metrics_report_independent_checks` |
| BETA-04 | A private-beta ACL grants no beta user every repository | `scripts/flip/validate_beta_acl.sh`, tested in `crates/choir-cli/tests/it/install_policy.rs` |
| BETA-05 | The manifest's ceilings are the unit's flags are the numbers the daemon parses | `crates/choir-node/tests/it/beta_limits.rs` |
| BETA-06 | Each readiness sub-check fails on its own, and the others stay true | `crates/choir-node/tests/limits.rs`, the three `_alone_makes_the_node_unready` tests |

BETA-04 refuses `*` alone; `@node` stays legitimate for the operator credential
holding the node-wide audit grant a recovery needs, and `*` never matches
`@node`. `render_private_beta_service.sh` calls `validate_beta_acl.sh` first.

## Go-live receipts

Production stays network-closed until the release record contains:

1. The BETA-0n tests above and the full CI gate green for the artifact commit.
2. Host-local and remote evidence that the node listens only on loopback and
   all application access crosses the hardened TLS proxy.
3. Public-hostname smoke receipts covering authentication, allowed and denied
   ACL access, scope and review policy, quotas, request logging, oversized API
   and Git requests, GUI, clone, and a canary push.
4. An off-host backup and clean-host restore meeting the one-hour RPO and
   four-hour RTO, with policy equivalence and the node identity recovered from
   the approved secret store.
5. CI rejection, staging-first promotion, every critical alert, atomic
   deployment, and one-command rollback each exercised.

## Inviting somebody (D57)

Mint an invite as the operator; you freeze the username and grants, and the
recipient changes neither:

```sh
curl -u <operator> -X POST \
  -d '{"user":"<their-name>","grants":["<owner>/<repo>.git write"]}' \
  https://<host>/api/accounts/invite
```

Send the response's `join_url` and nothing else. It opens a page showing what
the recipient is accepting, and one button that creates the account and shows a
password once.

| Property | What it means for handling the link |
|---|---|
| Preview-safe | Chat clients fetch the link to build a card; only the button spends it. |
| Single use, 24 hours by default | Pass `expires_in_secs` to shorten it. A link that has sat in a channel for a day is already dead. |
| Bearer credential | Anyone who can read the channel can redeem it, for the name and the grants you chose. `POST /api/accounts/revoke` removes the account and any outstanding invite together. Treat the channel as the boundary. |
| Never `@node` | The store refuses to issue node scope, so an invite cannot mint an auditor or a rate-limit exemption. Node-wide authority stays in the ACL file, edited by hand (D36). |

The request log records a redemption attempt under the invite id as `user`,
attributing it without naming the account it would create or the secret.
