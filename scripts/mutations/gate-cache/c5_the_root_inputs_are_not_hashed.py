"""The global key drops the build inputs that belong to no crate.

`.cargo/config.toml` carries the LIBSQLITE3_FLAGS every build in this
workspace needs and the root manifest carries the workspace lints, so a
change to either changes what a green verdict means for all fifteen
crates while leaving every key exactly where it was.
"""
import io

PATH = "gate"
OLD = "    for _root in Cargo.lock Cargo.toml .cargo/config.toml rust-toolchain.toml gate; do"
NEW = "    for _root in Cargo.lock gate; do"

s = io.open(PATH, encoding="utf-8").read()
assert s.count(OLD) == 1, f"anchor matched {s.count(OLD)} times"
io.open(PATH, "w", encoding="utf-8").write(s.replace(OLD, NEW))
