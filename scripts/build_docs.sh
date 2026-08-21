#!/bin/sh
# Build the documentation: the book, and the API documentation inside it.
#
#   scripts/build_docs.sh          build into ./book
#   scripts/build_docs.sh --open   build, then open it
#
# Two renderers over one set of files. `docs/*.md` is the book's source
# and is also pulled into the crates with `#![doc = include_str!]`, so a
# page cannot say one thing here and another thing in `cargo doc`.
#
# rustdoc lands at `book/api/`, which is what lets the book link to a
# type and rustdoc be reachable from the book's own navigation. Keeping
# them in separate trees is the version of this that produces a book
# whose API links 404.
#
# Fails closed: any stage failing exits nonzero, and no stage's status is
# read through a pipe.
set -eu
cd "$(dirname "$0")/.."

OPEN=0
[ "${1:-}" = "--open" ] && OPEN=1

if ! command -v mdbook >/dev/null 2>&1; then
  echo "mdbook is not installed." >&2
  echo "  cargo install mdbook --locked" >&2
  echo "and make sure \$HOME/.cargo/bin is on your PATH." >&2
  exit 1
fi

# Same flags the gate's rustdoc stage uses. A doc build that warns here
# and is denied there is a difference nobody wants to discover at commit
# time, and the pages in `docs/` are compiled by both.
echo "  api   cargo doc"
RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps

echo "  book  mdbook build"
mdbook build

# `target/doc` rather than a glob: `cargo doc` also writes files at that
# root (the search index, the cross-crate item list) that the crate
# directories alone would leave behind.
DOC=${CARGO_TARGET_DIR:-target}/doc
if [ ! -d "$DOC" ]; then
  echo "cargo doc produced no $DOC" >&2
  exit 1
fi

echo "  copy  $DOC -> book/api"
rm -rf book/api
mkdir -p book/api
# `cp -R <dir>/.` copies the contents rather than the directory, which
# is the difference between `book/api/index.html` and `book/api/doc/...`.
cp -R "$DOC/." book/api/

# rustdoc's own landing page lists every crate, but only when it is
# generated for a workspace; when it is not, there is no `index.html` at
# that root and `book/api/` would 404 from the book's own link.
if [ ! -f book/api/index.html ]; then
  echo "  note  no rustdoc index; writing a redirect to choir_node"
  cat > book/api/index.html <<'HTML'
<!doctype html>
<meta charset="utf-8">
<title>choir API documentation</title>
<meta http-equiv="refresh" content="0; url=choir_node/index.html">
<a href="choir_node/index.html">choir_node</a>
HTML
fi

echo
echo "  book/index.html      the book"
echo "  book/api/index.html  the API documentation"

if [ "$OPEN" = 1 ]; then
  if command -v open >/dev/null 2>&1; then
    open book/index.html
  elif command -v xdg-open >/dev/null 2>&1; then
    xdg-open book/index.html
  fi
fi
