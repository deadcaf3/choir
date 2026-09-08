# choir documentation

Getting started is in the [top-level README](../README.md). Everything else
is here, indexed by task.

Each page is also pulled into the crate that implements it with
`#![doc = include_str!]`, and the release gate runs `cargo doc -D warnings`,
so a Rust example here that stops compiling fails the build.

<a class="api-link" href="api/index.html">API documentation →</a>
<!--node-link-->

`choir docs` builds the book with rustdoc inside it at `/api/`. From a
checkout: `cargo run -p choir-cli -- docs --open`. The comment above is a
marker `choir docs` replaces with a link to the node (D76).

## Start here

| You want | Read |
|:--|:--|
| What this is | [Why choir exists](why.md) |
| How the pieces fit | [Architecture](architecture.md) |
| To run a node | [Running a node](operating/running-a-node.md) |
| To use a node | [The CLI and HTTP API](using/cli.md) |
| To get a change reviewed and landed | [The contribution workflow](using/workflow.md) |
| Something is broken | [Troubleshooting](reference/troubleshooting.md) |

## Operating a node

| Page | Covers |
|:--|:--|
| [Running a node](operating/running-a-node.md) | Flags, policy files, supervised install |
| [Authorization](operating/authorization.md) | ACLs (D29), ownership (D42), landing basis (D43), key rotation (D44), self-service (D36), publishing (D78) |
| [Rate limits, quotas and fairness](operating/limits.md) | Request log, rate limits (D33), quotas (D37), in-flight window |
| [Webhooks](operating/webhooks.md) | Ref-landed deliveries (D32) |
| [Observability and repair](operating/observability.md) | Decision journal, `choir repair`, derived records |
| [Transports and the browser surface](operating/transports.md) | Git over HTTPS and SSH (D31), read-only page (D28), browsing (D30) |

Runbooks:

- [Private single-node beta runbook](private-beta-runbook.md): network
  hold, TLS proxy, backups, staging promotion, go-live receipts.
- [Restoring a node from a backup](runbook-restore.md): ordering rule, and
  the secrets a backup never holds.
- [Canonical-node flip runbook](../scripts/flip/RUNBOOK.md): supervised
  install, protected-ref gates.

## Using a node

| Page | Covers |
|:--|:--|
| [The CLI and HTTP API](using/cli.md) | Every command and endpoint, generated from one table |
| [The contribution workflow](using/workflow.md) | Workspace to landed ref, review rules |
| [Agent templates](../templates/README.md) | Snippets for Claude Code, Codex and Cursor |
| [`AGENTS.md`](../AGENTS.md) | The surface, written for an agent |

## Reference

| Page | Covers |
|:--|:--|
| [Troubleshooting](reference/troubleshooting.md) | Symptoms, causes, fixes |
| [`ERRORS.md`](../ERRORS.md) | Every rejection code and repair hint, generated |
| [`SYNC.md`](../SYNC.md) | Catching up on a log and verifying a served page |
| [`DECISIONS.md`](../DECISIONS.md) | What each `D<n>` means, and which are one-way doors |
| [Bridge permissions](../crates/choir-bridge/PERMISSIONS.md) | Minimum GitHub App grants |

## Which of these is generated

The gate fails when a generated file differs from its source. Regenerate
with `cargo run -p choir-cli --example gen-surface`.

| Generated | Source |
|:--|:--|
| The surface block in [using/cli.md](using/cli.md) | `crates/choir-cli/src/surface.rs` |
| `theme/choir-tokens.css` | `crates/choir-node/src/ui.css` |
| The cheat-sheet block in [the README](../README.md) | `crates/choir-cli/src/surface.rs` |
| [`AGENTS.md`](../AGENTS.md), `/llms.txt`, `/api/schema` | `crates/choir-cli/src/surface.rs` |
| [`ERRORS.md`](../ERRORS.md) | `crates/choir-node/src/reject.rs` |
| The command lists in [`templates/`](../templates/README.md) | `crates/choir-cli/src/surface.rs` |

Everything else on this page is hand-written.
