# The sync contract

`GET /api/log?from=N` returns every admitted operation, in one total order,
from a cursor the reader controls. It is the only endpoint a replica needs,
and this file is what another implementation is written against.

It specifies the cursor, and how to check that the pages you were handed
are the chain. That check is a replay, not an inclusion proof: it catches a
node that contradicts your copy, not one that shows two readers two
consistent histories.

Every node serves this file at `GET /sync.md`.

## The cursor

`from` is an **absolute sequence number**. `from=0` is the first operation
ever admitted, and is the default.

A page carries at most **500** entries. Ask again with `from = last_seq + 1`
until you get an empty page, which means you are caught up.

```json
{
  "entries": [ { "seq": 41, "workspace": "...", "...": "..." } ],
  "window_base": 0,
  "source": "window"
}
```

- `window_base`: the oldest seq still in memory. Below it, answers come from
  the persisted log if there is one.
- `source`: `"window"` (memory) or `"log"` (persisted file). Provenance
  only; both render entries identically, and one catch-up run may change
  `source` from `log` to `window` near the head.
- `workspace`: the frozen format-v1 name of the signature-covered
  attribution channel. Not a workspace id; submit clients call it
  `channel`.

### Outcomes, and what to do about each

| Outcome | Meaning | Do |
|---|---|---|
| `200`, entries non-empty | A page starting at `from` | Verify, apply, ask again from `last_seq + 1` |
| `200`, entries empty | You are at the head | Poll later; there is no push channel |
| `409`, `code: "log_evicted"` | `from` is below `window_base` and there is no persisted log | Restart from `window_base` (in the body) as a new anchor; see the gap rule |
| `500`, `code: "unclassified"` | The node failed to read its own log | Retry |

Every rejection body carries `code`, `error` and `next`; see the
[rejection-code catalog](ERRORS.md). The 409 also carries `window_base` as
a number.

**The gap rule.** After resuming at `window_base`, your chain is two pieces
with an unverifiable hole. Record the resume point as a new trust anchor;
do not report the two pieces as one verified chain.

## Verifying a page

Three independent checks, in increasing strength.

### 1. Continuity: did I get the chain, whole and in order?

Within a page, for each entry after the first:

```
entry[i].parent == entry[i-1].hash
entry[i].seq    == entry[i-1].seq + 1
```

Across a page boundary, check page *N*'s last `hash` against page *N+1*'s
first `parent`. This holds across a `source` change too.

`parent` is `null` on exactly one entry: seq 0, the genesis.

### 2. Recomputation: is `hash` the truth about this entry?

Rebuild the entry's canonical bytes, hash with BLAKE3-256, compare. The
canonical form is `serde_json` compact output, fields in this order:

```json
{"format_version":1,"parent":{"codec":30,"digest":[..32 bytes..]},"seq":7,"workspace":"alice","payload":[..bytes..],"witnesses":[],"author_sig":{"key_id":"1e-<64 hex>","signature":[..64 bytes..]}}
```

Rules:

- `parent` is `null` for genesis; otherwise the object above. A
  `ContentHash` is always `{"codec":N,"digest":[bytes]}`; `30` is `0x1e`,
  BLAKE3-256.
- `payload` and `signature` are JSON **arrays of byte values**. The
  response serves them as hex (`payload_hex`, `author_sig_hex`); decode
  before rebuilding.
- `author_sig` is **omitted entirely** when absent, not `null`. An unsigned
  entry's canonical form ends `..."witnesses":[]}`.
- `witnesses` is `[]` and is part of the hashed bytes. Include it.
- `format_version` is `1`. New fields arrive additively and
  omitted-when-absent, so a version-1 reader keeps hashing version-1
  entries correctly.

Then: `hash == "1e-" + hex(blake3(canonical_bytes))`.

### 3. Authorship: did the actor it names actually sign it?

The author signs content, not position, so this survives any lie about
ordering:

1. Signing bytes: `serde_json` of the tuple `[channel, payload]`, with
   `channel` from the entry's `workspace` field, compact, e.g.
   `["alice",[104,105]]`.
2. `signing_hash = "1e-" + hex(blake3(those bytes))`.
3. Verify the ed25519 signature `author_sig_hex` over the **ASCII of that
   hex string** using the public key you hold for `author_key`.

