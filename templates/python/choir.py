"""A choir client, generated from the node's own API description.

This file exists to answer one question: **is `/api/schema` enough to
generate a working client without reading choir's source?** The generator
is handed the schema document and nothing else — not the Rust table the
schema is rendered from — so if a method here is wrong, the description
is what was insufficient.

Standard library only. A client a third party can drop in and run is the
point; `pip install` is a prerequisite, and a prerequisite is exactly the
thing this is testing the absence of.

    from choir import Choir

    node = Choir("https://node.example", auth_file="~/.choir/auth")
    print(node.choir_view()["log"]["head"])
    print(node.choir_log(**{"from": 0})["entries"][0]["seq"])

**What this cannot do, stated rather than discovered: sign.** Every write
is ed25519 over ``(channel, payload)`` and the standard library has no
ed25519. So the write methods take an *already signed* body — the
`choir` binary or the shell library produces one — and this client
carries it. Reads need nothing extra. Adding a signing dependency would
make this a different artifact answering a different question.
"""

import base64
import json
import urllib.error
import urllib.parse
import urllib.request


class ChoirError(Exception):
    """A refusal from the node, carrying its own words.

    choir's refusals are structured (`code`, `expected`, `actual`,
    `next`) precisely so a client does not have to guess what to do, so
    they are kept whole rather than flattened into a message.
    """

    def __init__(self, status, body):
        self.status = status
        self.body = body
        code = body.get("code") if isinstance(body, dict) else None
        nxt = body.get("next") if isinstance(body, dict) else None
        super().__init__(f"{status} {code or ''}: {nxt or body}".strip())


