"""D18: run_queue stops refusing an executor from another protocol.

A well-formed answer about the wrong thing then becomes a landing. The
anchor carries the following `let speculator` line because the same
refusal text appears in `run_train_ci`, and an anchor matching twice
reports as proving nothing rather than as a survivor -- which is how
this mutation was lost once already.
"""
import io

PATH = "crates/choir-bridge/src/queue.rs"
OLD = """    let info = ci.info().map_err(|error| error.to_string())?;
    if info.protocol != PROTOCOL {
        return Err(format!(
            "executor `{}` speaks protocol {} and this build speaks {PROTOCOL}",
            info.name, info.protocol
        ));
    }

    let speculator = choir_queue::git::GitSpeculator::new(repo.to_path_buf());"""
NEW = """    let _info = ci.info().map_err(|error| error.to_string())?;

    let speculator = choir_queue::git::GitSpeculator::new(repo.to_path_buf());"""

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
