# Observability and repair

Three records, answering three different questions, and not one of them is
the op log:

| Record | Question it answers | Flag |
|:--|:--|:--|
| Request log | what was *asked* of the node | `--request-log` (D33, in `docs/operating/limits.md`) |
| Decision journal | what the sequencer *decided*, and why | `--journal` |
| Torn-tail sidecar | what was being written when the node died | automatic |

All three are derived data. Nothing replays them, no hash covers them, and
losing every one of them changes no decision the node has made. That is what
licenses their cheapest properties — an unsynced write, a dropped record
under load — and it is also why none of them substitutes for the op log.

## Decision journal (`--journal`)

The request log records what was *asked*. It cannot say what the sequencer *decided*, because an accepted op and a refused one are both a `200` on `POST /api/submit`. `--journal <file>` appends one JSON object per admission decision, plus the events that explain the decisions around it:

```bash
cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --keys-file ~/.choir/keys \
  --journal ~/.choir/decisions.jsonl
```

```json
{"format_version":1,"kind":"decision","actor_id":"8f3a…","workspace":"op/agent","op_type":"SetRef","decision":"accepted","reject_reason":null,"seq":41,"parent":"…","decision_latency_us":812}
```

Four kinds, every one a flat object carrying `kind`, so `jq 'select(.kind == "cas_failure")'` works with no schema to hand:

| `kind` | What it records |
|:--|:--|
| `decision` | Every accept and every refusal: the author when the policy identified one, the op type when it decoded one, the refusal verbatim, and the time from dequeue to decision |
| `queue_depth` | How many commands the writer drained in one wake-up: its own view of the backlog, sampled rather than continuous |
| `window_resize` | The speculative merge window moving, **with its cause**, so a shrinking window is attributable and not merely visible |
| `cas_failure` | A lost compare-and-swap, recorded separately from the rejection it also produces; "two writers raced this ref" is a fact about contention that a rejection count cannot separate from a client sending nonsense |

**It is derived data and nothing else.** No hash covers it, nothing replays it, and losing the whole file changes no decision the node has made. That is what licenses the two properties it has: the writing happens on its own thread, and a record is dropped rather than allowed to stall the writer. A journal that could block the single writer would be a durability barrier wearing an observability costume.

Without the flag nothing is recorded and nothing is *built*. Each event owns its strings, so constructing one only to discard it costs several allocations per op on the writer thread. Wiring the journal in without that guard moved the submit path from 174 allocations per op to 212, and `submit_path_allocation_budget` failed, which is that test doing its job.

## Repairing a log (`choir repair`)

A node killed mid-write leaves a partial final record. `FileLog::open` truncates that torn tail automatically, so a daemon comes back after a power cut without a human, but it copies the bytes to a `<log>.torn-<offset>` sidecar first, synced before the truncation is issued. Nothing in this workspace deletes log bytes.

Everything else is a tool, and the tool makes the operator choose the mode:

```bash
cargo run -p choir-cli -- repair ~/.choir/repos/.choir/ops.jsonl --verify
```

| Mode | What it does | Exit |
|:--|:--|:--|
| `--verify` | Walks the chain, reports the first bad record and how much was intact before it. Changes nothing, quarantines nothing. | `0` usable, `1` damaged |
| `--truncate-tail` | Only when the damage is a torn final record: quarantines those bytes, truncates to the last complete entry, syncs. | `0` repaired, `1` refused |
| neither, or both | Usage error. `repair` alone is never read as permission to modify a log. | `2` |

`--verify` reads the file with its own reader rather than opening it as a log, because opening a log repairs it. A verify built on `FileLog::open` would truncate the tail as a side effect and then report the file intact, having caused the change it failed to mention.

Damage anywhere but the tail is **refused, not patched**. Cutting the file back past a mid-log break would drop records already acknowledged to clients, so the tool prints restore-from-backup steps and exits `1`. Refusing also means quarantining nothing: there is no suffix there it would be safe to remove.
