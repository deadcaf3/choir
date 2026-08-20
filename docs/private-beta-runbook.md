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
5. Render the TLS proxy with `scripts/flip/render_beta_nginx.sh`. Review it with
   `nginx -t` before installation. It redirects HTTP to HTTPS, sets HSTS and
   security headers, applies pre-auth request and connection limits, preserves
   `Authorization`, uses a 1 MiB default body ceiling, and gives Git a separate
   512 MiB streaming route with request buffering disabled.

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

## Go-live receipts

Production remains network-closed until the release record contains all of the
following:

1. BETA-01 through BETA-05 focused tests and the full CI gate are green for the
   exact artifact commit.
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
