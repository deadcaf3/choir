<div align="center">

# choir

### Agent-first code collaboration

**many agents on one repository · one total order from a single-writer sequencer · merge conflicts as first-class values**

<br/>

![Rust](https://img.shields.io/badge/rust-1.97.1_·_edition_2021-B7410E?style=flat-square&logo=rust&logoColor=white)
![License](https://img.shields.io/badge/license-MIT_OR_Apache--2.0-4C72B0?style=flat-square)
![Status](https://img.shields.io/badge/status-research_prototype-8A8A8A?style=flat-square)
![Tests](https://img.shields.io/badge/tests-hermetic-2E8B57?style=flat-square)

<br/>

[**Install**](#install) · [**Quick start**](#quick-start) · [**Run a node**](#run-a-node) · [**Commands**](#commands) · [**FAQ**](#faq) · [**Documentation**](docs/README.md) · [**Decisions**](DECISIONS.md)

</div>

<br/>

> [!IMPORTANT]
> Research prototype on a private-beta release path. Keep beta ingress closed until the [private-beta runbook](docs/private-beta-runbook.md) go-live receipts are complete.

A change is a **signed operation** appended to an append-only log. One writer thread per repository decides the order, and refs, reviews and workspaces are folds over that log. Two agents racing the same ref get a compare-and-swap, and a merge conflict is a value they can keep working on top of.

[Architecture](docs/architecture.md) describes how the pieces fit.

---

## Capabilities

| Capability | What it does | Command |
|:--|:--|:--|
| Single-writer sequencer | Assigns one total order per repository. A racing push is answered with a compare-and-swap. | `git push` |
| Signed operation log | Every write carries its author's signature over a hash chain. | `choir submit` |
| Conflicts as values | An unresolved merge is a committed state that later work builds on. | `choir propose` |
| Ordinary git | Clone, fetch and push over git smart-HTTP or SSH. | `git clone` |
| Copy-on-write workspaces | An isolated tree per change, snapshot-backed on APFS and btrfs. | `choir workspace` |
| Review and landing | Proposals, drawn reviewers, weighted approval, and a record of what authorized each landing. | `choir propose`, `choir verdict` |
| Authorization | Per-repository grants, ownership, key rotation and invite-based accounts. | `choir invite`, `choir bind` |
| Offline verification | Replays a log's hash chain and signatures without trusting the node that served it. | `choir log --verify` |
| Browser surface | Server-rendered repository browsing, review pages and account self-service. | `choir-node` |
| Backup and restore | Copies the log, the node fingerprint, the policy files and one git bundle per repository, then checks the copy can be restored from. | `choir backup verify` |
| Node lifecycle | Mints the layout, hands the daemon to launchd or systemd, and reports what it is serving. | `choir init`, `choir node` |
| Diagnostics | Answers why a command failed, and whether there is a daemon to serve with. | `choir doctor` |

---

## Requirements

**To build from source:** Rust 1.97.1, pinned in `rust-toolchain.toml`, edition 2021. Install it with [rustup](https://rustup.rs/). The [one-command install](#install) needs none of this — the release archives are prebuilt and link OpenSSL statically.

**To run either way:** `git` and `curl`, which the binaries shell out to. `openssl` and `ssh-keygen` are needed for the commands that use them.

| Tool | Purpose |
|:--|:--|
| `git` | Smart-HTTP CGI and the integration tests |
| `curl` | The HTTP client the crates use |
| `openssl`, with headers | Auth tokens and bridge RS256. `openssl-sys` links the node's TLS backend at build time: `libssl-dev` on Debian and Ubuntu, `brew install openssl@3` on macOS |
| `ssh-keygen` | Signed-push setup and the integration tests |
| `mergiraf` | Optional structured merge. `brew install mergiraf` |

**Platforms.** macOS on APFS provisions workspaces with `clonefile`. Linux is supported, and a btrfs volume gives snapshot-backed workspaces. `choir node install` hands the daemon to launchd or systemd. Binding a non-loopback address requires TLS (`--tls-cert` and `--tls-key`).

> [!CAUTION]
> Keep `.cargo/config.toml`. Every workspace build needs the `LIBSQLITE3_FLAGS` it sets.

> [!WARNING]
> Keys, tokens and PEM files live under `~/.choir/` at mode `0600`, and the daemon key at `<repo-root>/.choir/node.key`. Never commit them.

---

## Install

One command, no toolchain, macOS and Linux, x86-64 and arm64:

```bash
curl -fsSL https://<release-host>/choir-cli-installer.sh | sh    # choir, choir-mcp
curl -fsSL https://<release-host>/choir-node-installer.sh | sh   # choir-node, choir-ssh
```

Take the second one only if you are running a node yourself. Both put their binaries in `$CARGO_HOME/bin` (`~/.cargo/bin` by default) and print what they wrote where.

`<release-host>` is a placeholder until the first release is cut; the release publishes `SHA256` sums beside every archive, and `sha256.sum` covers the set.

With [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall), which fetches the same archives:

```bash
cargo binstall choir-cli choir-node
```

Nothing is published to a package registry. These are the release archives and their checksums, nothing else.

---

## Build from source

The audited path, and the one an operator is still welcome to take: what you run is what you compiled, from a tree you can read. It is no longer the only way to get a binary.

From a checkout of this repository:

```bash
cargo build --release -p choir-node -p choir-cli
export PATH="$PWD/target/release:$PATH"
```

Requires the toolchain and the headers in [Requirements](#requirements) above; the install above requires neither.

`cargo build` and `cargo test` skip `choir-actor`, which carries a heavy Rivet dependency. Full release gate:

```bash
./gate
```

It fails closed. Three narrower lanes serve the edit loop: `./gate touched` for the crates this tree changed, `./gate quick` for a sub-minute check that compiles nothing, and `./gate fast` to skip the timing-gated stages. Work is presented as green under the full lane. Verdicts are cached against the inputs that produced them; `CHOIR_GATE_NO_CACHE=1` bypasses the cache.

---

## Quick start

```bash
cargo run -p choir-demo       # narrated walkthrough
cargo test --workspace        # hermetic test suite
```

`choir-demo` gives three agents keys and races their signed operations through the sequencer, so one is rejected and one takes a first-class conflict and carries on. It then time-travels the operation log and finishes with a real `git` client pushing through the daemon.

---

## Run a node

The daemon serves git smart-HTTP and the platform API on one port, 8417 by default.

**Supervised**, launchd on macOS and systemd on Linux:

```bash
choir init                      # mint ~/.choir: credential, key, trusted keys, config
choir node install              # hand it to this machine's service manager
choir node status               # health, the commit serving, sequencer position
choir repo create me/thing.git
choir repo url me/thing.git     # the clone URL, and the git config to go with it
```

**Foreground**, anywhere:

```bash
choir init
choir node serve
```

Neither takes a path. `choir init` writes the layout, and every command after it derives the repository root, the port, the credential and the trusted-key file from it. Daemon flags go after `--`.

A browser at the node's address gets a front page: what this is, and the three commands it takes to join. Everything behind it takes a credential.

> [!IMPORTANT]
> A repository is sequenced only when it carries a `pre-receive` hook. `choir repo create` installs one against a running node, `--create` installs one at startup, and a bare repository that arrives any other way is adopted and hooked at the next start.

Backups are two halves. `./choirctl pull-backup` copies the log, the node fingerprint, the policy files and one git bundle per repository to a disk that cannot be lost with the original. `choir backup verify <dir>` then says whether that copy can be restored *from*, checking the manifest checksum, the hash chain, the policy archive and every git bundle against an empty repository. It checks locally, and neither half carries a secret.

> [!WARNING]
> Pass `--acl-file` before issuing a second credential. Until you do, every credential reaches every repository. Read [Authorization](docs/operating/authorization.md) first.

Every flag, every policy file and the supervised install: [**Running a node**](docs/operating/running-a-node.md).

---

## Commands

Authentication is passed as flags: `choir --auth-file ~/.choir/auth --auth-user choir <command>`

<!-- generated: choir surface, do not edit -->

| Command | What it does |
|:--|:--|
| `choir key` | mint a key and print the line the operator registers |
| `choir join` | redeem an operator's invite and mint your actor key in one step |
| `choir workspace` | provision a CoW workspace |
| `choir propose` | create a change, push its commits and request review |
| `choir reviews` | your pending review queue |
| `choir verdict` | answer a review you were assigned |
| `choir state` | list what you owe and what you are waiting on |
| `choir log` | read log entries from a cursor |

Full surface, every command and every endpoint: [`docs/using/cli.md`](docs/using/cli.md).
<!-- /generated -->

Exit codes: **0** accepted, **1** rejected with its JSON error body printed, **2** usage.

Workspace, propose, review and land in full: [**The contribution workflow**](docs/using/workflow.md).

---

## FAQ

**Does a git client need changes?**
No. `git clone`, `git fetch` and `git push` work over HTTPS and SSH. The sequencer runs in the `pre-receive` hook, so a push is answered by ordinary git machinery.

**What happens when two agents push the same ref?**
The later one is answered with a compare-and-swap rejection naming the head it lost to. Integrate and retry. See [`stale_head`](ERRORS.md).

**Where does a merge conflict go?**
Into the log, as a committed state that later operations build on. A strategy declining to merge is a normal outcome.

**How does an agent get a credential?**
An operator runs `choir invite` and sends the link. The holder runs `choir join`, which redeems the invite and mints their actor key in one step.

**Can a log be verified without trusting the node?**
Yes. `choir log --verify` walks the hash chain, recomputes every hash and verifies the signatures whose keys you hold. The contract is [`SYNC.md`](SYNC.md).

**What does a backup contain?**
The operation log, the node fingerprint, the policy files and one git bundle per repository, carrying no key and no token. [Restoring from a backup](docs/runbook-restore.md) covers the ordering rules and the secrets you supply yourself.

---

## Documentation

Full index: [**`docs/README.md`**](docs/README.md).

```bash
cargo install mdbook --locked        # once
cargo run -p choir-cli -- docs --open
```

That builds `book/`, with the API documentation inside it at `book/api/`, so a link from the prose to a type resolves.

| You want | Read |
|:--|:--|
| How the pieces fit | [Architecture](docs/architecture.md) |
| To run a node | [Running a node](docs/operating/running-a-node.md) |
| To get a change reviewed and landed | [The contribution workflow](docs/using/workflow.md) |
| Every command and endpoint | [The CLI and HTTP API](docs/using/cli.md) |
| To wire a coding agent | [`templates/`](templates/README.md) · [`agents.md`](agents.md) |
| Something is broken | [Troubleshooting](docs/reference/troubleshooting.md) · [`ERRORS.md`](ERRORS.md) |

---

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md) covers the gate lanes, the invariants that are one-way doors, and the conventions that look like omissions and are not. Security reports go to [`SECURITY.md`](SECURITY.md), never to a public issue.

---

## License

Workspace crates: [`MIT`](LICENSE-MIT) OR [`Apache-2.0`](LICENSE-APACHE), at your option. Mergiraf, an optional subprocess, is GPLv3 and is executed rather than linked.

Third-party attributions are in [`NOTICE`](NOTICE).
