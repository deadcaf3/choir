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
