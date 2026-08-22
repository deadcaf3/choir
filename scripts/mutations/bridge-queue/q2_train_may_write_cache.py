"""D5/D18: the node's queue lets speculative jobs write a shared cache.

A cache written from a state nobody approved is the CREEP shape, and a
merge queue open to outside proposals is exactly where it is reached.
Nothing about the round's outcome changes, so only an assertion on the
jobs themselves can see it.
"""
import io

PATH = "crates/choir-bridge/src/queue.rs"
OLD = "        may_write_cache: false,"
NEW = "        may_write_cache: true,"

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
