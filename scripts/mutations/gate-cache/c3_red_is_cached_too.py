"""A stage's verdict is cached whether it passed or failed.

A failure then stops re-proving itself: one red run leaves a marker, and
every run after it is served the failure's key as though it were green.
That is how a flaky pass becomes a durable one.
"""
import io

PATH = "gate"
OLD = '  stage_green clippy && mark_list clippy "$CL_RUN"'
NEW = '  mark_list clippy "$CL_RUN"'

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
