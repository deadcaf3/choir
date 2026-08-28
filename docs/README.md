# choir documentation

Getting started lives in the [top-level README](../README.md). Everything
else is here, indexed by what you are trying to do.

Every page here is also compiled into the API documentation: each one is
pulled into the crate that implements it with `#![doc = include_str!]`, so
`cargo doc` renders this prose beside the types it describes. That is not a
convenience — it is what keeps these pages honest. The release gate runs
`cargo doc` with `-D warnings`, so a Rust example in any of these files that
stops compiling fails the build.

<a class="api-link" href="api/index.html">API documentation →</a>
<!--node-link-->

Build both halves together with `choir docs`, which renders the book and
puts rustdoc inside it at `/api/`. From a checkout without the CLI
installed: `cargo run -p choir-cli -- docs --open`.

The comment above is not stray markup. This book and a running node are
two halves of one site on two hosts, so neither can reach the other with
a relative link and neither may name the other in a tracked file. `choir
docs` replaces that marker with a link to the node when the publishing
workflow tells it the address, and with nothing when it does not, so a
local build renders exactly what you see here.

## Start here

| You want | Read |
|:--|:--|
| What this is and how the pieces fit | [Architecture](architecture.md) |
| To run a node | [Running a node](operating/running-a-node.md) |
| To use a node | [The CLI and HTTP API](using/cli.md) |
| To get a change reviewed and landed | [The contribution workflow](using/workflow.md) |
| Something is broken | [Troubleshooting](reference/troubleshooting.md) |

## Operating a node

| Page | Covers |
|:--|:--|
| [Running a node](operating/running-a-node.md) | Starting the daemon, every policy file, the supervised macOS install |
| [Authorization](operating/authorization.md) | ACLs (D29), repository ownership (D42), landing basis (D43), key rotation (D44), credential self-service (D36) |
| [Rate limits, quotas and fairness](operating/limits.md) | Request log and rate limits (D33), per-user quotas (D37), the sequencer's in-flight window |
| [Webhooks](operating/webhooks.md) | Outbound ref-landed deliveries (D32) |
| [Observability and repair](operating/observability.md) | The decision journal, `choir repair`, what each derived record is for |
| [Transports and the browser surface](operating/transports.md) | Git over HTTPS and SSH (D31), the read-only page (D28), repository browsing (D30) |

Two runbooks sit beside these, for the two operations that are procedures
rather than configuration:

- [Private single-node beta runbook](private-beta-runbook.md) — the network
  hold, the TLS proxy, backups, staging promotion, go-live receipts.
- [Restoring a node from a backup](runbook-restore.md) — the ordering rule,
  and the secrets a backup deliberately never holds.
- [Canonical-node flip runbook](../scripts/flip/RUNBOOK.md) — supervised
  install and turning on the protected-ref gates.

## Using a node

| Page | Covers |
|:--|:--|
| [The CLI and HTTP API](using/cli.md) | Every command and endpoint, generated from one table |
| [The contribution workflow](using/workflow.md) | Workspace to landed ref, and the review rules that bite in practice |
| [Agent templates](../templates/README.md) | Drop-in harness snippets for Claude Code, Codex and Cursor |
| [`agents.md`](../agents.md) | The same surface written for an agent that has never seen choir |

## Reference

| Page | Covers |
|:--|:--|
| [Troubleshooting](reference/troubleshooting.md) | Symptoms, causes, fixes |
| [`ERRORS.md`](../ERRORS.md) | Every rejection code and its repair hint — generated from the node's own table |
| [`SYNC.md`](../SYNC.md) | Catching up on a log, and verifying a page's hash chain and signatures without trusting the node that served it |
| [`DECISIONS.md`](../DECISIONS.md) | What each `D<n>` in the code means, and which are one-way doors |
| [Bridge permissions](../crates/choir-bridge/PERMISSIONS.md) | The minimum GitHub App grants, and what must not be granted |

## Which of these is generated

Editing a generated file by hand is wasted work: the release gate compares
it against its source and fails when they differ. Regenerate with
`cargo run -p choir-cli --example gen-surface`.

| Generated | Source |
|:--|:--|
| The surface block in [using/cli.md](using/cli.md) | `crates/choir-cli/src/surface.rs` |
| `theme/choir-tokens.css`, the book's palette | `crates/choir-node/src/ui.css` |
| The cheat-sheet block in [the README](../README.md) | `crates/choir-cli/src/surface.rs` |
| [`agents.md`](../agents.md), `/llms.txt`, `/api/schema` | `crates/choir-cli/src/surface.rs` |
| [`ERRORS.md`](../ERRORS.md) | `crates/choir-node/src/reject.rs` |
| The command lists in [`templates/`](../templates/README.md) | `crates/choir-cli/src/surface.rs` |

Everything else on this page is written by hand and is fair game to edit.
