"""D49/D68: the node seals its check reports without a signature.

Every report is then refused by the node's own policy and the round
finishes with a full `unreported_checks` list and nothing in the log --
which looks, from inside the report, exactly like a round that had
nothing to report. Only the served view can tell the two apart.
"""
import io

PATH = "crates/choir-node/src/platform.rs"
OLD = "                (payload, Some(sig))"
NEW = "                (payload, None)"

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
