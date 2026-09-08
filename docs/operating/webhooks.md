# Webhooks (D32)

The node's only outbound request to an address somebody else chose. It
fires when a named ref moves and is best-effort; a receiver that may not
miss a ref polls `GET /api/log?from=N`.

`--hooks-file`, one subscription per line, `#` comments:

```text
# <repo:refname pattern>        <url>                        <secret>   [allow-private]
owner/demo:refs/heads/main      https://ci.example/choir      <SECRET>
owner/demo:refs/heads/*         https://ci.example/branches   <SECRET>
owner/notes:refs/tags/*         http://127.0.0.1:9000/hook    <SECRET>   allow-private
```

Patterns follow `--protected-refs`: trailing `*` is a prefix, else exact,
matched against `<repo>:<refname>`. Re-read on mtime change. Needs
`--keys-file`. Delivery records go to `<repo-root>/.choir/hooks.jsonl`.

The body:

```json
{"format_version":1,"event":"ref-landed","repo":"owner/demo","ref":"refs/heads/main",
 "ref_key":"owner/demo:refs/heads/main","old":"<git oid>","new":"<git oid>","seq":41,
 "entry":"<entry hash>","actor":"<channel>","key_id":"<signing key id>"}
```

`old` is null for a created ref, `new` null for a deleted one. `entry` is
unique per event; discard repeats on it.

**Verify the secret.** The delivery carries `X-Choir-Hook-Secret: <secret>`.
It is a bearer secret, so give each subscription its own
(`openssl rand -hex 32`), keep the file 0600, and use `https` for any
non-loopback target; the node refuses to send the secret in clear.

**Best-effort, never silent.** Three attempts per delivery; every attempt,
refusal and dropped event is a line in `hooks.jsonl`. A bounded queue on
its own thread drops events rather than delaying op admission.

**Targets are vetted.** Loopback, private, carrier-NAT, link-local
(including `169.254.169.254`), unique-local and unspecified addresses are
refused unless the line ends in `allow-private`. It connects to the vetted
address and follows no redirects.
