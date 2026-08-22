"""D68: the round stops filtering proposals by the branch they target.

Proposals aimed at other branches are then merged into this one, which
is a merge nobody asked for. A test must aim a proposal at a second
branch and notice it was not swept into the first round.
"""
import io

PATH = "crates/choir-node/src/queue.rs"
OLD = 'let prefix = format!("{repo}:refs/for/{branch}/");'
NEW = 'let prefix = format!("{repo}:refs/for/");'

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