`author_key` is `"1e-" + hex(blake3(public_key_32_bytes))`.

**A passkey signature (D39, D45) carries four more fields, inside the
hashed form.** When `author_scheme` is `2` the signature is WebAuthn
ES256: ECDSA P-256 over `authenticator_data ‖ SHA-256(client_data_json)`,
where `client_data_json`'s `challenge` is the base64url of the signing hash.
Both are served as `authenticator_data_hex` and `client_data_json_hex`.

The fourth is `credential_key_hex` (D45): the credential's public key as
SubjectPublicKeyInfo DER. **Verify a passkey signature against this and
nothing else.** For a passkey entry `author_key` is the credential id,
which resolves only inside the node's account store. Entries written
before D45 lack it and cannot be checked.

Because the key arrives with the signature, verifying proves the bytes are
intact and signed by the named credential, and nothing about whether that
credential belonged to `workspace`. Report the two outcomes separately.

All four are omitted when absent, so an ed25519 entry's JSON is unchanged.
A client that ignores them cannot recompute a passkey-signed entry's hash.
Absent `author_scheme` means ed25519; an unrecognised value is a scheme
this client cannot check, not a bad signature.

An entry with `author_key: null` is unsigned. The API admits no such op, so
this is history written before signatures existed. Do not treat unsigned
as verified.

## What none of this catches

**Equivocation.** A node that forks its history and serves two
self-consistent versions passes all three checks.

The node emits a **ref-state attestation** after every accepted ref
update: a signed op carrying the complete ref map, the seq it describes,
and a pointer to the previous attestation. `/api/view` projects the latest
as `snapshot: {id, at_seq, prev_snapshot}`. Two readers can exchange
`snapshot.id` at a given `at_seq` and learn whether they saw the same ref
state. It is still the node's own signature, so it is a comparison
primitive, not proof.

## Witnesses (D67)

A witness is an independent operator that cosigns the attestation.
`/api/view` reports `witnessed: {<operator>: {snapshot, at}}`, and
`view_growth.counts` reports `witnesses` (ever cosigned) and
`witnesses_current` (cosigned the attestation being served now). Act on
the second.

**A cosignature is its own op, not a field on the entry.** `witnesses` on
an entry is inside its hashed bytes, so adding one later would orphan every
later entry, and gathering signatures before the append is the latency
path D16 forbids. `OpEntry.witnesses` stays empty; a witness cosigns the
attestation.

As a client:

1. Read `snapshot.id` and `witnessed` from `/api/view`.
2. Count the rows whose `snapshot` equals `snapshot.id`.
3. Decide what that count must be. The node sets no threshold.

Only the latest attestation can be cosigned, so a witness that races a new
one must re-read and re-sign. The node cannot cosign its own attestation,
but nothing here proves two operator names are two people. Until you have
independent witnesses, treat a single node's ordering as
trusted-by-configuration.

## Seeds (D80)

A **seed** is a node that holds a copy of another node's log: its
**home**. It replicates the home's log one way with the cursor above,
checks every page with the three checks above before it keeps any of
it, appends what passed through its own single writer, fetches the git
objects the log names, and serves what it verified. It writes nothing
of its own. There is one writer per log, and a seed is not a second
one: it is a reader that keeps what it read and says so.

### Setting one up

A seed is a named reader of its home, never an anonymous one. The
home's operator:

1. registers the seed's node public key in the home's keys file;
2. binds that key to an operator name, `<seed-name>`;
3. grants `<seed-name> @node auditor` (the node-wide read `/api/log`
   needs) and `<seed-name> * read` (or `read` on the repositories it
   should hold, since the log grant does not cover git).

The seed runs `choir-node <root> [port] --seed <home-url>
--seed-credential <file>`, where the file holds the one `user:token`
line the home issued. With no port it binds nothing. `choir seed
<home-url>` composes all of that: it mints the key, prints the three
lines above for the home's operator, and, run again with the credential,
starts and supervises the seed; `docs/operating/running-a-node.md` has
the walk-through.

### `GET /api/signers`

The log records an author's key id, a hash of the key, and never the
key, so the third check needs key material from elsewhere. Every node
serves it, behind the same node-wide read grant as `/api/log`:

