<div align="center">

# choir

### Agent-first code collaboration

**many agents on one repository · one total order from a single-writer sequencer · merge conflicts as first-class values**

![Rust](https://img.shields.io/badge/rust-1.97.1_·_edition_2021-B7410E?style=flat-square&logo=rust&logoColor=white)
![License](https://img.shields.io/badge/license-MIT_OR_Apache--2.0-4C72B0?style=flat-square)
![Status](https://img.shields.io/badge/status-research_prototype-8A8A8A?style=flat-square)

[**Quick start**](#quick-start) · [**Install**](#install) · [**Run a node**](#run-a-node) · [**How it works**](#how-it-works) · [**Documentation**](docs/README.md) · [**Why**](docs/why.md)

</div>

<br/>

Every change is a signed operation on an append-only log, ordered by one
writer thread. A race gets a compare-and-swap rejection, not a lost write.
A merge conflict is a committed value. Plain `git clone`, `fetch` and
`push` work: the sequencer is a `pre-receive` hook.

---

## Quick start

Needs only `git`. The last command prints the node URL:

```bash
curl -fsSL https://github.com/deadcaf3/choir/releases/latest/download/choir-cli-installer.sh | sh
curl -fsSL https://github.com/deadcaf3/choir/releases/latest/download/choir-node-installer.sh | sh
choir host
```

`choir host` mints the repository root, a `0600` credential, a trusted key
and a config file, then installs the daemon under launchd or systemd.

Narrated demo, no install:

```bash
git clone https://github.com/deadcaf3/choir
cd choir
cargo run -p choir-demo
```

---

## Install

**Prebuilt**, macOS and Linux, x86-64 and arm64, with a `SHA256` per
archive and a `sha256.sum` over the set:

```bash
curl -fsSL https://github.com/deadcaf3/choir/releases/latest/download/choir-cli-installer.sh | sh
```

That is `choir` and `choir-mcp`. Node binaries, only if you run one:

```bash
curl -fsSL https://github.com/deadcaf3/choir/releases/latest/download/choir-node-installer.sh | sh
```

Both land in `$CARGO_HOME/bin` (`~/.cargo/bin` by default).
[`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):
`cargo binstall choir-cli choir-node`.

**From source**, with [rustup](https://rustup.rs/):

```bash
cargo install --git https://github.com/deadcaf3/choir choir-cli choir-node
```

Nothing is on a package registry.

**Subprocesses.** `git` and `curl` always; `openssl` and `ssh-keygen`
where used; `mergiraf` for structured merges. Workspaces use APFS
`clonefile` on macOS, btrfs snapshots on Linux.
---

## Run a node

One port, 8417 by default, serves git smart-HTTP and the platform API.

```bash
choir host                      # nothing to running node, supervised, prints the URL
choir repo create me/thing.git  # a sequenced repository
choir repo url me/thing.git     # the clone URL and the git config to go with it
choir node status               # health, the commit serving, sequencer position
```

`choir host --domain example.com` issues a Let's Encrypt certificate.
`choir host --foreground` execs the daemon instead of installing a service
unit. A non-loopback bind requires TLS.

`choir invite` prints one link; `choir join '<link>'` mints the key, stores
the token and points git at it.

> [!IMPORTANT]
> Pass `--acl-file` before issuing a second credential; until then every
> credential reaches every repository. See
> [Authorization](docs/operating/authorization.md).

`./choirctl pull-backup` copies the log, node fingerprint, policy files
and one git bundle per repository. `choir backup verify <dir>` checks it
restores. Neither carries a secret.

Every flag and policy file: [**Running a node**](docs/operating/running-a-node.md).

---

## How it works

A change is a **signed operation** on a hash-chained log. One writer thread
per repository decides the order. Refs, reviews and workspaces are folds
over that log.

- **Two agents push the same ref.** The later one gets a compare-and-swap
  rejection naming the winning head. Integrate and retry.
- **A merge cannot be resolved.** The conflict is committed as a value.
- **Proof.** `choir log --verify` recomputes every hash and checks the
  signatures whose keys you hold.
- **Not included.** No Git LFS server; submodules are unsequenced
  gitlinks; a force-push over a rejection is refused.

Layer map: [Architecture](docs/architecture.md).

---

## Commands

Authentication is passed as flags: `choir --auth-file ~/.choir/auth --auth-user choir <command>`

<!-- generated: choir surface, do not edit -->

| Command | What it does |
|:--|:--|
| `choir host` | take this machine from nothing to a running node and print the… |
| `choir key` | mint a key and print the line the operator registers |
| `choir join` | redeem the one link an operator sent you and set this machine… |
| `choir workspace` | provision a CoW workspace |
| `choir propose` | create a change, push its commits and request review, with no… |
| `choir reviews` | your pending review queue |
| `choir verdict` | answer a review you were assigned |
| `choir state` | list what you owe and what you are waiting on |
| `choir log` | read log entries from a cursor |

Full surface, every command and every endpoint: [`docs/using/cli.md`](docs/using/cli.md).
<!-- /generated -->

Exit codes: **0** accepted, **1** rejected with its JSON error body,
**2** usage. `choir doctor` explains a failure.

---

## Documentation

Full index: [**`docs/README.md`**](docs/README.md).

| You want | Read |
|:--|:--|
| Why | [Why choir exists](docs/why.md) |
| How the pieces fit | [Architecture](docs/architecture.md) |
| To run a node | [Running a node](docs/operating/running-a-node.md) |
| To get a change reviewed and landed | [The contribution workflow](docs/using/workflow.md) |
| Every command and endpoint | [The CLI and HTTP API](docs/using/cli.md) |
| To wire up a coding agent | [`templates/`](templates/README.md) · [`AGENTS.md`](AGENTS.md) |
| Something is broken | [Troubleshooting](docs/reference/troubleshooting.md) · [`ERRORS.md`](ERRORS.md) |

Build the book:

```bash
cargo install mdbook --locked
choir docs --open
```

---

## Building and testing

```bash
cargo build --release -p choir-node -p choir-cli
cargo test --workspace                              # hermetic: no network, no services
```

Full release gate:

```bash
./gate
```

Fails closed. Edit-loop lanes: `./gate touched` (changed crates),
`./gate quick` (compiles nothing), `./gate fast` (skips timing-gated
stages).

> [!CAUTION]
> Keep `.cargo/config.toml`. Every build needs its `LIBSQLITE3_FLAGS`, and
> the failure it prevents does not name itself.

---

## Status and scope

> [!IMPORTANT]
> **Research prototype.** `DECISIONS.md` is the register behind every
> design choice; the invariants in [`CONTRIBUTING.md`](CONTRIBUTING.md)
> are one-way doors.
>
> **The code is public; write access is not.** Fork and open a pull
> request.
>
> **A hosted node is separate and invite-only.** Running your own needs
> nothing from anybody.

Keys, tokens and PEM files: `~/.choir/` at mode `0600`, the daemon key at
`<repo-root>/.choir/node.key`. Never commit them.

---

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md): gate lanes, invariants, house
conventions. Security reports: [`SECURITY.md`](SECURITY.md), never a
public issue.

## License

MIT ([`LICENSE-MIT`](LICENSE-MIT)) or Apache-2.0
([`LICENSE-APACHE`](LICENSE-APACHE)). Mergiraf, an optional subprocess, is
GPLv3 and executed rather than linked. Attributions: [`NOTICE`](NOTICE).
