# Private single-node beta runbook

## Status and launch hold

This runbook defines the first private beta. It is not a declaration that a
host is ready. Keep production DNS unpublished and the production firewall
closed to all non-operator source addresses until every go-live receipt in the
last section is attached to the release record. Staging should be reachable
only through the operator VPN or an explicit source allowlist.

The beta application is the server-rendered UI in `choir-node`. Do not build or
deploy a separate frontend. Any anonymous marketing site belongs on a separate
host and origin.

## Product boundary

Enabled:

- Authenticated node dashboard, repository browser, commits, diffs, and review
  pages.
- Git smart HTTP clone and push through the reverse proxy.
- Signed CLI operations, operator-issued credentials, repository ACLs, scoped
  operations, assigned reviewers, and protected-ref review gates.
- Authenticated health, readiness, and Prometheus-format metrics endpoints.

Unsupported for this beta:

- Anonymous access of any kind.
- Browser mutations, passkeys, self-service accounts, SSH, webhooks, and the
  bridge.
- A consumer login experience or a separate SPA.

The launch objectives are 99.5 percent monthly availability, a one-hour RPO,
and a four-hour RTO. The initial administrative ceiling is 25 provisioned beta
users and 10 concurrent interactive sessions. Per-user limits are 512 MiB per
Git push, eight workspaces, 120 API requests per minute, and 60 Git requests per
minute. Treat the user and concurrency figures as an initial operating cap,
not a measured capacity claim. Raise them only after a recorded staging load
test.

## Host and network

1. Provision one supported Linux VM with at least twice the forecast beta data,
   encrypted persistent storage, inode and disk monitoring, and a dedicated
   system account such as `choir` with `/usr/sbin/nologin`. Keep the repository
   root, policy state, logs, release archive, and backup staging paths on
   explicitly monitored filesystems.
2. Install the checksummed artifact under a versioned release directory. Point
   `/opt/choir/current` at that directory. Never build on the production host.
3. Populate the service user's state directory outside this repository. It must
   contain `auth`, `keys`, `reviewers`, `protected-refs`, `repos.list`, `acl`,
   the three adjudication/audit JSONL files, and an exact copy of
   `scripts/flip/private-beta.manifest`. Set the directory to mode 0700 and all
   credential and policy files to mode 0600.
4. Render the system service with
   `scripts/flip/render_private_beta_service.sh`. This refuses missing ACL,
   scope, review gates, policy, or the beta manifest. Install the output as a
   system unit only after review. The generated node binds to `127.0.0.1`,
   enables the read-only browser boundary, request and decision logging,
   limits, quotas, readiness disk floor, and systemd hardening.
5. Render the TLS proxy with `scripts/flip/render_beta_nginx.sh <domain>
   <node-port> <tls-cert> <tls-key>`. Review it with `nginx -t` before
   installation. It redirects HTTP to HTTPS, sets HSTS and security headers,
   preserves `Authorization`, uses a 1 MiB default body ceiling, and gives Git
   a separate 512 MiB streaming route with request buffering disabled.

   It handles no client address (D59): no access log, no error log, no
   per-address limit zone, and `X-Forwarded-For` cleared rather than appended.
   Pre-auth rate limiting is therefore the node's own node-wide ceiling alone.
   Do not add a per-address zone to restore per-client fairness, and do not
   restore the access log to diagnose an incident; the node's `--request-log`
   is the record to read, and it carries the authenticated user rather than an
   address.

   Prove that after installation rather than assuming it. `nginx -t` checks
   syntax and cannot show where a request error would be written. Make one
   deliberately failing request over TLS, then confirm no file under the proxy's
   log directory gained a line naming an address. Repeat it once for a failed
   TLS handshake, which is logged on a different path from a failed request.

The host firewall must expose only 80 and 443 through the beta allowlist during
pre-launch. The node port must not be reachable on any non-loopback interface.
Verify this both with `ss -lntp` on the host and a connection attempt from a
separate machine. Do not use the legacy direct-TLS installer for this beta.

