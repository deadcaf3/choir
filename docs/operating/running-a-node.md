# Running a node

One binary serves two protocols on one port: git smart-HTTP for `clone`,
`fetch` and `push`, and the platform API for signed operations. This page is
how to start it and what each policy file means.

Using a node: `docs/using/cli.md`. The policy behind each flag:
`docs/operating/authorization.md`, `docs/operating/limits.md`,
`docs/operating/webhooks.md`, `docs/operating/observability.md`,
`docs/operating/transports.md`.

Default port **8417**. A repository is sequenced only when it carries a
`pre-receive` hook: create with `choir repo create` or `--create`; a bare
repository that arrives any other way is adopted and hooked at the next
start. With no arguments the binary uses `./repos` and port 8417;
configured invocations must supply both `<repo-root>` and `<port>` before
any flags.

## Getting the binaries

One command, no Rust toolchain and no OpenSSL headers. macOS and Linux,
x86-64 and arm64:

```bash
curl -fsSL https://choirs.dev/download/install.sh | sh
```

That installs both packages, which is what this page needs: every command
here is `choir`, and `choir node serve` execs the daemon. The four
binaries land in `$CARGO_HOME/bin` (`~/.cargo/bin` by default) as
`choir-node`, `choir-ssh`, `choir` and `choir-mcp`; each archive is
checked against the `SHA256` beside it on the shelf.

The node renders that installer from the `Host` you asked it on, so the
script and the archives come from the same origin and no address is
configured anywhere. Any node with a shelf serves its own:
`https://<node>/download/`.

