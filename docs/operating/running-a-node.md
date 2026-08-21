# Running a node

The daemon is one binary serving two protocols on one port: git smart-HTTP
for `clone`, `fetch` and `push`, and the platform API for signed operations.
This page is the operator's side of it — how to start it, what each policy
file means, and the one warning that matters before a second person gets a
credential.

Everything a *user* of a running node needs is in `docs/using/cli.md`. The
policy behind each flag has its own page: `docs/operating/authorization.md`,
`docs/operating/limits.md`, `docs/operating/webhooks.md`,
`docs/operating/observability.md` and `docs/operating/transports.md`.

The daemon serves **git smart-HTTP** and the **platform API** on one port (default **8417**). Repos must be created with `--create` (or the installer) so the `pre-receive` hook is installed; a bare repo made any other way is **not** sequenced. With no arguments, the binary uses `./repos` and port 8417; configured invocations must supply both `<repo-root>` and `<port>` before any flags.

## Option A: macOS dogfood (supervised)

```bash
./choirctl install-cli          # put `choir` on your PATH as a real binary
./choirctl install              # build, mint ~/.choir secrets, load launchd
./choirctl status
./choirctl url                  # clone/push URL with credentials
./choirctl logs
# ./choirctl stop | uninstall   # stop keeps data under ~/.choir
```

Override port with `CHOIR_PORT`. Full flip procedure: `scripts/flip/RUNBOOK.md`.

## Option A2: private beta behind a TLS proxy

For the private beta, keep `choir-node` bound to `127.0.0.1` and terminate TLS at a hardened reverse proxy. Do not use the legacy direct-TLS installer. The beta service renderer requires an ACL, protected-ref review policy, scoped operations, operator-issued auth, and the read-only browser mode. The proxy renderer preserves authentication, streams Git separately, and applies route-specific limits.

See [`docs/private-beta-runbook.md`](../private-beta-runbook.md) for the network hold, service and proxy renderers, backups, CI packaging, staging promotion, monitoring, rollback, and go-live receipts. Every Choir route remains authenticated. A separate anonymous marketing page must use another host and origin.

## Option B: any Unix (foreground)

```bash
mkdir -p /tmp/choir-repos ~/.choir
# user:token per line, mode 0600
printf 'choir:%s\n' "$(openssl rand -hex 32)" > ~/.choir/auth && chmod 600 ~/.choir/auth
: > ~/.choir/keys && chmod 600 ~/.choir/keys
cargo run -p choir-cli -- key ~/.choir/agent.key myop/agent >> ~/.choir/keys
printf '# reviewer channels, one per line\n' > ~/.choir/reviewers && chmod 600 ~/.choir/reviewers

cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --create owner/demo.git \
  --auth-file ~/.choir/auth \
  --keys-file ~/.choir/keys \
  --reviewers-file ~/.choir/reviewers
```

Useful flags: `--bind`, `--tls-cert` / `--tls-key`, `--acl-file <file>` (required before a second credential), `--request-log <file>` and `--rate-limit-api` / `--rate-limit-git` (also required before a second credential), `--quota-push-bytes` / `--quota-workspaces`, `--api-body-limit`, `--batch-limit`, `--ready-min-free-bytes`, `--read-only-browser`, `--journal <file>`, `--require-assignment`, `--protected-refs <file>`, `--require-review`, `--reviewer-conflict-graph <file>` with `--reviewer-conflict-distance <hops>`, `--review-retention <count>`, and `--review-lapse-after-secs <seconds>`. Authenticated operations endpoints are `/healthz`, `/readyz`, and `/metrics`. Flag reference: module docs at the top of `crates/choir-node/src/main.rs`, or `agents.md`.

**File formats (all mode 0600)**

| File | Format |
|:--|:--|
| `--auth-file` | `user:token` per line; authentication only, pair with `--acl-file` |
| `--acl-file` | `<user> <repo\|*\|@node> <level>` per line; levels `read` < `propose` < `write` < `own`, and `auditor` on `@node` |
| `--keys-file` | `<64-hex>` or `<channel> <64-hex>` (bound key) |
| `--reviewers-file` | channel name per line; re-read on each draw |
| `--protected-refs` | `owner/repo.git:refs/heads/main` (trailing `*` ok) |
| `--reviewer-conflict-graph` | undirected `operator operator` edges; pair with an explicit maximum hop distance |

Hot-reload: trusted keys, channel bindings, push-certificate signers, reviewers, and ACL grants take effect on the next request.

> [!WARNING]
> **Without `--acl-file`, every credential reaches every repository.** The auth file authenticates and nothing else; the node prints a line saying so at startup. A second `user:token` line can then clone every repo, push to any unprotected ref, and provision workspaces anywhere. Protected refs and the review requirement still hold, so it cannot land on a gated `main` unreviewed. **Do not issue a second credential without an ACL.**