## Access and policy

Create credentials on an operator workstation, never in the checkout. Deliver
each credential through the approved secret-sharing system. Give each beta user
only their named repositories in `acl`; there must be no wildcard beta-user
grant. Keep a separate operator credential with the node-wide audit grant and
repository ownership needed for recovery. Test a denied repository as well as
an allowed one from the public hostname.

Register every signed-operation key in `keys`. The reviewer pool must contain
at least two eligible operators with registered keys. A reviewer needs no more
than `read` on the repository under review (D55): verdicts, comments and
viewing receipts authorize at that level, so do not grant `write` merely to
draw somebody into a review. Name every protected ref
in `protected-refs`. The private-beta service always enables required
assignment, required review, and required scope. A missing or malformed policy
must stop rendering or startup rather than relax a gate.

The browser must return 403 for `/account` and `/api/prepare`. Review pages must
show no browser mutation controls. Perform mutations with the signed CLI.

## Backup and recovery

Run `pull_backup.sh` hourly to encrypted off-host storage. It publishes a new
copy only after transport checksum, append-only prefix, format version,
sequence, parent-chain, and recomputed-hash verification. It also carries the
node fingerprint, ref attestation, complete Git bundles, ACL, repository list,
review policy and adjudication files, and the private-beta configuration
manifest. It deliberately excludes authentication tokens, private keys, and
TLS keys.

Store the node identity key separately in the approved encrypted secret store.
Retain at least 24 hourly, 14 daily, and 8 weekly generations. Run
`verify_backup.sh` daily and alert if the last successful backup is older than
90 minutes or any verification fails.

Once before launch and once per release cycle, restore onto a clean host with
`restore_from_backup.sh`. Supply the recovered node key and a newly issued
operator auth file from the secret store. The rehearsal must finish within four
hours and prove the full log chain, policy files, ref attestation, GUI, clone,
and a canary push. A second operator must perform at least one rehearsal using
only this runbook and the secret store.

## Delivery and rollback

CI uses Rust 1.97.1 and must pass formatting, workspace tests, Clippy and
rustdoc with warnings denied, the Phase-0 spike, generated-file freshness, and
RustSec audit. `build_private_beta_artifact.sh` builds with `--locked`, stamps
the commit, emits SHA-256 checksums and CycloneDX SBOMs, and packages a versioned
artifact.

`private-beta-release.yml` is manual. It repeats the fail-closed gate, deploys
the same artifact to staging, and runs authenticated GUI, API-limit, clone, and
canary-push smoke tests. Production promotion is a separate protected
environment approval. Configure required reviewers on that environment before
the workflow is enabled.

`deploy_private_beta.sh` installs into a new release directory, archives the
current symlink target, switches atomically, and restores the old target if the
service restart fails. `rollback_private_beta.sh` is the one-command rollback.
Rehearse both scripts in staging and record the active commit reported by the
node before and after rollback.

## Monitoring and deliberate alert tests

Probe `/healthz`, `/readyz`, and `/metrics` with an operator credential. Never
make these Choir routes anonymous. Readiness checks the live log format and
chain, sequencer durability, a real write-and-sync storage probe, free disk, and
repository/ref agreement.

Alert on process restart loops, readiness failure, durability errors, latency
gate breaches, 401/429/5xx spikes, disk and inode exhaustion, certificate expiry
inside 21 days, and backup age beyond 90 minutes. Before launch, deliberately
trigger each alert in staging. Use an invalid credential for 401, a staging-only
low rate limit for 429, the impossible readiness disk floor for readiness, a
stopped backup timer for stale backup, and the supervisor test fixture for a
durability exit. Do not fill a production filesystem to test disk alerts.

## The BETA-0n tests

Receipt 1 below names five focused tests. They had names and nothing else
for as long as the receipt existed, which is a receipt that cannot be
collected; four of them now exist and run in the ordinary suite.

