"""A crate's key covers only its own files, not the crates it depends on.

Editing a crate then invalidates that crate and nothing downstream, so
the dependents are served green against a dependency they have never
been compiled with. This is the failure `git diff` alone cannot see, and
it is the whole reason the key asks cargo for a closure.
"""
import io

PATH = "gate"
OLD = "      for _d in $_deps; do"
NEW = "      for _d in $_c; do"

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
