#!/bin/sh
# Build one immutable, checksummed private-beta artifact. Requires the
# pinned SBOM tool installed by CI: cargo-cyclonedx 0.5.9.
set -eu

HERE=$(cd "$(dirname "$0")/.." && pwd)
OUT_ROOT=${1:-$HERE/dist/private-beta}

fail() { echo "build-private-beta: $1" >&2; exit 1; }
command -v cargo-cyclonedx >/dev/null 2>&1 \
  || fail "cargo-cyclonedx 0.5.9 is required"
[ "$(cargo cyclonedx --version | awk '{print $2}')" = "0.5.9" ] \
  || fail "cargo-cyclonedx must be pinned to 0.5.9"

cd "$HERE"
git diff --quiet && git diff --cached --quiet \
  || fail "release artifacts must come from a clean commit"
COMMIT=$(git rev-parse HEAD)
SHORT=$(git rev-parse --short=12 HEAD)
VERSION="0.0.1-$SHORT"
OUT="$OUT_ROOT/$VERSION"
[ ! -e "$OUT" ] || fail "artifact directory already exists: $OUT"

CHOIR_GIT_HEAD=$COMMIT cargo build --release --locked -p choir-node -p choir-cli
mkdir -p "$OUT"
install -m 0755 target/release/choir-node "$OUT/choir-node"
install -m 0755 target/release/choir "$OUT/choir"
install -m 0644 scripts/flip/private-beta.manifest "$OUT/private-beta.manifest"

for generated in crates/choir-node/choir-node.cdx.json crates/choir-cli/choir.cdx.json; do
  [ ! -e "$generated" ] || fail "refusing to overwrite pre-existing SBOM: $generated"
done
cleanup() {
  rm -f crates/choir-node/choir-node.cdx.json crates/choir-cli/choir.cdx.json
}
trap cleanup EXIT INT TERM
cargo cyclonedx --format json --describe binaries \
  --manifest-path crates/choir-node/Cargo.toml --override-filename choir-node.cdx.json
cargo cyclonedx --format json --describe binaries \
  --manifest-path crates/choir-cli/Cargo.toml --override-filename choir.cdx.json
install -m 0644 crates/choir-node/choir-node.cdx.json "$OUT/choir-node.cdx.json"
install -m 0644 crates/choir-cli/choir.cdx.json "$OUT/choir.cdx.json"

cat > "$OUT/release-manifest.json" <<MANIFEST
{"format_version":1,"version":"$VERSION","commit":"$COMMIT","rust":"1.97.1","cargo_cyclonedx":"0.5.9","profile":"release","locked":true}
MANIFEST
(
  cd "$OUT"
  sha256sum choir-node choir private-beta.manifest choir-node.cdx.json choir.cdx.json release-manifest.json > SHA256SUMS
)
tar -C "$OUT_ROOT" -czf "$OUT_ROOT/choir-private-beta-$VERSION.tar.gz" "$VERSION"
sha256sum "$OUT_ROOT/choir-private-beta-$VERSION.tar.gz" \
  > "$OUT_ROOT/choir-private-beta-$VERSION.tar.gz.sha256"
echo "build-private-beta: $OUT_ROOT/choir-private-beta-$VERSION.tar.gz"
