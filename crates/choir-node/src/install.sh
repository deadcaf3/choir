#!/bin/sh
# The choir installer, served by a choir node at /download/install.sh.
#
# Every URL it fetches is built from BASE, and BASE is substituted by the
# node from the Host header of the request that asked for this script. So
# the bytes come from the same origin the script did, and there is no
# second host to trust and none baked in at build time. A copy saved to
# disk and re-run still points at the node it came from.
#
# What this does and does not promise: the checksums are served by the
# same node as the archives, so they prove the transfer, not the build.
# The trust boundary is TLS plus whoever runs the node. Read the script
# before piping it anywhere, which is why it is short.
#
# Usage, once served: fetch it from a node and pipe it to sh. It takes
# package names as arguments and installs both when given none.
set -eu

BASE="__CHOIR_DOWNLOAD_BASE__"

# An unsubstituted copy must fail loudly. Served straight off a disk by
# something that is not a choir node, this script would otherwise build
# URLs against a placeholder and report a confusing 404 per archive.
#
# The pattern is deliberately not the whole placeholder. The renderer
# replaces every occurrence of that string, so spelling it here in full
# would rewrite this test into "refuse when BASE starts with BASE",
# which refuses every rendered copy and no unrendered one.
case "$BASE" in
*CHOIR_DOWNLOAD_BASE*)
	echo "install.sh: this script is rendered by a choir node; fetch it from one" >&2
	exit 1
	;;
esac

# Everything, by default. The first version installed `choir-cli` alone,
# on the reasoning that somebody joining a node does not run one -- and
# that made the second line of our own quick start fail, because
# `choir host` execs the daemon. A default that needs an argument to
# match the documented path is the wrong default, and the daemon is
# 2.9 MiB of download, which is no reason to hand somebody a
# `command not found` one step later. Pass names to narrow it.
APPS=${*:-choir-cli choir-node}

case $(uname -s) in
Darwin) os=apple-darwin ;;
Linux) os=unknown-linux-gnu ;;
*)
	echo "install.sh: no prebuilt binaries for $(uname -s); build from source instead:" >&2
	echo "  cargo install --git $BASE/../choir/choir.git choir-cli choir-node" >&2
	exit 1
	;;
esac

case $(uname -m) in
arm64 | aarch64) arch=aarch64 ;;
x86_64 | amd64) arch=x86_64 ;;
*)
	echo "install.sh: no prebuilt binaries for $(uname -m)" >&2
	exit 1
	;;
esac

target="$arch-$os"

# BSD and GNU spell the same digest with two commands and neither is
# everywhere. Both print the hex first, which is the only field read.
if command -v sha256sum >/dev/null 2>&1; then
	digest() { sha256sum "$1"; }
elif command -v shasum >/dev/null 2>&1; then
	digest() { shasum -a 256 "$1"; }
else
	echo "install.sh: need sha256sum or shasum to verify the download" >&2
	exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

dest="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "$dest"

installed=""
for app in $APPS; do
	archive="$app-$target.tar.xz"
	echo "install.sh: fetching $archive"
	curl -fsSL "$BASE/$archive" -o "$tmp/$archive"
	curl -fsSL "$BASE/$archive.sha256" -o "$tmp/$archive.sha256"

	want=$(cut -d' ' -f1 <"$tmp/$archive.sha256")
	got=$(digest "$tmp/$archive" | cut -d' ' -f1)
	if [ "$want" != "$got" ]; then
		echo "install.sh: checksum mismatch on $archive" >&2
		echo "  expected $want" >&2
		echo "  got      $got" >&2
		exit 1
	fi

	# The archive holds one directory named after itself, with the
	# binaries beside a README and the licences. Anything executable in
	# there is a binary we ship; nothing else is.
	if ! tar -xJf "$tmp/$archive" -C "$tmp"; then
		echo "install.sh: could not unpack $archive; is xz installed?" >&2
		exit 1
	fi
	for bin in $(find "$tmp/$app-$target" -type f -perm -u+x); do
		name=$(basename "$bin")
		cp "$bin" "$dest/$name"
		chmod +x "$dest/$name"
		installed="$installed $name"
	done
done

echo "install.sh: installed$installed in $dest"

# Installed is not the same as found. On a machine with no Rust on it,
# $dest is on nobody's PATH, and the reader's very next command would be
# `command not found`. So new terminals are given the directory, once, in
# the startup file of the reader's shell, and this terminal, which no
# child process can change, is told the one line that finishes it.
case ":$PATH:" in
*":$dest:"*) ;;
*)
	line="export PATH=\"$dest:\$PATH\""
	case ${SHELL:-} in
	*/zsh) profile="${ZDOTDIR:-$HOME}/.zshrc" ;;
	*/bash)
		# A terminal on macOS starts a login shell, which reads this one.
		if [ "$os" = apple-darwin ]; then profile="$HOME/.bash_profile"; else profile="$HOME/.bashrc"; fi
		;;
	*/fish) profile="" ;;
	*) profile="$HOME/.profile" ;;
	esac
	if [ -z "$profile" ]; then
		echo "install.sh: run this once and every fish terminal finds it: fish_add_path $dest" >&2
	else
		if grep -qsF "$line" "$profile"; then
			:
		elif ! printf '\n# Added by the choir installer, so a new terminal finds choir.\n%s\n' "$line" >>"$profile" 2>/dev/null; then
			echo "install.sh: could not write $profile; add this line to it yourself: $line" >&2
		fi
		echo "install.sh: in this terminal, run: $line" >&2
	fi
	;;
esac
