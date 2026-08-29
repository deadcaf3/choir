# Observability and repair

Three records, each answering a different question, all separate from the op
log:

| Record | Question it answers | Flag |
|:--|:--|:--|
| Request log | what was *asked* of the node | `--request-log` (D33, in `docs/operating/limits.md`) |
| Decision journal | what the sequencer *decided*, and why | `--journal` |
| Torn-tail sidecar | what was being written when the node died | automatic |

All three are derived data, which licenses an unsynced write and a
dropped record under load.

> [!WARNING]
> Keep the op log. None of these three substitutes for it.

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
finish writing), and `choir_request_duration_microseconds_total`. An
average-latency alert subtracts the last two across two scrapes.

Counters increment for every served request whether or not
`--request-log` is enabled, so a node that keeps no per-request record
stays alertable.

A scrape renders before it finishes, so every scrape is one request
behind, uniformly.

### The rules themselves

`scripts/flip/choir-alerts.rules.yml` holds the expressions, in two
groups: `choir-node-state` off the gauges, `choir-node-traffic` off the
counters. Load it into Prometheus beside the host exporter's own rules.

**Every window and rate in it is a starting point rather than an
observation**, and the file says so at the top. The latency rule is a
**mean**: the gate's bound is a p99 under 100 ms, so a mean above 100 ms
is the strongest claim a sum and a count support.

`every_metric_these_alert_rules_name_is_one_the_node_exports` in
`crates/choir-node/tests/limits.rs` asserts every `choir_*` name the
rules read is one the node still exports, so a rename that misses the
rules fails the test.

### The alerts a node cannot source

`docs/private-beta-runbook.md` requires nine critical alerts. Four come
from this endpoint: readiness failure (`choir_ready`), durability errors
(`choir_sequencer_live`), disk exhaustion (`choir_free_disk_bytes`), and
request spikes with latency breaches (the counters above). A fifth,
restart loops, is visible as `choir_process_start_time_seconds` moving.

Source the remaining four elsewhere:

| Alert | Where it comes from |
|:--|:--|
| Inode exhaustion | the host's own exporter; the node counts bytes, not inodes |
| Certificate expiry inside 21 days | the reverse proxy, which is what holds the certificate |
| Backup age beyond 90 minutes | the pull timer, on the host that pulls |
| Staging promotion failures | the deployment path, not the running node |

## Decision journal (`--journal`)

An accepted op and a refused one are both a `200` on `POST /api/submit`, so the journal is what records the sequencer's *decision*. `--journal <file>` appends one JSON object per admission decision, plus the events that explain the decisions around it:

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
| `window_resize` | The speculative merge window moving, **with its cause**, so a shrinking window is attributable |
| `cas_failure` | A lost compare-and-swap, recorded separately from the rejection it also produces, so contention is distinguishable from a client sending nonsense |

**It is derived data** ([Architecture](../architecture.md)): writing happens on its own thread, and a record is dropped rather than stalling the writer.

The flag gates construction as well as writing. Building an event to discard it took the submit path from 174 to 212 allocations per op and failed `submit_path_allocation_budget`.

## Repairing a log (`choir repair`)

A node killed mid-write leaves a partial final record. `FileLog::open` truncates that torn tail automatically, so a daemon comes back from a power cut without a human. The bytes go to a `<log>.torn-<offset>` sidecar first, synced before the truncation, so every log byte survives.

Everything else is a tool, and it makes the operator choose the mode:

```bash
cargo run -p choir-cli -- repair ~/.choir/repos/.choir/ops.jsonl --verify
```

| Mode | What it does | Exit |
|:--|:--|:--|
| `--verify` | Walks the chain, reports the first bad record and how much was intact before it. Read-only. | `0` usable, `1` damaged |
| `--truncate-tail` | Only when the damage is a torn final record: quarantines those bytes, truncates to the last complete entry, syncs. | `0` repaired, `1` refused |
| neither, or both | Usage error: the mode is always explicit. | `2` |

`--verify` reads the file with its own reader rather than opening it as a log, because opening a log repairs it.

Damage anywhere but the tail is **refused, not patched**: cutting back past a mid-log break would drop records already acknowledged to clients. The tool prints restore-from-backup steps and exits `1`.

## Taking a node with you (`--export`, D61)

```bash
cargo run -p choir-node -- --export ~/.choir/repos /tmp/choir-export
cargo run -p choir-node -- --verify-export /tmp/choir-export
cargo run -p choir-node -- --import /tmp/choir-export /srv/new-root
```

It runs offline, on the machine the node is on, and writes a directory holding `ops.jsonl`, one `repos/<owner>/<name>.git.bundle` per repository, and a `manifest.json` carrying its own `format_version`. A repository nobody has pushed to is listed without a bundle, since git refuses to write a zero-ref bundle.

`--verify-export` folds the log into a view and requires every ref the view names to be in that repository's bundle at the same oid. A bundle carrying refs the log does not name is a push that landed while the export ran, counted and reported as *ahead*. A log naming a commit no bundle holds is refused.

**An export is secret-free by construction.** Four names are copied out of `.choir`, and the finished directory is then walked again and refused if it holds anything named `auth` or ending `.key` or `.pem`: the guarantee is a property of the output. The node's signing key stays with the node; [Restoring from a backup](../runbook-restore.md) covers what its absence means. Policy files stay behind too, and the manifest records that in a field: `--acl-file` and its siblings name paths anywhere on the host.

`--import` verifies first, then refuses a root that already holds a log or any repository the manifest names. It places files and stops there; the daemon adopts a repository it finds under its root. Settle a restore by accepting a write, and read [Restoring from a backup](../runbook-restore.md) for the secrets a boot needs.

`scripts/flip/pull_backup.sh` stays the disaster-recovery path for one deployment: over ssh, from a fixed remote path, and it refuses to run where the log lives. `--export` is the format tool beside it.