From source instead, needing only [rustup](https://rustup.rs/):

```bash
cargo install --git https://choirs.dev/choir/choir.git choir-cli choir-node
```

Neither package pulls in `choir-actor`, the one crate that needs this
workspace's `LIBSQLITE3_FLAGS`.

**From a checkout you already have**: the toolchain and headers the
[README](../../README.md#install) lists, then:

```bash
cargo build --release -p choir-node -p choir-cli
export PATH="$PWD/target/release:$PATH"
```

`choir node status` and `choir --version` report the built commit from a
stamp the release workflow sets.

## One command

```bash
choir host
```

| you have | run | you get |
|:--|:--|:--|
| a laptop, or a box nobody else reaches | `choir host` | `http://127.0.0.1:8417`, in seconds, no certificate |
| a name pointing at this box | `choir host --domain node.example` | `https://node.example:8417` |
| a VPS and no name | `choir host --public` | `https://<this-ip>.sslip.io:8417` |

It prints the URL people use and, with `--invite`, one invite link:

```bash
choir host --repo me/thing.git --invite Ada
```

**The two public modes take two commands.** Obtaining a certificate is
privileged and `choir host` does not run `sudo`: it prints the line to
paste and exits 3. Paste it, run the same `choir host` again, and it
continues.

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

Dry-run the certificate step without spending a Let's Encrypt rate-limit
slot:

```bash
sudo choir node tls node.example --user "$(id -un)" --dry-run
```

### Why a public bind needs a certificate at all

`choir-node` refuses a non-loopback bind without TLS
([invariant 9](../../CONTRIBUTING.md)). There is no flag to soften it.

### `--public`, and why the address is ugly

`choir host --public` uses an [sslip.io](https://sslip.io) name built from
the box's address, `203-0-113-7.sslip.io` for `203.0.113.7`, so a
certificate can be issued with no DNS. Replace it any time:

```bash
choir host --domain the-name-you-bought.example
```

sslip.io and nip.io share one Let's Encrypt rate-limit pool. If issuance
fails for that reason, `--public-name <name>` takes any name pointing at
this box.

## What it did

Every step is a command you could run yourself:

| step | what | undo |
|:--|:--|:--|
| state | `choir init`: `~/.choir` with the credential, actor key, trusted keys, and `.choir/config` | delete `~/.choir` |
| acl + accounts | `~/.choir/acl` granting the operator everything, and an empty `~/.choir/accounts.jsonl` | delete either file |
| certificate | `choir node tls`: certbot, a renewal deploy hook, the pair projected where the node can read it, and `~/.choir/tls.enabled` | `sudo rm /etc/letsencrypt/renewal-hooks/deploy/choir-tls`, `sudo certbot delete` |
| address | `~/.choir/public-url`, and `.choir/config` pointed here | edit either |
| supervised | `choir node install`: a launchd agent or a `systemd --user` unit | `choir node uninstall` |
| healthy | polls `/healthz` | |
| repository | `choir repo create`, with `--repo` | |
| invited | `choir invite`, with `--invite` | `choir revoke` |

Two things it prints but never runs:

- **the firewall.** `ufw` or `firewalld`, opening the serving port, and
  port 80 when HTTP-01 is the challenge (renewals rebind it every ~60 days).
- **linger.** Without `loginctl enable-linger`, systemd stops a `--user`
  unit at logout. `choir host` stops with the line to paste; `--yes` accepts
  a node that dies at logout.

### TLS, and what happens every 60 days

**The daemon terminates TLS itself** with `--tls-cert` and `--tls-key`. An
nginx or Caddy front is a valid topology; it is not required.

**The daemon reads its certificate once, at bind.** The certbot deploy hook
`choir node tls` installs re-projects the pair into `~/.choir/tls/` and
restarts the unit. Renewal runs on certbot's timer.

Root for certbot, an unprivileged account for the node; the node's copy of
the pair is a copy for that reason.

The ACME account is registered **without an email**. `sudo certbot renew
--dry-run` is the manual check; `choir doctor` reports the expiry date.

### Checking it

```bash
choir doctor
```

On a hosting machine it adds six rows: bind address, TLS on, certificate
expiry, linger, unit loaded and running, and whether the public URL
answers (asked from the box, so it proves name and certificate, not the
firewall).

### Uninstalling

```bash
choir node uninstall
```

Removes the unit and stops the node. **Keeps `~/.choir`**: keys,
repositories and the op log. If a renewal hook is installed it prints the
two `sudo` lines that remove it and the certificate.

## In a container

`Dockerfile` at the repository root builds from source and runs the daemon
as an unprivileged user with `/var/lib/choir` as a volume. Loopback mode and
bring-your-own-certificate mode; no ACME client in the daemon. No compose
file: the volume and the port are one flag each.

## By hand

```bash
choir init                                  # the same layout, without a service manager
choir node serve                            # runs here, in this terminal
choir node serve -- --reviewers-file ~/.choir/reviewers   # any daemon flag, after `--`
```

`choir node serve` derives root, port, credential and trusted-key file from
what `choir init` wrote, then **execs** the daemon. Three files switch
behaviour by existing:

| file | effect |
|:--|:--|
| `~/.choir/tls.enabled` | two lines, cert path then key path: binds `0.0.0.0` with that pair |
| `~/.choir/acl` | `--acl-file` |
| `~/.choir/accounts.jsonl` | `--accounts-file` (refused without an ACL) |

Supervised, step by step:

```bash
choir init                      # mint ~/.choir: credential, key, trusted keys, config
choir node install              # hand it to launchd (macOS) or systemd (Linux)
choir node status               # health, the commit serving, sequencer position
choir repo create me/thing.git  # a repository on the running node
choir repo url me/thing.git     # the clone URL, and the git config to go with it
choir node logs                 # the tail of the daemon log
# choir node restart | stop | uninstall   -- uninstall keeps ~/.choir
```

`choir node install` writes a unit that runs `choir node serve`. Daemon
flags go after `--` and are recorded in the unit:

```bash
choir node install -- --acl-file ~/.choir/acl --rate-limit-api 60
```

The daemon can be run directly:

```bash
choir-node /tmp/choir-repos 8417 \
  --create owner/demo.git \
  --auth-file ~/.choir/auth \
  --keys-file ~/.choir/keys
```

Useful flags: `--bind`, `--tls-cert` / `--tls-key`, `--acl-file <file>` (required before a second credential), `--request-log <file>` and `--rate-limit-api` / `--rate-limit-git` (also required before a second credential), `--quota-push-bytes` / `--quota-workspaces`, `--api-body-limit`, `--batch-limit`, `--ready-min-free-bytes`, `--read-only-browser`, `--journal <file>`, `--require-assignment`, `--protected-refs <file>`, `--require-review`, `--reviewer-conflict-graph <file>` with `--reviewer-conflict-distance <hops>`, `--review-retention <count>`, and `--review-lapse-after-secs <seconds>`. Authenticated operations endpoints: `/healthz`, `/readyz`, `/metrics`. Flag reference: `crates/choir-node/src/main.rs` module docs, or `AGENTS.md`.

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
> **Without `--acl-file`, every credential reaches every repository.** A
> second `user:token` line can clone every repo, push to any unprotected ref
> and provision workspaces anywhere. **Do not issue a second credential
> without an ACL.** `choir host` writes one.

## Serving your own binaries

`--downloads-dir <dir>` publishes one directory at `/download/`, with no
credential (D79). Three things are served there and only three: an index
of the directory, any file in it by exact name, and `install.sh`, which
the node renders from the `Host` of the request rather than reading off
the shelf. That is what lets the script fetch from the node it came from
without anybody configuring an address.

The installers run `./choirctl deploy` with the flag whenever
`~/.choir/downloads` exists, so creating the directory is the switch:

```bash
./choirctl publish-release            # or: publish-release v0.1.0
./choirctl deploy
```

`publish-release` reads `owner/name` from `~/.choir/release-repo` and
copies that release's archives and digests onto the shelf. It
deliberately leaves two things behind: the packaging tool's own
`*-installer.sh`, which carries the release host baked in at build time
and would send a reader straight back to it, and `source.tar.gz`, which
the node already serves over git at a revision a reader can name.

**The node is not the builder.** CI builds and publishes; this changes
where a reader fetches from. The digests served beside the archives
prove the transfer, not the build, so the trust boundary is TLS plus
whoever runs the node.

Names are one path segment of `[A-Za-z0-9._-]`, dotfiles are refused and
symlinks are not followed, so the shelf can only hand out a regular file
an operator put directly in it.

## Behind a TLS proxy

For the private beta, bind `127.0.0.1` and terminate TLS at a hardened
reverse proxy. Do not use the legacy direct-TLS installer. The beta service
renderer requires an ACL, protected-ref review policy, scoped operations,
operator-issued auth, and the read-only browser.

[`docs/private-beta-runbook.md`](../private-beta-runbook.md) covers the
network hold, renderers, backups, CI packaging, staging promotion,
monitoring, rollback and go-live receipts.

`scripts/flip/RUNBOOK.md` is the operator's own dogfood procedure; not the
page to start from.

## A copy is a seed

A node that dies takes its repositories with it, unless a copy of them
exists somewhere else. The cheapest copy needs no choir process at all:

```bash
choir-node --export <root> <dir>        # on the node: the log, one git bundle per repository, a manifest
# copy <dir> anywhere: another disk, an object store, a static web host
choir-node --verify-export <dir>        # on the copy, with the same binary
```

`--verify-export` folds the log and requires every ref the view names to
be in that repository's bundle at the same oid (D61), so a copy that
verifies is the node's history and the objects that history points at,
checked on the machine that holds it. `choir-node --import <dir> <root>`
turns it back into a node.

What the copy proves, and what it does not:

- **It proves integrity.** The log is a hash chain, each entry carries its
  author's signature, and the bundles hold every commit the log names.
- **It does not prove freshness.** A copy is the node as of the moment it
  was taken. Nothing in it says the node has not moved on since, or that
  the copy you are holding is the latest one taken.
- **It does not witness.** A copy made by the node's own operator is the
  node's own word. It cannot tell you whether a second reader was shown
  the same history. That needs a seed run by somebody else; see
  [SYNC.md](../../SYNC.md) for what a seed signs and how two statements
  are compared.

Secrets and policy files are never in an export, by construction and then
by inspection; `docs/runbook-restore.md` covers what a restore needs
beside it.
