#!/bin/sh
# Puts a release on the node's shelf, so `curl https://<node>/download/…`
# serves the binaries rather than a forge (D79).
#
# usage: publish_release.sh <owner/repo> <tag|latest> <shelf-dir>
#
# The archives are built by CI and downloaded from wherever that CI
# publishes them; what this changes is where a *reader* fetches them
# from, which is the whole claim the install command makes. It does not
# make the node the builder, and nothing here pretends otherwise.
#
# Two kinds of file are deliberately left behind:
#
#   *-installer.sh   the packaging tool's own installers, which carry the
#                    release host baked in at build time. Serving one
#                    from the node would hand a reader a script that goes
#                    straight back to the forge -- the exact thing the
#                    shelf exists to stop. The node renders its own at
#                    /download/install.sh.
#   source.tar.gz    the source is the repository this node already
#                    serves over git, at a revision a reader can name.
set -eu

REPO=${1:?usage: publish_release.sh <owner/repo> <tag|latest> <shelf-dir>}
TAG=${2:?usage: publish_release.sh <owner/repo> <tag|latest> <shelf-dir>}
SHELF=${3:?usage: publish_release.sh <owner/repo> <tag|latest> <shelf-dir>}

case $TAG in
latest) BASE="https://github.com/$REPO/releases/latest/download" ;;
*) BASE="https://github.com/$REPO/releases/download/$TAG" ;;
esac

# The manifest of what a release contains, so this script needs no list
# of target triples of its own and cannot drift from the one CI builds.
api="https://api.github.com/repos/$REPO/releases"
case $TAG in
latest) api="$api/latest" ;;
*) api="$api/tags/$TAG" ;;
esac

# Every `"name"` in that document, narrowed to the ones that are
# artifacts. The release itself carries a `"name"` too -- its tag -- and
# an allowlist of extensions drops that without this script having to
# know what the tag is called, which `latest` does not tell it.
names=$(curl -fsSL "$api" |
	sed -n 's/.*"name": "\(.*\)".*/\1/p' |
	grep -E '\.(tar\.xz|sha256|sum|json)$' |
	grep -v -e '-installer\.sh$' -e '^source\.' || true)

if [ -z "$names" ]; then
	echo "publish_release.sh: no artifacts found for $REPO $TAG" >&2
	exit 1
fi

mkdir -p "$SHELF"
staged=0
for name in $names; do
	# A name that is not one path segment is not fetched. The shelf is
	# a public directory and this script writes into it from a remote
	# listing, so the listing does not get to choose a path.
	case $name in
	*/* | .* | "") echo "publish_release.sh: skipping $name" >&2; continue ;;
	esac
	echo "publish_release.sh: fetching $name"
	curl -fsSL "$BASE/$name" -o "$SHELF/$name.part"
	mv "$SHELF/$name.part" "$SHELF/$name"
	staged=$((staged + 1))
done

echo "publish_release.sh: $staged files on the shelf at $SHELF"
echo "publish_release.sh: the node serves them at /download/ once it is"
echo "publish_release.sh: (re)installed with this directory present"