```json
{
  "format_version": 1,
  "node": { "actor_id": "1e-...", "public_key_hex": "..." },
  "signers": [ { "actor_id": "1e-...", "public_key_hex": "...", "name": "alice" } ]
}
```

Check that each `actor_id` is `"1e-" + hex(blake3(public key))` before
using it. A seed pins the home's `node.actor_id` beside its own log on
first contact and refuses a home that later signs with another key.

An entry whose author key is not in this table is **unverified**: its
continuity and hash still checked, and the home admitted it, but nobody
here can say who signed it. A seed keeps it and counts it in
`replica.unverified_entries`. An entry that fails a check, or that the
seed's build cannot fold, **halts** replication at that seq: the prefix
before it is kept and served, and nothing after it is skipped past.

### `not_home`

A seed answers every write, API or git, with `421 Misdirected Request`:

```json
{"code":"not_home","error":"this node is a seed of <home> and writes nothing of its own","home":"<home>","next":"send the same request to <home>"}
```

On a push, git prints the refusal to the pusher as `remote:` lines
naming the home. Nothing needs re-signing to follow it: a seed's
`log.node` names its home and its `log.head` is a head of the home's
log, so an op signed against a seed's view is one the home admits, and
a compare-and-swap read from a seed that fell behind fails at the home
with the ordinary `stale_head`.

### `replica` in `/api/view`

`null` on a home. On a seed:

| Field | Meaning |
|---|---|
| `home`, `home_node_id` | Whose copy this is, and the key it pinned |
| `head_seq` | The last seq this seed holds |
| `home_head_seq` | The highest seq the home has served it; a fact about the last contact, not about now |
| `refs_verified`, `refs_pending` | Refs the view names that the bare repositories here hold at that oid, and that they do not yet |
| `unverified_entries` | Entries kept with no key to check their author |
| `gap` | The home evicted what this seed needed; the copy cannot continue the chain and signs no more statements |
| `halted` | `{seq, reason}` once replication has stopped |

### The witness statement

After any page that contained an attestation it folded, a seed signs a
statement over the latest attestation it holds and serves it at
`GET /api/witness`, with the last 64:

```json
{"statement":{"format_version":1,"witness":"1e-<seed key id>","home_node_id":"1e-...","snapshot":"1e-<attestation id>","at_seq":41,"seen_at_seq":57},
 "public_key_hex":"...","signature_hex":"..."}
```

The signed bytes are the statement exactly as shown, compact, fields in
that order, signed the way an op's author signs: over the signing hash
of `["seed/witness", <statement bytes>]`. `witness` must equal
`"1e-" + hex(blake3(public key))`, which is what lets the key travel
with the statement. It is the same key the home bound as the seed's
operator, so `bindings` on the home names who said it.

A seed never submits `CountersignSnapshot` to its home. Only the latest
attestation can be cosigned there, so a seed would have to win a race
against every push; the rule below makes the race irrelevant.

### The fork rule

A statement over attestation `S` endorses `S` and every attestation
reachable from `S` by `prev_snapshot`. Two attestations, whether two
statements or a statement and the home's current `snapshot.id`:

- **agree** when they are equal, or one is an ancestor of the other;
- are a **fork** when neither is an ancestor of the other.

Ancestry is walked over the `RecordRefSnapshot` payloads in the home's
own log, which a reader holds: fetch `/api/log` from the lower of the
two `at_seq` positions to the head, map each attestation's id to its
`prev_snapshot`, and follow the links. Read the seed's statement
**before** the home's view: then an honest home's current attestation
is never older than the one the seed folded, and a statement the walk
cannot reach from it is two histories, one shown to the seed and one
shown to you.

The body around the statement carries `gap` and `halted` from `replica`,
so a reader of `/api/witness` alone sees why a statement stopped moving.

`choir doctor` runs this for every seed named by `seeds =` in
`.choir/config`: a fork is an error, a seed with `gap` or `halted` is a
warning naming the seq and reason, and a statement far behind the home's
current attestation is a warning that says how far.

### What this still does not catch

A home that shows the same lie to every seed and every reader. The rule
detects two histories; it cannot tell you which one is true, and one
consistent history is consistent however it was made.
