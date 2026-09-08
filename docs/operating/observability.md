# Observability and repair

| Record | Question it answers | Flag |
|:--|:--|:--|
| Request log | what was *asked* of the node | `--request-log` (D33, in `docs/operating/limits.md`) |
| Decision journal | what the sequencer *decided*, and why | `--journal` |
| Torn-tail sidecar | what was being written when the node died | automatic |

All three are derived data: unsynced writes, dropped under load.

> [!WARNING]
> Keep the op log. None of these substitutes for it.

## Metrics and what can be alerted on (`/metrics`)

Prometheus text format, authenticated.

**Gauges**: `choir_ready`, `choir_log_verified`, `choir_sequencer_live`,
`choir_storage_writable`, `choir_free_disk_bytes`,
`choir_ref_disagreements`, `choir_process_start_time_seconds`.

**Counters**: `choir_requests_total`, `choir_requests_unauthorized_total`
(401 and 403), `choir_requests_throttled_total` (429),
`choir_requests_failed_total` (5xx and unfinished responses),
`choir_request_duration_microseconds_total`. Counters increment whether or
not `--request-log` is on. Every scrape is one request behind.

### The rules themselves

`scripts/flip/choir-alerts.rules.yml`: `choir-node-state` off the gauges,
`choir-node-traffic` off the counters. Every window and rate is a starting
point. The latency rule is a **mean**.

`every_metric_these_alert_rules_name_is_one_the_node_exports` in
`crates/choir-node/tests/limits.rs` checks every `choir_*` name the rules
read is still exported.

### The alerts a node cannot source

Of the nine critical alerts in `docs/private-beta-runbook.md`, five come
from this endpoint: readiness (`choir_ready`), durability
(`choir_sequencer_live`), disk (`choir_free_disk_bytes`), request spikes
and latency (the counters), restart loops
(`choir_process_start_time_seconds`).

| Alert | Where it comes from |
|:--|:--|
| Inode exhaustion | the host's own exporter |
| Certificate expiry inside 21 days | the reverse proxy |
| Backup age beyond 90 minutes | the pull timer, on the host that pulls |
| Staging promotion failures | the deployment path |

## Decision journal (`--journal`)

Accepted and refused ops are both `200` on `POST /api/submit`; the journal
records the decision:

```bash
cargo run -p choir-node -- /tmp/choir-repos 8417 \
  --keys-file ~/.choir/keys \
  --journal ~/.choir/decisions.jsonl
```

```json
{"format_version":1,"kind":"decision","actor_id":"8f3a…","workspace":"op/agent","op_type":"SetRef","decision":"accepted","reject_reason":null,"seq":41,"parent":"…","decision_latency_us":812}
```

Every record carries `kind`:

| `kind` | What it records |
|:--|:--|
| `decision` | every accept and refusal, with author, op type, reason, and dequeue-to-decision time |
| `queue_depth` | commands drained per writer wake-up |
| `window_resize` | the speculative merge window moving, with its cause |
| `cas_failure` | a lost compare-and-swap, separate from its rejection |

Derived data ([Architecture](../architecture.md)): written on its own
thread, dropped rather than stalling the writer. The flag gates
construction too.

## Repairing a log (`choir repair`)

`FileLog::open` truncates a torn final record automatically, after saving
the bytes to `<log>.torn-<offset>`. Everything else is explicit:

```bash
cargo run -p choir-cli -- repair ~/.choir/repos/.choir/ops.jsonl --verify
```

| Mode | What it does | Exit |
|:--|:--|:--|
| `--verify` | Walks the chain, reports the first bad record. Read-only. | `0` usable, `1` damaged |
| `--truncate-tail` | Only for a torn final record: quarantines, truncates, syncs. | `0` repaired, `1` refused |
| neither, or both | Usage error. | `2` |

Damage anywhere but the tail is **refused**: the tool prints
restore-from-backup steps and exits `1`.

## Taking a node with you (`--export`, D61)

```bash
cargo run -p choir-node -- --export ~/.choir/repos /tmp/choir-export
cargo run -p choir-node -- --verify-export /tmp/choir-export
cargo run -p choir-node -- --import /tmp/choir-export /srv/new-root
```

Offline, on the node's machine. Writes `ops.jsonl`, one
`repos/<owner>/<name>.git.bundle` per repository, and `manifest.json` with
its own `format_version`. A never-pushed repository is listed without a
bundle.

`--verify-export` requires every ref the log names to be in a bundle at the
same oid. Extra refs in a bundle are reported as *ahead*; a log naming a
commit no bundle holds is refused.

**An export is secret-free by construction**: the output is walked and
refused if it holds anything named `auth` or ending `.key` or `.pem`. The
signing key stays with the node ([Restoring from a
backup](../runbook-restore.md)). Policy files stay behind; the manifest
records that.

`--import` verifies, then refuses a root holding a log or any named
repository. It places files; the daemon adopts them. Settle a restore by
accepting a write.

`scripts/flip/pull_backup.sh` is the disaster-recovery path over ssh;
`--export` is the format tool beside it.
