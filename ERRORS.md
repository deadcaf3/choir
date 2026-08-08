# Rejection codes

Generated from `crates/choir-node/src/reject.rs`. Do not edit; edit the table.

Every rejection body carries `code`, `error` and `next`. `expected` and `actual` are present when the check compared two states — a compare-and-swap failure, or a channel bound to a name other than the one used.

`code` is the contract: branch on it, not on `error`. Adding a code is not a breaking change; renaming one is.

| Code | Meaning | What to do |
|---|---|---|
| `unknown_key` | The submission was not signed by a key this node trusts | Ask the operator to register your public key. `choir key <file> <you>` prints the line; it takes effect on the next request. |
| `malformed_op` | The payload did not decode as a `ViewOp` | Serialize a `ViewOp` and sign its bytes. `choir submit` does this correctly; `GET /llms.txt` lists the operations. |
| `malformed_request` | The request body was missing fields or badly encoded | Send a JSON object with the fields the endpoint wants. `GET /llms.txt` lists them. |
| `reviewer_mismatch` | A verdict claimed a reviewer other than the signed channel | Resubmit on your own channel. `choir verdict` signs on the reviewer name by construction, so use it rather than hand-rolling. |
| `channel_not_owned` | The signing key is bound to a different channel name | Submit on the channel your key is bound to — it is in `expected`. Or ask the operator to bind a key to the channel you want. |
| `node_only` | Only the node's own key may author this operation | Nothing to retry: this operation is the node's to author. For reviewer assignment, request a review with an empty reviewer list. |
| `assignment_required` | This node assigns reviewers; a self-named list was refused | Resubmit with an empty reviewer list. The node draws reviewers and returns their names in the response. |
| `protected_ref` | The target ref is protected and needs a node-drawn reviewer list | Resubmit with an empty reviewer list. On a protected ref only a node-drawn list is accepted. |
| `review_required` | The target ref is protected and no approved review authorises this commit | Open a review naming this ref and commit (`choir review ... --ref <repo:ref>`), get it approved, then push again. |
| `ref_undeletable` | The target ref is protected and cannot be deleted | Do not delete this ref, or ask the operator to remove it from the protected-ref list. |
| `stale_head` | Compare-and-swap failed: the state moved under the submission | Re-read `GET /api/view`, rebase your intent on the value in `actual`, and resubmit with that as `prev`. If you are retrying a submission whose response you lost, check for `already_applied` first — a completed retry answers 200, not this. |
| `review_state` | A review-op precondition failed (duplicate id, unknown review, already assigned, archived) | Read `reviews` in `GET /api/view` for this id. A review that is already assigned, complete, or archived does not accept the op you sent. |
| `provenance_state` | A provenance record was missing a subject or kind | Resubmit with a non-empty subject and kind. |
| `policy_unavailable` | The operator's protected-ref list could not be read, so the gate failed closed | Operator problem, not a client one: the gate fails closed rather than guessing. Retry once the file is restored. |
| `log_evicted` | Requested log entries are older than anything this node can serve | Resync from the sequence in `window_base`; entries before it are gone from this node. |
| `unclassified` | A rejection that did not originate as a structured one | Read `error`. This path does not name a repair yet — that is a gap, and worth reporting. |

## Retrying a submission whose response you lost

Resubmit the identical signed bytes. If it already landed, the node answers **200** with `already_applied: true` and the original `seq` and `hash`, rather than the compare-and-swap failure the two cases would otherwise share. Read those back and proceed; do not rebuild the operation.

This is bounded to recent history: the node keeps the index over the same window `GET /api/log` serves from. A retry seconds or minutes later is covered; one after the window has turned over reads as `stale_head`, which is the safe direction.