| Test | Claim | Where |
|---|---|---|
| BETA-01 | A review gate that cannot enforce anything is refused at startup, all three ways to configure one | `crates/choir-node/tests/it/review_gate_config.rs` |
| BETA-02 | Under `--read-only-browser` no page renders a mutation control, the review page included | `crates/choir-node/tests/it/browse.rs`, `a_read_only_browser_renders_no_mutation_control_anywhere` |
| BETA-03 | `/healthz`, `/readyz` and `/metrics` each refuse an anonymous request | `crates/choir-node/tests/limits.rs`, `authenticated_health_readiness_and_metrics_report_independent_checks` |
| BETA-04 | The shipped ACL grants no beta user more than their named repositories | **not written; see below** |
| BETA-05 | The manifest's ceilings are the unit's flags are the numbers the daemon parses | `crates/choir-node/tests/it/beta_limits.rs` |

BETA-04 is open, and deliberately so rather than by omission. The rule it
would enforce is narrower than "no wildcards": the ACL section above
requires a wildcard, for the operator credential that holds the node-wide
audit grant and the ownership a recovery needs. So a check has to
distinguish an operator row from a beta-user row, and the ACL file format
carries no such distinction -- a row is `<user> <repo|*|@node> <level>`
and nothing more. Deciding what marks an operator is a design question,
not a test, and it is not answered by writing the test first. Until it is
answered, receipt 1 has four tests and one named gap, which is a truer
receipt than five where one enforces a rule nobody has defined.

## Go-live receipts

Production remains network-closed until the release record contains all of the
following:

1. The BETA-0n focused tests above and the full CI gate are green for the exact
   artifact commit. BETA-04 is unwritten, so this receipt cannot be collected in
   full until its design question is answered or the receipt is deliberately
   narrowed to the four that exist.
2. Host-local and remote evidence proves the node listens only on loopback and
   all application access crosses the hardened TLS proxy.
3. Public-hostname smoke receipts cover authentication, allowed and denied ACL
   access, scope and review policy, quotas, request logging, oversized API and
   Git requests, GUI, clone, and a canary push.
4. An off-host backup and clean-host restore meet the one-hour RPO and four-hour
   RTO, with policy equivalence and the node identity recovered from the
   approved secret store.
5. CI rejection, staging-first promotion, every critical alert, atomic
   deployment, and one-command rollback have each been exercised.

Only after those receipts are reviewed may the firewall allow invited beta
users and production DNS be published. Authentication does not replace this
launch hold.

## Inviting somebody (D57)

Mint an invite as the operator. The username and the grants are frozen
here, by you, and nothing the recipient does can change either:

```sh
curl -u <operator> -X POST \
  -d '{"user":"<their-name>","grants":["<owner>/<repo>.git write"]}' \
  https://<host>/api/accounts/invite
```

The response carries `join_url`. **That is the whole thing you send** —
paste it into the chat and nothing else. It opens a page that shows them
what they are accepting, and one button that creates the account and
shows a password once.

Four properties worth knowing, because they change how you handle a link:

- **A preview is harmless.** Chat clients fetch the link to build a card;
  fetching never spends it. Only the button does.
- **Single use, and 24 hours by default.** Pass `expires_in_secs` to
  shorten it. A link that has sat in a channel for a day is already dead.
- **It is a bearer credential.** Anyone who can read the channel can
  redeem it. That is bounded — they get the name and the grants you
  chose, and `POST /api/accounts/revoke` removes the account and any
  outstanding invite together — but treat the channel as the boundary.
- **Never `@node`.** The store refuses to issue node scope, so an invite
  cannot mint an auditor or a rate-limit exemption. Node-wide authority
  stays in the ACL file where you edit it by hand (D36).

The invite id appears in the request log as the `user` for a redemption
attempt, which is deliberate: it attributes the attempt without naming
the account it would create. The secret never appears, because it travels
in a query string and the log records only paths.