class Choir:
    """One node, one credential."""

    def __init__(self, api, auth_file=None, timeout=30):
        self.api = api.rstrip("/")
        self.timeout = timeout
        self._auth = None
        if auth_file:
            # The node's own `user:token`-per-line format. Read from a
            # file and never from an environment variable or an argument,
            # which is the rule the CLI enforces: a secret on a command
            # line is visible to every process through `ps`.
            import os

            with open(os.path.expanduser(auth_file), "r", encoding="utf-8") as handle:
                for line in handle:
                    line = line.strip()
                    if line and not line.startswith("#"):
                        self._auth = line
                        break

    def _request(self, method, path, query=None, body=None):
        url = self.api + path
        if query:
            url += "?" + urllib.parse.urlencode(query)
        data = None
        headers = {}
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if self._auth:
            token = base64.b64encode(self._auth.encode("utf-8")).decode("ascii")
            headers["Authorization"] = "Basic " + token
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                return json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as error:
            raw = error.read().decode("utf-8", "replace")
            try:
                parsed = json.loads(raw)
            except ValueError:
                parsed = raw
            raise ChoirError(error.code, parsed) from None

    def capabilities(self):
        """What this particular node will accept.

        The schema's static half describes the API; this is the half that
        describes the deployment, and it is why a client should ask
        rather than assume. Branch on these instead of probing an
        endpoint and reading the refusal.
        """
        return self.choir_schema()["capabilities"]

    # --- generated: choir surface, do not edit ---

    # One method per tool, from the node's own description.
    # Generated; edit `crates/choir-cli/src/surface.rs`.

    def choir_submit(self, **arguments):
        """Submit one signed operation (hex payload, hex signature)

        Arguments become the JSON request body.
        """
        return self._request("POST", "/api/submit", body=arguments)

    def choir_submit_batch(self, **arguments):
        """Same, in array order; the primary path for agent workloads
        (throughput figures live in the build log, not here, so they
        cannot go stale)

        Arguments become the JSON request body.
        """
        return self._request("POST", "/api/submit-batch", body=arguments)

    def choir_view(self, **arguments):
        """The materialized view plus the latest ref-state attestation,
        durable key bindings, T2 new-actor review outcomes, T3
        concentration, T4 newcomer harm, complete-view growth, the commit
        this daemon was built from, and the sequencer's measured decision
        latency against the 100 ms gate. On a node running an ACL you are
        served your own slice: the repositories your credential may read,
        plus reviews you were assigned to; the node-wide sections need a
        node-wide grant. A repository missing from the response is one
        you were not granted, not one that is gone. Every map-shaped
        section is bounded: `limit` rows each (200 by default, 1000 at
        most), `offset` rows skipped in key order, `<section>_omitted`
        counting what this page left out, and `paging.next` naming the
        request that fetches the rest or being null when there is none

        Arguments become the query string: limit, offset.
        """
        return self._request("GET", "/api/view", query=arguments)

    def choir_appeal(self, **arguments):
        """Record an appeal for a rejected newcomer attempt; it requests
        operator adjudication and never changes privilege

        Arguments become the JSON request body.
        """
        return self._request("POST", "/api/appeal", body=arguments)

    def choir_log(self, **arguments):
        """Ordered log entries, the catch-up and sync primitive. Absolute
        `from`: entries evicted from the in-memory window are served from
        the persisted log (`source` says which), and a node that cannot
        reach that far back answers 409 rather than a page with a hole in
        it. Each entry carries its hash, parent and author signature so
        pages can be chained, replayed and checked against the chain a
        second reader holds; SYNC.md is that procedure

        Arguments become the query string: from.
        """
        return self._request("GET", "/api/log", query=arguments)

    def choir_workspace(self, **arguments):
        """Provision a CoW workspace; optional exact base/change binding
        makes retries idempotent

        Arguments become the JSON request body.
        """
        return self._request("POST", "/api/workspace", body=arguments)

    def choir_workspace_archive(self, **arguments):
        """Recoverably archive a change-bound workspace and remove it from
        the active view

        Arguments become the JSON request body.
        """
        return self._request("POST", "/api/workspace/archive", body=arguments)

    def choir_reviews(self, **arguments):
        """One actor's pending review queue

        Arguments become the query string: reviewer, limit, offset.
        """
        return self._request("GET", "/api/reviews", query=arguments)

    def choir_schema(self):
        """This surface, machine-readable and versioned, plus what this
        particular node will accept — the description an agent
        generates a client from (D17)

        Takes no arguments.
        """
        return self._request("GET", "/api/schema")

    def choir_search(self, **arguments):
        """Search repository contents, file names or commit messages.
        Node-wide by default: every repository your credential may read,
        each at its own HEAD, which is why `rev` is accepted only
        alongside a single `repo`. A repository you were not granted is
        absent from the results and, asked for by name, is answered
        exactly as one that does not exist. Unindexed -- one `git grep`
        per repository -- so `limit` bounds what comes back while
        `matches` still counts everything found, and `truncated` says
        which happened. The same search the browser pages run, so the two
        cannot disagree about what a match is

        Arguments become the query string: q, in, repo, rev, limit.
        """
        return self._request("GET", "/api/search", query=arguments)

    def choir_profile(self, **arguments):
        """One actor's standing, counted out of the view you may already
        see: the keys bound to them and how many ops ago, changes they
        own, reviews they were assigned and the verdicts they gave,
        approvals slashed, checks they reported. It is a reading of
        `/api/view` and never a wider one -- two callers with different
        grants get different numbers about the same actor, which is the
        point. `vouches` names the operator whose graph it is (a vouch is
        between operators, so `ops/agent` reads `ops`), who vouches for
        them and whether each edge points both ways. D24 wants key age,
        vouches, scoped grants and bonds against approval inflation; two
        of the four are reported here as inputs, because a score would be
        a weighting of one against the other that nobody has measured.
        Scoped grants exist since D66 and are absent from this reading on
        purpose: a time-locked grant lives in the node's authorization
        files rather than in the log, so nothing counted out of the view
        can see one

        Arguments become the query string: channel.
        """
        return self._request("GET", "/api/profile", query=arguments)
# --- /generated ---
