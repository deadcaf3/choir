"""D5: run_queue stops verifying that its base is a commit.

Every change is then merged onto a state the repository does not have,
and the resulting failures are reported against the people who opened
the pull requests. A base that is not a commit is the node's fault and
must read as one stall, not as everyone's change going red.
"""
import io

PATH = "crates/choir-bridge/src/queue.rs"
OLD = "    let base_oid = speculator.verify(base)?;"
NEW = "    let base_oid = base.to_string();"

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
