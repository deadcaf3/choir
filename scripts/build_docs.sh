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

# Ask cargo where it writes rather than guessing.
#
# `target-dir` can come from `$CARGO_TARGET_DIR`, from a `[build]` table
# in a `.cargo/config.toml` in ANY ancestor directory of the working
# directory, or from the workspace default -- and only cargo knows which
# one won. This script first tried `${CARGO_TARGET_DIR:-target}`, which
# is right for the first and the third and wrong for the second: the
# worktrees this repository is developed in take their target directory
# from `.claude/worktrees/.cargo/config.toml`, two levels above the
# checkout, so `cargo doc` succeeded and the copy that followed it said
# "cargo doc produced no target/doc".
#
# `--no-deps` keeps it to this workspace's own manifests: no network, no
# build, and it answers in milliseconds.
DOC_ROOT=$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)
[ -n "$DOC_ROOT" ] || DOC_ROOT=${CARGO_TARGET_DIR:-target}

# `<dir>/doc` rather than a glob: `cargo doc` also writes files at that
# root (the search index, the cross-crate item list) that the crate
# directories alone would leave behind.
DOC=$DOC_ROOT/doc
if [ ! -d "$DOC" ]; then
  echo "cargo doc reported success but $DOC does not exist." >&2
  echo "cargo says its target directory is: $DOC_ROOT" >&2
  exit 1
fi

echo "  copy  $DOC -> book/api"
rm -rf book/api
mkdir -p book/api
# `cp -R <dir>/.` copies the contents rather than the directory, which
# is the difference between `book/api/index.html` and `book/api/doc/...`.
cp -R "$DOC/." book/api/

# rustdoc writes a root `index.html` only when it has a single root crate
# to point at; documenting a workspace with `--no-deps` leaves that root
# empty, so the book's own `/api/` link would 404. This writes the
# landing page rustdoc did not.
#
# It uses the same generated token sheet the book does -- copied in
# unhashed, since mdBook fingerprints its copy -- so the two halves of
# the documentation are the same colours rather than merely adjacent.
if [ ! -f book/api/index.html ]; then
  echo "  index no rustdoc root index; writing one"
  cp theme/choir-tokens.css book/api/choir-tokens.css

  {
    cat <<'HTML'
<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>choir — API documentation</title>
<link rel="stylesheet" href="choir-tokens.css">
<style>
  :root { font-size: 17px; }
  body { margin: 0; background: var(--ground); color: var(--ink);
         font-family: var(--font-sans); letter-spacing: var(--tr-body);
         line-height: var(--lh-copy); }
  main { max-width: var(--measure); margin: 0 auto;
         padding: var(--sp-16) var(--sp-5); }
  h1 { font-size: var(--fs-900); line-height: var(--lh-tight);
       letter-spacing: var(--tr-display); color: var(--strong);
       margin: 0 0 var(--sp-3); }
  p.lede { font-size: var(--fs-600); color: var(--muted); margin: 0 0 var(--sp-10); }
  a { color: var(--accent-ink); text-decoration-color: var(--accent-line);
      text-underline-offset: .18em; }
  a:hover { color: var(--accent-hover); }
  ul { list-style: none; padding: 0; margin: 0;
       display: grid; gap: var(--sp-2);
       grid-template-columns: repeat(auto-fill, minmax(13rem, 1fr)); }
  li a { display: block; background: var(--card); border: var(--border);
         border-radius: var(--r-md); padding: var(--sp-3) var(--sp-4);
         font-family: var(--font-mono); font-size: var(--fs-300);
         text-decoration: none; color: var(--ink); }
  li a:hover { border-color: var(--accent-line); background: var(--accent-tint);
               color: var(--accent-ink); }
  footer { margin-top: var(--sp-12); padding-top: var(--sp-5);
           border-top: var(--bw-hair) solid var(--line);
           color: var(--faint); font-size: var(--fs-300); }
</style>
<main>
<h1>choir</h1>
<p class="lede">API documentation, one page per crate. The prose that
explains how they fit is in <a href="../index.html">the book</a>.</p>
<ul>
HTML

    # Every directory rustdoc actually produced a page for, in glob
    # order. Binary targets appear here too -- `choir`, `choir_mcp`,
    # `choir_ssh` -- because they are real documentation pages, not
    # because the list failed to filter them out.
    for dir in book/api/*/; do
      name=$(basename "$dir")
      [ -f "$dir/index.html" ] || continue
      printf '<li><a href="%s/index.html">%s</a></li>\n' "$name" "$name"
    done

    cat <<'HTML'
</ul>
<footer>Generated by <code>scripts/build_docs.sh</code>. Rebuild with
<code>scripts/build_docs.sh --open</code>.</footer>
</main>
</html>
HTML
  } > book/api/index.html
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
