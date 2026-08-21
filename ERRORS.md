# Rejection codes

Generated from `crates/choir-node/src/reject.rs`. Do not edit; edit the table.

Every rejection body carries `code`, `error` and `next`. `expected` and `actual` are present when the check compared two states — a compare-and-swap failure, or a channel bound to a name other than the one used.

`code` is the contract: branch on it, not on `error`. Adding a code is not a breaking change; renaming one is.

| Code | Meaning | What to do |
|---|---|---|
| `unknown_key` | The signature names a key id this node has no record of | Ask the operator to register your public key. `choir key <file> <you>` prints the line; it takes effect on the next request. |
| `malformed_op` | The payload did not decode as a `ViewOp` | Serialize a `ViewOp` and sign its bytes. `choir submit` does this correctly; `GET /llms.txt` lists the operations. |
| `malformed_request` | The request body was missing fields or badly encoded | Send a JSON object with the fields the endpoint wants. `GET /llms.txt` lists them. |
| `reviewer_mismatch` | A verdict or comment claimed an attribution other than the signed channel | Resubmit on your own channel. `choir verdict` and `choir comment` sign on the attribution name by construction, so use them rather than hand-rolling. |
| `channel_not_owned` | The signing key is bound to a different channel name | Submit on the channel your key is bound to — it is in `expected`. Or ask the operator to bind a key to the channel you want. |
| `node_only` | Only the node's own key may author this operation | Nothing to retry: this operation is the node's to author. For reviewer assignment, request a review with an empty reviewer list. |
| `assignment_required` | This node assigns reviewers; a self-named list was refused | Resubmit with an empty reviewer list. The node draws reviewers and returns their names in the response. |
| `protected_ref` | The target ref is protected and needs a node-drawn reviewer list | Resubmit with an empty reviewer list. On a protected ref only a node-drawn list is accepted. |
| `review_required` | The target ref lacks the required independent approval weight | Open a review naming this ref and commit (`choir review ... --ref <repo:ref>`), obtain approvals from two distinct operators, then push again. |
| `ref_undeletable` | The target ref is protected and cannot be deleted | Do not delete this ref, or ask the operator to remove it from the protected-ref list. |
| `stale_head` | Compare-and-swap failed: the state moved under the submission | Re-read `GET /api/view`, rebase your intent on the value in `actual`, and resubmit with that as `prev`. If you are retrying a submission whose response you lost, check for `already_applied` first — a completed retry answers 200, not this. |
| `review_state` | A review-op precondition failed (duplicate id, unknown review, already assigned, archived) | Read `reviews` in `GET /api/view` for this id. A review that is already assigned, complete, or archived does not accept the op you sent. |
| `provenance_state` | A provenance record was missing a subject or kind | Resubmit with a non-empty subject and kind. |
| `change_state` | A stable change was unknown, duplicated, archived, or mismatched | Read `changes` in `GET /api/view`, then use its owner, workspace and revision or choose a new change id. |
| `workspace_state` | A workspace lifecycle request conflicted with its durable binding | Read `changes` and `workspaces` in `GET /api/view`; retry only with the exact existing binding, or choose a new workspace name. |
| `identity_state` | A key-binding precondition failed (key already bound to another operator, revoked or unbound key, channel naming a different operator) | Read `bindings` in `GET /api/view` for this key. Not a retry: a key belongs to one operator for the life of the key, and a revoked key is never rebindable. Bind a fresh key instead. `error` names which of the two applies. |
| `vouch_state` | A vouch precondition failed (an end with no unrevoked key binding, a self-vouch, an edge that already stands, or a withdrawal of one that does not) | Read `vouches` in `GET /api/view`. Both ends of a vouch must be operators with an unrevoked key bound in the log, so if `error` names an unbound end the repair is the operator's: `choir bind`. An edge that already stands is not a retry — withdraw it and vouch again if the note should change. |
| `policy_unavailable` | The operator's protected-ref list could not be read, so the gate failed closed | Operator problem, not a client one: the gate fails closed rather than guessing. Retry once the file is restored. |
| `log_evicted` | Requested log entries are older than anything this node can serve | Resync from the sequence in `window_base`; entries before it are gone from this node. |
| `duplicate_submission` | These exact signed bytes already landed; a signature is admissible once | If you are retrying, this is your op: read `seq`. A submission that already landed answers 200 with `already_applied`, and only reaches you as a rejection if the window moved underneath the retry. If you meant a second, distinct change, sign a new op — two otherwise byte-identical ops are told apart by their scope. |
| `scope_required` | This node admits only ops signed for its own log and a recent head, and this op carried no scope | Read `log.node` and `log.head` from `GET /api/view`, put them in the op's `scope`, and sign that. `choir submit` does this automatically. An unscoped op cannot be admitted here because nothing in it says which log it was meant for or that it has not run before. |
| `foreign_scope` | The op was signed for another node's log | Nothing to retry against this node: the op names another node's id in `expected`. Sign a scope naming this node, whose id is in `actual` and in `log.node` of `GET /api/view`. |
| `stale_scope` | The head the op was signed against is no longer in the node's recent window | Re-read `log.head` from `GET /api/view` and sign a fresh op against it. A signature is only admissible while the head it names is still in the node's window, which is what stops a captured op from being replayed later. |
| `quota_exceeded` | A per-user quota was already full, or this request was larger than one is allowed to be | Not a retry: retrying the same request gets the same answer. `expected` names the ceiling and `actual` what you asked for. For a push, send fewer objects — several smaller pushes, or a shallower history. For a workspace, archive one you are finished with (`POST /api/workspace/archive`) to free the allowance. If neither is possible, the ceiling is the operator's to raise. |
| `bad_signature` | The signature does not verify over these bytes, under a key this node does trust | Re-sign the exact bytes you are submitting: a signature covers one `(channel, payload)` pair and does not carry to another. Registering a key does not help here, the key this names is already trusted. If you did not send this, a signature of yours was replayed onto bytes you never signed, and the operator wants to know. |
| `unclassified` | A rejection that did not originate as a structured one | Read `error`. This path does not name a repair yet — that is a gap, and worth reporting. |

## Retrying a submission whose response you lost

Resubmit the identical signed bytes. If it already landed, the node answers **200** with `already_applied: true` and the original `seq` and `hash`, rather than the compare-and-swap failure the two cases would otherwise share. Read those back and proceed; do not rebuild the operation.

This is bounded to recent history: the node keeps the index over the same window `GET /api/log` serves from. A retry seconds or minutes later is covered; one after the window has turned over reads as `stale_head`, which is the safe direction.

