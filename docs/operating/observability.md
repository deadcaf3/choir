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

## Metrics and what can be alerted on (`/metrics`)

`/metrics` is Prometheus text format, authenticated like every other
operational endpoint. It carries two kinds of number, and the difference
decides what an alert can say.

**Gauges** describe a state the node can read on demand:
`choir_ready`, `choir_log_verified`, `choir_sequencer_live`,
`choir_storage_writable`, `choir_free_disk_bytes`,
`choir_ref_disagreements`, and `choir_process_start_time_seconds`.

**Counters** describe what has happened, and only ever rise:
`choir_requests_total`, `choir_requests_unauthorized_total` (401 and
403), `choir_requests_throttled_total` (429),
`choir_requests_failed_total` (5xx, and any response the node failed to
finish writing), and `choir_request_duration_microseconds_total`. The
last two together are the pair an average-latency alert subtracts across
two scrapes; a single gauge could not carry that.

Counters are incremented for every served request whether or not
`--request-log` is enabled. Declining to keep a per-request record is a
privacy choice; it must not also be a decision to make the node
unalertable.

A scrape renders before it finishes, so it never counts itself. Every
scrape is one request behind, uniformly, which no rate can see.

### The rules themselves

`scripts/flip/choir-alerts.rules.yml` holds the expressions, in two
groups: `choir-node-state` off the gauges, `choir-node-traffic` off the
counters. Load it into Prometheus beside whatever the host exporter
already provides.

Two things about that file are deliberate. **Its thresholds are not
measured** - this node has served no beta traffic, so every window and
rate in it is a starting point, not an observation, and the file says so
at the top. And the latency rule is a **mean**, not a percentile: a sum
and a count cannot yield a p99, and since the gate's bound is a p99
under 100 ms, a mean above 100 ms is the strongest claim those two
numbers support.

`every_metric_these_alert_rules_name_is_one_the_node_exports` in
`crates/choir-node/tests/limits.rs` scrapes a live node and asserts every
`choir_*` name the rules read is one the node still exports. Renaming a
metric without editing the rules is otherwise silent: the rule fires
never, and a rule that never fires looks exactly like a rule with
nothing to report.

### The alerts a node cannot source

`docs/private-beta-runbook.md` requires nine critical alerts. Four come
from this endpoint: readiness failure (`choir_ready`), durability errors
(`choir_sequencer_live`), disk exhaustion (`choir_free_disk_bytes`), and
request spikes with latency breaches (the counters above). A fifth,
restart loops, is visible as `choir_process_start_time_seconds` moving.

The remaining four are outside the node by construction, and the
operator has to source them elsewhere rather than wait for a metric that
is not coming:

| Alert | Where it comes from |
|:--|:--|
| Inode exhaustion | the host's own exporter; the node counts bytes, not inodes |
| Certificate expiry inside 21 days | the reverse proxy, which is what holds the certificate |
| Backup age beyond 90 minutes | the pull timer, on the host that pulls |
| Staging promotion failures | the deployment path, not the running node |

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

## Taking a node with you (`--export`, D61)

```bash
cargo run -p choir-node -- --export ~/.choir/repos /tmp/choir-export
cargo run -p choir-node -- --verify-export /tmp/choir-export
cargo run -p choir-node -- --import /tmp/choir-export /srv/new-root
```

No network, no ssh, no interpreter, and it works on the machine the node is on. What comes out is a directory holding `ops.jsonl`, one `repos/<owner>/<name>.git.bundle` per repository, and a `manifest.json` carrying its own `format_version`. A repository nobody has pushed to is listed without a bundle rather than dropped, because git refuses to write a zero-ref bundle and losing the repository on every move would be the worse answer.

An export is a claim with two halves that can disagree, and `--verify-export` settles it: it folds the log into a view and requires every ref the view names to be in that repository's bundle at the same oid. The check runs in one direction. A bundle may carry refs the log does not name -- that is a push landing while the export ran, and it is counted and reported as *ahead* rather than hidden -- but a log naming a commit no bundle holds is refused, because restoring that produces a node whose view points at objects it does not have, and startup reconciliation answers it by retracting, in signed ops, exactly the refs being restored.

**No secret is ever in an export.** Four names are copied out of `.choir`, and then the finished directory is walked again and refused if it holds anything named `auth` or ending `.key` or `.pem` -- so the guarantee is a property of the output rather than of the care taken writing it. The node's signing key stays with the node; [Restoring from a backup](../runbook-restore.md) covers what its absence means. Policy files are absent too, and the manifest says so in a field: `--acl-file` and its siblings name paths anywhere on the host, so a root does not know where they are.

`--import` verifies first, then refuses a root that already holds a log or any repository the manifest names, because the thing being written over is the fallback. It places files and stops there: it writes no hook and no git config, since the daemon adopts a repository it finds under its root and a second copy of that rule would drift. It also does not claim the result works. A restored node that serves is not a restored node; what settles it is one that accepts a write, and the secrets that boot needs are deliberately not in an export. [Restoring from a backup](../runbook-restore.md) covers them.

This is not `scripts/flip/pull_backup.sh` and does not replace it. That script is disaster recovery for one deployment, over ssh, from a fixed remote path, and it refuses to run where the log lives. This is the format tool the plan's one-way-door rule asks for.
