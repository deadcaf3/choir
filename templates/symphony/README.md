# Symphony workspace backend

`choir-workspace-backend.sh` is the Choir side of a real Symphony workspace
backend. It creates or reuses an exact-base Choir workspace, checkpoints a
committed revision, and recoverably archives the workspace. It keeps tracker
issue identity, worker run identity, stable change identity, and immutable
revision identity separate.

This contract is pinned to OpenAI Symphony specification commit
[`f8e8b8a670c799f6e0ade7a8c25c4bf4a4a56ec7`](https://github.com/openai/symphony/blob/f8e8b8a670c799f6e0ade7a8c25c4bf4a4a56ec7/SPEC.md).
That reference implementation has a concrete filesystem workspace manager,
not a configurable backend seam. Its `after_create` hook cannot return a
different path, and `before_remove` failures do not stop filesystem deletion.
Do not install this adapter as lifecycle hooks. A Symphony fork must make the
backend authoritative for both creation and removal.

## Symphony-side seam

The workspace manager should send one JSON request on stdin, read one JSON
response from stdout, and treat a nonzero exit as a structured failure. The
adapter performs one attempt and never sleeps. Symphony retains ownership of
dispatch retries and backoff.

The minimum calls are:

```json
{
  "protocol_version": 1,
  "operation": "ensure",
  "issue": {"id": "tracker-stable-id", "identifier": "ABC-123"},
  "workspace_key": "ABC-123",
  "generation": "first-change",
  "run_id": "worker-attempt-7"
}
```

```json
{
  "protocol_version": 1,
  "operation": "checkpoint",
  "issue": {"id": "tracker-stable-id", "identifier": "ABC-123"},
  "workspace_key": "ABC-123",
  "generation": "first-change",
  "run_id": "worker-attempt-7",
  "workspace_path": "/absolute/path/returned/by/ensure"
}
```

Use the same shape with `"operation": "archive"` for terminal cleanup. The
`generation` belongs to the logical contribution, not to the issue or worker
attempt. Replacement workers reuse it. A reopened issue starts a new
generation, which produces a new Choir `change_id` and archive destination
while preserving Symphony's deterministic issue key.

The seam must preserve these rules:

1. Launch the coding agent at the exact absolute path returned by `ensure`.
2. Store the returned binding outside the mutable workspace for restart cleanup.
3. Call `archive` instead of deleting the directory. If archive fails, preserve
   the live path and expose the failure for Symphony to retry.
4. Call `checkpoint` only as a strict finalize step. Symphony's current
   `after_run` hook ignores failures, so it is not a sufficient checkpoint gate.
5. Keep Symphony's collision-safe `workspace_key` derivation and root safety
   checks before invoking the backend.

## Install and configure

The adapter requires `bash`, `jq`, Git, the `choir` CLI, and a Choir node on the
same host or a shared filesystem mount. Install it and a mode-0600 config
outside repositories:

```bash
install -d "$HOME/.choir/bin"
install -m 0755 templates/symphony/choir-workspace-backend.sh \
  "$HOME/.choir/bin/choir-symphony-workspace"
install -m 0600 templates/symphony/config.example.json \
  "$HOME/.choir/symphony.json"
```

Edit paths and identities in the copied config. The same file is read by
`choir runner`, which owns identity derivation and every call to the node;
this adapter only translates Symphony's request and result shapes.

`base_ref` is the exact namespaced ref from `choir view`, such as
`owner/repo.git:refs/heads/main`. It is resolved when a generation is first
created. A later retry that resolves a ref which has since moved still reports
the base the change is actually bound to, because the node reports it and the
adapter does not second-guess that answer.

`namespace` separates this orchestrator's identifiers from every other one
driving the same repository. Changing it after work is in flight derives
different change ids, so treat it as fixed at install time.

The adapter keeps no durable state of its own. Convergence between competing
schedulers comes from Choir's idempotency key rather than from a lock on one
machine, so two of them racing the same `ensure` reuse one workspace instead of
forking the work. Non-secret binding metadata is written inside the workspace
at `.git/choir/symphony-backend.json` and is used to refuse a `checkpoint` or
`archive` aimed at a directory bound to a different change. Keep key and HTTP
credential values in their referenced mode-0600 files, never in the config or
repository.

## Checkpoint and recovery behavior

`checkpoint` resolves the workspace's committed `HEAD`, pushes it to the
immutable `refs/choir/revisions/<oid>` ref, then submits Choir's signed revision
CAS. Git authentication must already be configured for the workspace origin.
A stale checkpoint fails and must be reconciled; the adapter never blind-retries
a CAS conflict.

Exact create and archive retries converge on Choir's original operation
receipt. A `workspace_state`, malformed request, unknown key, or channel
ownership rejection is non-retryable until configuration is reconciled.
Transport and other availability failures are marked retryable for Symphony's
scheduler. The adapter forwards the runner's classification unchanged rather
than forming a second opinion about it.

Current limits:

- Remote Symphony SSH workers need a shared node path.
- The reference Symphony checkout still needs the workspace-manager seam
  described above. This repository does not claim hook-only compatibility.
- A workspace whose `.git/choir/symphony-backend.json` is lost cannot be
  checkpointed or archived through this adapter. Recover it from the binding
  Symphony was told to store, or archive the change directly with the CLI.
