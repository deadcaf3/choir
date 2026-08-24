"""A voided freshness receipt leaves the markers that run minted.

Those stages ran against whatever was on disk mid-run, which is not the
tree any key names. Keeping them means the next run is served verdicts
for a tree that was never tested as a whole -- the receipt is void and
the cache says otherwise.
"""
import io

PATH = "gate"
OLD = 'if [ "$FRESH" -ne 0 ] && [ -s "$LOG/marked.list" ]; then'
NEW = 'if false && [ -s "$LOG/marked.list" ]; then'

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
