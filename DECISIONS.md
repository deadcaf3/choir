# Decision register

Code comments across this workspace cite decisions by number (`D6`, `D29`,
`D41`). This table is what those numbers mean.

Each row records **what** was decided and whether it is a **one-way door** — a
choice that is a data migration to reverse rather than a refactor. One-way rows
are the ones to read before changing anything a comment tags.

The rationale behind each decision, the fallback chosen for it, and the tripwire
that would reverse it are not published.

| # | Decision | Door |
|---|---|---|
| D1 | Op-log / workspaces as the version model (L1) | One-way-leaning |
| D2 | Single-writer per-repo sequencer | Two-way |
| D3 | Rivet Actors as the actor runtime | Two-way |
| D4 | Mergiraf for structured merge, subprocess only | Two-way |
| D5 | Speculative merge queue with a TCP-like window | Two-way |
| D6 | BLAKE3 + FastCDC content addressing (L0) | **One-way** |
| D7 | S3-API object store, no vendor extensions | Two-way |
| D8 | Firecracker microVM + copy-on-write provisioning | Two-way |
| D9 | Key-per-actor identity and signed-ref format (L8) | **One-way** (trust root) |
| D10 | Signed CRDT record schemas for collaboration objects (L9) | **One-way** once exported |
| D11 | Content-addressed provenance format (L5) | **One-way** (append-only audit) |
| D12 | Forked knot daemon for the node (L3) | Two-way |
| D13 | Zoekt for code search (L7) | Two-way |
| D14 | Centralized transport now, P2P later (L10) | Gated |
| D15 | CRDTs for collaboration objects only, never for code | Two-way |
| D16 | Append-only log now, witnessed later | Gated |
| D17 | Typed, self-describing API as the agent SDK surface | One-way-leaning |
| D18 | Nix-based CI with a shared remote build cache (L6) | Two-way |
| D19 | LLM auto-resolve limited to low-risk hunks, never silent | Two-way |
| D20 | Dogfood: the canonical repo migrates onto the platform | Two-way |
| D21 | Mirror-first forge bridge as the adoption surface | Two-way |
| D22 | Provenance-conditioned merging as the research bet | Two-way |
| D23 | CI green is necessary but never sufficient to land; the queue adds differential tests | Two-way |
| D24 | Operator-anchored trust and Sybil resistance | Two-way |
| D25 | Signed ref-state snapshot (`RefSnapshot`) as the attestation unit | **One-way** once written |
| D26 | A signature pins position and log identity, not just `(channel, payload)` | **One-way-leaning** |
| D27 | Semantic-merge observatory: replay landed merges to harvest specimens | Two-way |
| D28 | A read-only browser surface served by the node itself | Two-way |
| D29 | Per-repository authorization via an ACL file with read/write levels | Two-way |
| D30 | Repository browsing: server-rendered tree, blob, commit list and diff | Two-way |
| D31 | SSH transport via the host's `sshd` and a forced command, not an in-daemon server | Two-way |
| D32 | Outbound ref-landed webhooks from an operator-side hooks file | Two-way |
| D33 | Attributed request log and per-request rate limits | Two-way |
| D34 | Review pages on the browse surface | Two-way |
| D35 | Change lifecycle as three op variants (`CreateChange`, `CheckpointChange`, `ArchiveChange`) | **One-way for readers** |
| D36 | Account and credential self-service, deliberately outside the op log | Two-way |
| D37 | Per-user quotas as a request ceiling plus a replay-derived projection | Two-way |
| D38 | PR discussion as one additive op variant, `PostComment` | **One-way for readers** |
| D39 | Browser writes via passkeys, narrowing D28's no-JavaScript rule | **One-way for the signature format** |
| D40 | Replay purity and conflict quarantine, promoted to contract | **One-way if broken** |
| D41 | Push provenance as a schema field, not a channel-string convention | **One-way for readers** |
| D42 | Repository ownership as a landing gate, granted as an ACL level | Two-way |

## Reading the door column

- **Two-way** — reversible by deleting code. No persisted format, op, hash or
  signature is involved, so reversing leaves nothing to migrate.
- **One-way** — the choice is baked into bytes that already exist. Reversing it
  means migrating persisted data.
- **One-way for readers** — the feature can stop being *written* at any time and
  nothing breaks, but the log is append-only, so the first accepted entry of
  that shape permanently ends replay compatibility with earlier binaries.
- **Gated** — deliberately deferred, with the swap planned rather than assumed.
