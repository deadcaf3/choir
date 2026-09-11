#!/usr/bin/env python3
"""Compact, one-screen renderings of the node's JSON for demo/run.sh.

Reads one API response on stdin and prints the few rows a beat is about.
The full documents are `choir view <api>` and `choir log <api>`; nothing
here is computed, only trimmed.

    show.py refs     < /api/view          every ref, short oid
    show.py log      < /api/log?from=N    one line per op: seq, kind, CAS, chain
    show.py checks   < /api/view          every ci/queue verdict, one line each
    show.py queue    < /api/queue/run     what the round decided
    show.py next-seq < /api/view          the log's next sequence number
    show.py subject  < /api/view          one candidate oid that carries a check
    show.py conflict < /r/<repo>/blob/<rev>/<path>   the web view's three-sided conflict
"""

import json
import sys


def oid(h):
    """Short git oid from a ContentHash with the git-oid codec."""
    return bytes(h["digest"]).hex()[:7] if h else "-------"


def refs(v):
    for name, value in sorted(v["refs"].items()):
        print(f"  {name:45} {value[3:10]}")


def log(page):
    snapshots = 0
    for e in page["entries"]:
        kind = json.loads(bytes.fromhex(e["payload_hex"]))["kind"]
        seq = e["seq"]
        h = e["hash"][3:10]
        parent = (e["parent"] or "---------")[3:10]
        # Ref names are `<repo>:<refname>`; one repo here, so the refname.
        if "SetRef" in kind:
            op = kind["SetRef"]
            cas = f"{oid(op.get('prev'))} -> {oid(op['commit'])}"
            name = op["name"].split(":", 1)[-1]
            print(f"  seq {seq:<3} SetRef    {name:28} {cas}  hash {h} parent {parent}")
        elif "DeleteRef" in kind:
            op = kind["DeleteRef"]
            cas = f"{oid(op.get('prev'))} -> deleted"
            name = op["name"].split(":", 1)[-1]
            print(f"  seq {seq:<3} DeleteRef {name:28} {cas}  hash {h} parent {parent}")
        elif "RecordRefSnapshot" in kind:
            # The D25 attestation the node appends after each ref move;
            # in the chain, and verified in beat 5, but not the story.
            snapshots += 1
        else:
            print(f"  seq {seq:<3} {next(iter(kind)):40} hash {h} parent {parent}")
    if snapshots:
        print(f"  ({snapshots} RecordRefSnapshot entries between these, one per ref move, omitted)")


def conflict(html):
    """The conflict the node's web view found in a file, as three columns."""
    import re

    head = re.search(r'<div class="conflict-head">.*?<span>line (\d+)</span>', html)
    if not head:
        print("  (the web view shows no conflict in this file)")
        return
    print(f"  conflict at line {head.group(1)}, as the node's web view reads it:")
    for label, body in re.findall(r'<div class="side side-\w+"><h4>(.*?)</h4><pre>(.*?)</pre>', html):
        rows = re.findall(r'<span class="row[^"]*">(.*?)</span>', body)
        text = " / ".join(r.replace("&quot;", '"').replace("&lt;", "<").replace("&gt;", ">") for r in rows)
        print(f"    {label[:7]:8} {text}")


def checks(v):
    rows = sorted(v["checks"].items(), key=lambda kv: kv[1]["status"])
    for key, c in rows:
        print(f"  candidate {key.split(':')[0][3:10]}  {c['status']:8} {c['evidence']}")


def queue(r):
    print(f"  merged   : {r['merged']}")
    print(f"  rejected : {[(x['id'], x['why']) for x in r['rejected']]}")
    print(f"  stalled  : {r['stalled']}")
    print(f"  main tip : {r['tip'][:7]}")


def next_seq(v):
    print(v["log"]["next_seq"])


def subject(v):
    print(sorted(v["checks"])[0].split(":")[0][3:])


COMMANDS = {
    "refs": refs,
    "log": log,
    "checks": checks,
    "queue": queue,
    "next-seq": next_seq,
    "subject": subject,
    "conflict": conflict,
}

if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.argv[1] not in COMMANDS:
        sys.exit(__doc__)
    if sys.argv[1] == "conflict":
        conflict(sys.stdin.read())
    else:
        COMMANDS[sys.argv[1]](json.load(sys.stdin))
