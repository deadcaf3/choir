"""The key is built from the index alone, not from what is on disk.

An unstaged edit then names the same key as the tree without it, so the
edit-loop case -- the one the cache exists for -- serves a green verdict
for a tree nobody ran anything against. A test must edit a file without
staging it and notice the verdict survived.
"""
import io

PATH = "gate"
OLD = '      git status --porcelain --untracked-files=all -- "crates/$_c" |'
NEW = "      true |"

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
