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

Ten agents editing one repository will collide. choir gives every change a
place in one order: a signed operation on an append-only log, decided by a
single writer thread, so a race is answered with a compare-and-swap instead
of a lost write, and a merge conflict is a value that later work can build
on rather than an error that stops it.

Your git client does not change. `git clone`, `git fetch` and `git push`
all work, because the sequencer runs inside an ordinary `pre-receive` hook.

---

## Quick start

You need [Rust](https://rustup.rs/) and `git`. Two commands, and the second
one prints the URL:

```bash
cargo install --git https://github.com/deadcaf3/choir choir-cli choir-node
choir host
```

`choir host` takes a machine from nothing to a running node: it mints the
repository root, a credential at mode `0600`, a key the node trusts and a
config file, then hands the daemon to launchd or systemd and prints the
address. Open it in a browser and you get a front page and a sign-in.

Want to see the ideas move before installing anything?

```bash
git clone https://github.com/deadcaf3/choir
cd choir
cargo run -p choir-demo
```

That gives three agents keys and races their signed operations through the
sequencer, so one gets rejected, one takes a first-class conflict and carries
on, and a real `git` client pushes through the daemon at the end. It is
narrated, and it is the fastest way to understand the model.

---

## Install

**From source, today.** This is the working install until the first tagged
release, and it needs only a Rust toolchain:

```bash
cargo install --git https://github.com/deadcaf3/choir choir-cli choir-node
```

`choir-cli` gives you `choir` and `choir-mcp`. `choir-node` gives you
`choir-node` and `choir-ssh`, and you only need it if you are running a node
rather than talking to someone else's. Both land in `~/.cargo/bin`.

**From a release archive.** Once a version is tagged, prebuilt archives for
macOS and Linux on x86-64 and arm64 are published with `SHA256` sums beside
them, installable with one `curl` and no toolchain, or with
[`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall). Nothing is
published to a package registry.

**What the binaries shell out to.** `git` and `curl` always; `openssl` and
`ssh-keygen` for the commands that use them; `mergiraf` only if you want
structured merges. On macOS, workspaces use APFS `clonefile`; on Linux, a
btrfs volume gives the same snapshot-backed behaviour.

---

## Run a node

The daemon serves git smart-HTTP and the platform API on one port, 8417 by
default.

```bash
choir host                      # nothing to running node, supervised, prints the URL
choir repo create me/thing.git  # a sequenced repository
choir repo url me/thing.git     # the clone URL and the git config to go with it
choir node status               # health, the commit serving, sequencer position
```

`choir host --domain example.com` issues a Let's Encrypt certificate for a
name you already own. `choir host --foreground` execs the daemon instead of
installing a service unit, which is what a container wants. Binding anything
other than loopback requires TLS, and the node refuses to start without it.

To invite somebody, `choir invite` prints one link. They run
`choir join '<link>'`, which redeems it, mints their key, stores their token
and points git at it. The next thing they type is `git clone`.

> [!IMPORTANT]
> Pass `--acl-file` before you issue a second credential. Until you do, every
> credential reaches every repository.
> [Authorization](docs/operating/authorization.md) is the short read that
> prevents this.

Backups are two halves. `./choirctl pull-backup` copies the log, the node
fingerprint, the policy files and one git bundle per repository somewhere
that cannot be lost with the original. `choir backup verify <dir>` then says
whether that copy can be restored *from*. Neither half carries a secret.

Every flag and every policy file: [**Running a node**](docs/operating/running-a-node.md).

---

## How it works

A change is a **signed operation** appended to a hash-chained log. One writer
thread per repository decides the order. Refs, reviews and workspaces are all
folds over that log.

- **Two agents push the same ref.** The later one gets a compare-and-swap
  rejection naming the head it lost to. Integrate and retry.
- **A merge cannot be resolved.** The conflict is committed as a value, and
  later operations build on top of it. Nothing silently picks a side.
- **A reader wants proof.** `choir log --verify` walks the chain, recomputes
  every hash and checks the signatures whose keys you hold, so a served page
  is checkable against the one you already have.
- **Three things do not come along.** Git LFS has no server here, submodules
  are ordinary gitlinks and are not sequenced, and a force-push over a
  rejection is refused rather than obeyed.

[Architecture](docs/architecture.md) has the layer map and the path one
operation takes end to end.

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

Exit codes: **0** accepted, **1** rejected with its JSON error body printed,
**2** usage. `choir doctor` answers why a command failed.

---

## Documentation

Full index: [**`docs/README.md`**](docs/README.md). The same pages build into
a book with the API reference inside it.

| You want | Read |
|:--|:--|
| The five-minute version of why | [Why choir exists](docs/why.md) |
| How the pieces fit | [Architecture](docs/architecture.md) |
| To run a node properly | [Running a node](docs/operating/running-a-node.md) |
| To get a change reviewed and landed | [The contribution workflow](docs/using/workflow.md) |
| Every command and endpoint | [The CLI and HTTP API](docs/using/cli.md) |
| To wire up a coding agent | [`templates/`](templates/README.md) · [`AGENTS.md`](AGENTS.md) |
| Something is broken | [Troubleshooting](docs/reference/troubleshooting.md) · [`ERRORS.md`](ERRORS.md) |

To build the book yourself, from a checkout:

```bash
cargo install mdbook --locked
choir docs --open
```

---

## Building and testing

From a checkout, if you want to compile rather than install:

```bash
cargo build --release -p choir-node -p choir-cli
cargo test --workspace                              # hermetic: no network, no services
```

Full release gate:

```bash
./gate
```

It fails closed on format, tests, clippy, rustdoc, the measurement spike,
freshness and the history scan. Three narrower lanes serve the edit loop:
`./gate touched` for the crates this tree changed, `./gate quick` for a
sub-minute check that compiles nothing, and `./gate fast` to skip the
timing-gated stages. Work is presented as green under the full lane.

> [!CAUTION]
> Keep `.cargo/config.toml`. Every workspace build needs the
> `LIBSQLITE3_FLAGS` it sets, and the failure it prevents does not name
> itself.

---

## Status and scope

> [!IMPORTANT]
> **Research prototype.** `DECISIONS.md` is the register behind every design
> choice, and the invariants in [`CONTRIBUTING.md`](CONTRIBUTING.md) are
> one-way doors rather than style preferences.
>
> **The code is public; write access is not.** Read it, clone it, fork it and
> open a pull request. Nobody but the maintainer can push here and every
> change lands through review, so a fork and a PR is the way in rather than a
> limitation of it.
>
> **A hosted node is a separate thing, and it is invite-only.** Running the
> daemon yourself needs nothing from anybody. Accounts on somebody *else's*
> node are theirs to grant.

Keys, tokens and PEM files live under `~/.choir/` at mode `0600`, and the
daemon key at `<repo-root>/.choir/node.key`. Never commit them.

---

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md) covers the gate lanes, the invariants
that are one-way doors, and the conventions that look like omissions and are
not. Security reports go to [`SECURITY.md`](SECURITY.md), never to a public
issue.

## License

MIT ([`LICENSE-MIT`](LICENSE-MIT)) or Apache-2.0
([`LICENSE-APACHE`](LICENSE-APACHE)), at your option. Mergiraf, an optional
subprocess, is GPLv3 and is executed rather than linked. Third-party
attributions are in [`NOTICE`](NOTICE).
