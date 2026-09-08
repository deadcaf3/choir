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

The daemon serves **git smart-HTTP** and the **platform API** on one port (default **8417**). Every repository is served with a `pre-receive` hook or it is not sequenced, so make them with `choir repo create` against a running node, or `--create` at startup; a bare repository that arrives any other way — restored from a bundle, copied in — is adopted and hooked at the next start. With no arguments, the binary uses `./repos` and port 8417; configured invocations must supply both `<repo-root>` and `<port>` before any flags.

## Getting the binaries

One command, no Rust toolchain and no OpenSSL headers. macOS and Linux,
x86-64 and arm64:

```bash
curl -fsSL https://<release-host>/choir-node-installer.sh | sh   # choir-node, choir-ssh
curl -fsSL https://<release-host>/choir-cli-installer.sh | sh    # choir, choir-mcp
```

Take both: every command on this page is `choir`, and `choir node serve`
execs the daemon the first line installed. Both land in `$CARGO_HOME/bin`
(`~/.cargo/bin` by default), and each release publishes a `SHA256` beside
every archive with a `sha256.sum` over the set. `<release-host>` is a
placeholder until the first release is cut.

`cargo binstall choir-node choir-cli` fetches the same archives.

**From source** is the audited path and still supported: what you run is
what you compiled, from a tree you can read. It needs the toolchain and
the headers the [README](../../README.md#requirements) lists.

```bash
cargo build --release -p choir-node -p choir-cli
export PATH="$PWD/target/release:$PATH"
```

A prebuilt daemon reports the commit it was built from the same way a
source-built one does — `choir node status` and `choir --version` read a
stamp the release workflow sets, not a `git rev-parse` in whatever
directory you are standing in.

## Option A: your own machine, supervised

Four commands, and none of them needs a path. `choir init` writes the
layout under `~/.choir`; every command after it derives what it needs
from that layout, so the flags below are the ones you choose, not the
ones you have to remember.

```bash
choir init                      # mint ~/.choir: credential, key, trusted keys, config
choir node install              # hand it to launchd (macOS) or systemd (Linux)
choir node status               # health, the commit serving, sequencer position
choir repo create me/thing.git  # a repository on the running node
choir repo url me/thing.git     # the clone URL, and the git config to go with it
choir node logs                 # the tail of the daemon log
# choir node restart | stop | uninstall   -- uninstall keeps ~/.choir
```

`choir node install` writes a unit that runs `choir node serve`, so a
daemon flag changing later never means re-rendering it. Pass daemon
flags after `--`, and they are recorded in the unit:

```bash
choir node install -- --acl-file ~/.choir/acl --rate-limit-api 60
```

Override the port with `choir init --port <n>`; every command afterwards
reads it back out of `.choir/config`. Full flip procedure:
`scripts/flip/RUNBOOK.md`.

## Option A2: private beta behind a TLS proxy

For the private beta, keep `choir-node` bound to `127.0.0.1` and terminate TLS at a hardened reverse proxy. Do not use the legacy direct-TLS installer. The beta service renderer requires an ACL, protected-ref review policy, scoped operations, operator-issued auth, and the read-only browser mode. The proxy renderer preserves authentication, streams Git separately, and applies route-specific limits.

See [`docs/private-beta-runbook.md`](../private-beta-runbook.md) for the network hold, service and proxy renderers, backups, CI packaging, staging promotion, monitoring, rollback, and go-live receipts. Every Choir route remains authenticated. A separate anonymous marketing page must use another host and origin.

## Option B: any Unix, in the foreground

```bash
choir init                                  # the same layout, without a service manager
choir node serve                            # runs here, in this terminal
choir node serve -- --reviewers-file ~/.choir/reviewers   # any daemon flag, after `--`
```

`choir node serve` derives the repository root, the port, the credential
and the trusted-key file from what `choir init` wrote, then **execs** the
daemon — so the process you signal, the process the supervisor watches
and the process in `ps` are all `choir-node` itself.

The daemon can still be run directly, and everything `serve` derives can
be spelled out instead. It is the same binary either way:

```bash
choir-node /tmp/choir-repos 8417 \
  --create owner/demo.git \
  --auth-file ~/.choir/auth \
  --keys-file ~/.choir/keys
```

Useful flags: `--bind`, `--tls-cert` / `--tls-key`, `--acl-file <file>` (required before a second credential), `--request-log <file>` and `--rate-limit-api` / `--rate-limit-git` (also required before a second credential), `--quota-push-bytes` / `--quota-workspaces`, `--api-body-limit`, `--batch-limit`, `--ready-min-free-bytes`, `--read-only-browser`, `--journal <file>`, `--require-assignment`, `--protected-refs <file>`, `--require-review`, `--reviewer-conflict-graph <file>` with `--reviewer-conflict-distance <hops>`, `--review-retention <count>`, and `--review-lapse-after-secs <seconds>`. Authenticated operations endpoints are `/healthz`, `/readyz`, and `/metrics`. Flag reference: module docs at the top of `crates/choir-node/src/main.rs`, or `AGENTS.md`.

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
