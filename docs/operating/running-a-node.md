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

## One command

```bash
choir host
```

There are three of them, and which one you want depends only on what
your machine is:

| you have | run | you get |
|:--|:--|:--|
| a laptop, or a box nobody else reaches | `choir host` | `http://127.0.0.1:8417`, in seconds, no certificate |
| a name pointing at this box | `choir host --domain node.example` | `https://node.example:8417` |
| a VPS and no name | `choir host --public` | `https://<this-ip>.sslip.io:8417` |

It prints two things on stdout: the URL people use, and — with
`--invite` — one invite link for the first person.

```bash
choir host --repo me/thing.git --invite Ada
```

**The two public modes take two commands, not one.** Obtaining a
certificate is privileged, and `choir host` does not run `sudo` on your
behalf: it prints the one line to paste and stops, exit 3. Paste it, run
the same `choir host` again, and it carries on from where it stopped.

```bash
$ choir host --domain node.example

  ok  state           /home/you/.choir — credential, key, trusted keys

  next: a certificate for node.example has to be issued as root.
  certbot writes /etc/letsencrypt, and the renewal hook that keeps
  this working for the next two years lives there too. Paste this:

    sudo choir node tls node.example --user you --port 8417
    sudo ufw allow 8417/tcp  &&  sudo ufw allow 80/tcp

  then: choir host --domain node.example
```

Try the certificate step without spending a Let's Encrypt rate-limit
slot on finding out that port 80 is closed:

```bash
sudo choir node tls node.example --user "$(id -un)" --dry-run
```

### Why a public bind needs a certificate at all

`choir-node` refuses a non-loopback bind without TLS. That is
[invariant 9](../../CONTRIBUTING.md), the privacy rule written as code
rather than as a doc note, and there is deliberately no flag to soften
it — not for testing either. It is the reason modes 2 and 3 are a
certificate first and a node second.

### `--public`, and why the address is ugly

