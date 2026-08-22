"""D68: a refused landing is ignored instead of stopping the round.

The queue then believes it landed something the log rejected, advances
its base, and every later change in the round speculates on a state that
does not exist. A test must notice that a refusal is not a landing.
"""
import io

PATH = "crates/choir-node/src/queue.rs"
OLD = "        handle.try_submit(QUEUE_CHANNEL, payload, Some(sig))?;"
NEW = "        let _ = handle.try_submit(QUEUE_CHANNEL, payload, Some(sig));"

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
