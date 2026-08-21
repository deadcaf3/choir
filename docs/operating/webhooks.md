# Webhooks (D32)

The node's only outbound request to an address somebody else chose. It fires
when a ref you named moves, it carries what moved and nothing else, and it is
best-effort rather than at-least-once — a receiver that may not miss a ref
polls `GET /api/log?from=N` instead.

`--hooks-file` posts to a URL you name whenever a ref you name moves. One subscription per line, `#` comments, and the same append-a-line discipline as every other policy file:

```text
# <repo:refname pattern>        <url>                        <secret>   [allow-private]
owner/demo:refs/heads/main      https://ci.example/choir      <SECRET>
owner/demo:refs/heads/*         https://ci.example/branches   <SECRET>
owner/notes:refs/tags/*         http://127.0.0.1:9000/hook    <SECRET>   allow-private
```

Patterns are the `--protected-refs` grammar: a trailing `*` is a prefix, anything else is exact, and the string matched is the view's `<repo>:<refname>` key. The file is re-read when its mtime moves, so adding a subscription is appending a line. It needs `--keys-file`, since refs reach the log through the platform sequencer. Delivery records go to `<repo-root>/.choir/hooks.jsonl`.

The body says what moved, and nothing else:

```json
{"format_version":1,"event":"ref-landed","repo":"owner/demo","ref":"refs/heads/main",
 "ref_key":"owner/demo:refs/heads/main","old":"<git oid>","new":"<git oid>","seq":41,
 "entry":"<entry hash>","actor":"<channel>","key_id":"<signing key id>"}
```

`old` is null for a created ref and `new` is null for a deleted one. `entry` is the log entry's content hash: it is unique per event, so a receiver that has already acted on one can discard a repeat.

**Verify the secret.** The delivery carries `X-Choir-Hook-Secret: <secret>`, the secret on that subscription's line, and a receiver should compare it before acting; otherwise anything that can reach the URL can pretend to be your node. It is a bearer secret rather than a signature over the body, so give each subscription its own (`openssl rand -hex 32`) and keep the file mode 0600. A non-loopback target must therefore be `https`; the node refuses to send the secret in clear.

**Best-effort, never silent.** Three attempts per delivery, and every attempt, every refusal and every dropped event is a JSON line in `hooks.jsonl`. Deliveries are *not* at-least-once: a webhook runs on its own thread behind a bounded queue, and when a receiver is slower than the node produces refs, events are dropped and counted rather than allowed to delay op admission. A receiver that may not miss a ref polls `GET /api/log?from=N` instead, which is what agents already do for catch-up.

**Targets are vetted.** This is the node's only outbound request to an address someone else chose, so it refuses loopback, private, carrier-NAT, link-local (including the `169.254.169.254` metadata service), unique-local and unspecified addresses unless the line ends in `allow-private`; it connects to the address it vetted rather than re-resolving the name; and it follows no redirects.
