"""D68: the landing stops compare-and-swapping on the base it read.

The branch then moves regardless of who else moved it first, so a round
built on a stale reading overwrites the winner's work instead of losing
to it. A test must notice that the loser landed.
"""
import io

PATH = "crates/choir-node/src/queue.rs"
OLD = "            prev: self.at.clone(),"
NEW = "            prev: None,"

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