With no domain, `choir host --public` gives the box a
[sslip.io](https://sslip.io) name built from its own address —
`203-0-113-7.sslip.io` for `203.0.113.7` — which resolves without you
touching DNS, so a certificate can be issued with zero setup. It is not
a name anyone will remember, and you can replace it whenever you like:

```bash
choir host --domain the-name-you-bought.example
```

sslip.io and nip.io are run by the same maintainer and share one Let's
Encrypt rate-limit pool, which has been exhausted before. If issuance
fails for that reason, `--public-name <name>` takes any name you can
point at this box instead.

## What it did

Every sub-step is a command you could have run yourself, and `choir
host` is the order rather than a new mechanism.

| step | what | undo |
|:--|:--|:--|
| state | `choir init` — `~/.choir` with the credential, actor key, trusted keys, and `.choir/config` | delete `~/.choir` |
| acl + accounts | `~/.choir/acl` granting the operator everything, and an empty `~/.choir/accounts.jsonl` so invites can be minted | delete either file |
| certificate | `choir node tls` — certbot, a renewal deploy hook, the pair projected where the node can read it, and `~/.choir/tls.enabled` | `sudo rm /etc/letsencrypt/renewal-hooks/deploy/choir-tls`, `sudo certbot delete` |
| address | `~/.choir/public-url`, and `.choir/config` pointed here | edit either |
| supervised | `choir node install` — a launchd agent or a `systemd --user` unit | `choir node uninstall` |
| healthy | polls `/healthz` until it answers | — |
| repository | `choir repo create`, with `--repo` | — |
| invited | `choir invite`, with `--invite` | `choir revoke` |

Two things it detects and prints but never runs, because both change how
much of the machine the world reaches:

- **the firewall.** `ufw` or `firewalld`, with the line that opens the
  serving port — and port 80 as well when HTTP-01 is the challenge,
  because renewals rebind it every ~60 days.
- **linger.** Without `loginctl enable-linger`, systemd stops a
  `--user` unit at logout and takes the node with it. `choir host` stops
  with the one line to paste; `--yes` accepts a node that dies at logout
  instead.

### TLS, and what happens every 60 days

**The daemon terminates TLS itself.** No proxy, no second package to
install, no second configuration to keep in step — `--tls-cert` and
`--tls-key`, and the node binds the public address directly. An nginx or
Caddy front is still a valid topology and the dogfood node uses one; it
is not the one a stranger should have to stand up.

**The daemon reads its certificate once, when it binds. It has no
reload.** A rotated pair reaches the running node only through a
restart, so the certbot deploy hook `choir node tls` installs does both:
re-projects the pair into `~/.choir/tls/` where the unprivileged node can
read it, and restarts the unit. Renewal runs on certbot's own timer;
nothing of ours is on a schedule.

The privilege split is deliberate and matches
`scripts/flip/setup_tls.sh`: root for certbot, because
`/etc/letsencrypt` is genuinely root's, and an unprivileged account for
the node. The node's copy of the pair is a copy for that reason — a unit
reading the root-owned live directory works exactly until the first
renewal rotates the files.

The ACME account is registered **without an email**, on purpose: no
personal identifier goes into infrastructure. The cost is no
expiry-warning mail, which the deploy hook is the real answer to;
`sudo certbot renew --dry-run` is the manual check, and `choir doctor`
reports the expiry date.

### Checking it

```bash
choir doctor
```

On a machine hosting a node it adds six rows to the usual report: the
bind address, whether TLS is on, the certificate's expiry date, whether
linger is on, whether the unit is loaded and running, and whether the
public URL answers. The last is asked from the box itself, so it is a
hairpin — it proves the name resolves and the certificate matches it,
and the firewall row is what covers the rest.

### Uninstalling

```bash
choir node uninstall
```

Removes the unit and stops the node. It **keeps `~/.choir`** — the keys,
the repositories and the op log are in there, and no command here
deletes those. If a renewal hook is installed it says so and prints the
two `sudo` lines that remove it and the certificate, because that is the
one thing `choir host` created that lives outside the state directory
and the one thing an unprivileged uninstall cannot take back.

## In a container

`Dockerfile` at the repository root builds from source in a builder
stage and runs the daemon as an unprivileged user with
`/var/lib/choir` as a volume. It covers the loopback mode and the
bring-your-own-certificate mode; there is no ACME client in the daemon,
so issuing a certificate stays on the host. There is no compose file:
the volume and the port are one flag each.

## By hand

Everything above is composition. The daemon underneath has not changed,
and this is the audited path.

```bash
choir init                                  # the same layout, without a service manager
choir node serve                            # runs here, in this terminal
choir node serve -- --reviewers-file ~/.choir/reviewers   # any daemon flag, after `--`
```

`choir node serve` derives the repository root, the port, the credential
and the trusted-key file from what `choir init` wrote, then **execs** the
daemon — so the process you signal, the process the supervisor watches
and the process in `ps` are all `choir-node` itself. It also reads three
files whose *existence* is the switch, which is what lets a certificate
arrive later without the supervision file being re-rendered:

| file | effect |
|:--|:--|
| `~/.choir/tls.enabled` | two lines, cert path then key path: binds `0.0.0.0` with that pair |
| `~/.choir/acl` | `--acl-file` |
| `~/.choir/accounts.jsonl` | `--accounts-file` (refused without an ACL) |

Or supervise it without `choir host` deciding anything:

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
> **Without `--acl-file`, every credential reaches every repository.** The auth file authenticates and nothing else; the node prints a line saying so at startup. A second `user:token` line can then clone every repo, push to any unprotected ref, and provision workspaces anywhere. Protected refs and the review requirement still hold, so it cannot land on a gated `main` unreviewed. **Do not issue a second credential without an ACL.** `choir host` writes one for this reason: the accounts file that lets it mint an invite is refused without it.

## Behind a TLS proxy

For the private beta, keep `choir-node` bound to `127.0.0.1` and terminate TLS at a hardened reverse proxy. Do not use the legacy direct-TLS installer. The beta service renderer requires an ACL, protected-ref review policy, scoped operations, operator-issued auth, and the read-only browser mode. The proxy renderer preserves authentication, streams Git separately, and applies route-specific limits.

See [`docs/private-beta-runbook.md`](../private-beta-runbook.md) for the network hold, service and proxy renderers, backups, CI packaging, staging promotion, monitoring, rollback, and go-live receipts. Every Choir route remains authenticated. A separate anonymous marketing page must use another host and origin.

`scripts/flip/RUNBOOK.md` is the operator's own dogfood procedure. It assumes a cloud project, a mirror host and `choirctl`; it is not the page to start from.
