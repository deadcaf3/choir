<div align="center">

# choir

### Agent-first code collaboration

**many agents on one repository · one total order from a single-writer sequencer · merge conflicts as first-class values**

<br/>

![Rust](https://img.shields.io/badge/rust-1.94.1_·_edition_2021-B7410E?style=flat-square&logo=rust&logoColor=white)
![License](https://img.shields.io/badge/license-MIT_OR_Apache--2.0-4C72B0?style=flat-square)
![Status](https://img.shields.io/badge/status-research_prototype-8A8A8A?style=flat-square)
![Tests](https://img.shields.io/badge/tests-hermetic,_no_network-2E8B57?style=flat-square)

<br/>

[**Try it**](#try-it) · [**Run a node**](#run-a-node) · [**Day one**](#day-one) · [**Documentation**](docs/README.md) · [**Decisions**](DECISIONS.md)

</div>

<br/>

> [!IMPORTANT]
> **Status:** research prototype with a private-beta release path. Phase-0 gate passed; Phase 1 in progress. Not production software. Keep beta ingress closed until the [private-beta runbook](docs/private-beta-runbook.md) go-live receipts are complete.

A change here is not a diff against a branch. It is a **signed operation** appended to an append-only log. One writer thread per repository decides the order; every other structure — refs, reviews, workspaces — is a fold over that log. Two agents racing the same ref get a compare-and-swap, not a lock, and a merge conflict is a value they can keep working on top of.

[Architecture](docs/architecture.md) is the ten-minute version of why.

---

## Prerequisites

| Tool | Required? | Notes |
|:--|:--:|:--|
| Rust stable + Cargo | **yes** | Built with **1.94.1** (pinned in `rust-toolchain.toml`), edition 2021. Install via [rustup](https://rustup.rs/) or Homebrew. |
| `git` | **yes** | Smart-HTTP CGI + all integration tests. |
| `curl` | **yes** | Only HTTP client the crates use. |
| `openssl` | **yes** | Auth tokens, bridge RS256; tests shell out to it. **Headers too** (`libssl-dev` on Debian/Ubuntu, `brew install openssl@3` on macOS): the node's TLS is `tiny_http`'s OpenSSL backend, so `openssl-sys` links it at build time. |
| `ssh-keygen` | **yes** | Integration tests / signed-push setup. |
| `mergiraf` | optional | Structured merge slot. Without it, line merge + first-class conflicts still work. Homebrew: `brew install mergiraf`. |
| `jj` | optional | Not required to build or run choir. |

**OS / filesystem**

- **macOS (APFS):** supported. Fast CoW workspaces via `clonefile` / `cp -Rc`. Dogfood installer uses **launchd** (`./choirctl`).
- **Linux:** supported. Prefer a **btrfs** volume for workspace snapshots (Phase-0 gate used btrfs). Without CoW, provisioning still works but is slower.
- **Non-loopback bind** requires TLS (`--tls-cert` + `--tls-key`). Plain HTTP is loopback-only by design.

> [!CAUTION]
> **Do not remove** `.cargo/config.toml` (`LIBSQLITE3_FLAGS`). Every workspace build needs it (rivetkit / sqlite workaround).

> [!WARNING]
> **Secrets:** keys, tokens, PEMs under `~/.choir/` at mode `0600` (daemon key: `<repo-root>/.choir/node.key`). Never commit them.

---

## Build

```bash
git clone <this-repo> && cd choir
cargo build --release -p choir-node -p choir-cli
```

Default `cargo build` / `cargo test` skip `choir-actor` (heavy Rivet dep). Full release gate:

```bash
./gate
```

It fails closed, and three narrower lanes exist for the edit loop: `./gate touched` (only the crates this tree changed), `./gate quick` (sub-minute; compiles nothing), `./gate fast` (skips the timing-gated stages). None of them substitutes for the full lane, which is what a piece of work is presented as green under. Verdicts are cached against the inputs that produced them, so a repeat full run on an unchanged tree is cheap; `CHOIR_GATE_NO_CACHE=1` turns that off.

Put the CLI on your PATH (or use `cargo run -p choir-cli -- …`):

```bash
export PATH="$PWD/target/release:$PATH"
```

---

## Try it

```bash
cargo run -p choir-demo              # narrated walkthrough: keys, ops, conflict, real git push
cargo run -p choir-spike --release   # Phase-0 gate binary; nonzero exit = gate fail
cargo test --workspace               # hermetic: no network, no external services
```

> [!TIP]
> `choir-demo` is the fastest "what is this?" path. It gives three agents keys, races their signed ops through the single-writer sequencer — one gets rejected, one gets a first-class conflict and keeps working — time-travels the op log, and finishes with a real `git` client pushing through the daemon.

---

## Run a node

The daemon serves **git smart-HTTP** and the **platform API** on one port (default **8417**).

**macOS, supervised:**

```bash
./choirctl install-cli   # put `choir` on your PATH as a real binary
./choirctl install       # build, mint ~/.choir secrets, load launchd
./choirctl status
./choirctl url           # clone/push URL with credentials
```

**Any Unix, foreground:**

```bash
mkdir -p /tmp/choir-repos ~/.choir
printf 'choir:%s\n' "$(openssl rand -hex 32)" > ~/.choir/auth && chmod 600 ~/.choir/auth
cargo run -p choir-node -- /tmp/choir-repos 8417 --create owner/demo.git --auth-file ~/.choir/auth
```

Repos must be created with `--create` (or the installer) so the `pre-receive` hook is installed; a bare repo made any other way is **not** sequenced.

A browser at the bare address gets a front page rather than a password box: what this is, and the three commands it takes to join. Everything behind it needs a credential. Backups run from the same script, once the node lives on its own host — `./choirctl pull-backup` copies the log, the node fingerprint, the policy files and one git bundle per repository to a disk that cannot be lost with the original, and `./choirctl verify-backup` checks that copy offline. Neither ever carries a key or a token.

> [!WARNING]
> **Without `--acl-file`, every credential reaches every repository.** The auth file authenticates and nothing else. Read [Authorization](docs/operating/authorization.md) before issuing a second credential.

Every flag, every policy file and the supervised install in full: [**Running a node**](docs/operating/running-a-node.md).

---

## Day one

Auth on the CLI is flags, not env: `choir --auth-file ~/.choir/auth --auth-user choir <command> …`

<!-- generated: choir surface, do not edit -->

| Command | What it does |
|:--|:--|
| `choir key` | mint a key and print the line the operator registers; pass your channel name to print the bound form |
| `choir join` | redeem an operator's invite and mint your actor key in one step; writes the issued token to an auth file at 0600, and on a node started with --invite-binds-keys the key is registered by the redemption itself |
| `choir workspace` | provision a CoW workspace; advanced flags owner-sign an exact base and stable change, and each --path owner-signs a subtree this change declares it works within |
| `choir propose` | propose from a git checkout in one command: create the change, push the commits, checkpoint the revision and request review; the node and repository come from the git remote, and the branch name is the change identity, so re-running after an amend updates the same proposal |
| `choir reviews` | your pending review queue |
| `choir verdict` | answer a review you were assigned |
| `choir state` | your bounded next-actions document: verdicts you owe, what your changes need, what you are waiting on, each with a command and its risk |
| `choir log` | read log entries from a cursor; --verify checks continuity, recomputes every hash, and verifies the signatures whose keys you hold — SYNC.md as a flag |

Full surface, every command and every endpoint: [`docs/using/cli.md`](docs/using/cli.md).
<!-- /generated -->

Exit codes: **0** accepted, **1** rejected (its JSON error body is printed), **2** usage.

The whole loop — workspace, propose, review, land — is [**The contribution workflow**](docs/using/workflow.md).

---

## Documentation

Full index: [**`docs/README.md`**](docs/README.md).

Those pages render three ways from one source. On GitHub, as the files themselves. In `cargo doc`, because each is pulled into the crate that implements it with `#![doc = include_str!]`. And as a book:

```bash
cargo install mdbook --locked        # once
cargo run -p choir-cli -- docs --open
```

That builds `book/`, with the API documentation inside it at `book/api/`, so a link from the prose to a type resolves. The book's palette is generated from the node's own stylesheet, so the documentation and the product look like one thing.

| You want | Read |
|:--|:--|
| How the pieces fit | [Architecture](docs/architecture.md) |
| To run a node | [Running a node](docs/operating/running-a-node.md) |
| ACLs, ownership, key rotation, invites | [Authorization](docs/operating/authorization.md) |
| Rate limits, quotas, sequencer fairness | [Limits](docs/operating/limits.md) |
| Ref-landed webhooks | [Webhooks](docs/operating/webhooks.md) |
| The decision journal and `choir repair` | [Observability and repair](docs/operating/observability.md) |
| SSH, the browser page, repository browsing | [Transports](docs/operating/transports.md) |
| Every command and endpoint | [The CLI and HTTP API](docs/using/cli.md) |
| To get a change reviewed and landed | [The contribution workflow](docs/using/workflow.md) |
| Something is broken | [Troubleshooting](docs/reference/troubleshooting.md) · [`ERRORS.md`](ERRORS.md) |
| To sync a log without trusting the node | [`SYNC.md`](SYNC.md) |
| Why a decision was made the way it was | [`DECISIONS.md`](DECISIONS.md) |
| To wire a coding agent | [`templates/`](templates/README.md) · [`agents.md`](agents.md) |
| To prepare a private beta | [Private beta runbook](docs/private-beta-runbook.md) |

---

## License

Workspace crates: [`MIT`](LICENSE-MIT) OR [`Apache-2.0`](LICENSE-APACHE), at your option. Mergiraf (optional subprocess) is GPLv3 and is never linked, only executed.

Third-party attributions are in [`NOTICE`](NOTICE); everything not listed there is original to this project.

<br/>

<div align="center">
<sub>a conflict is a value, never a failure</sub>
</div>
