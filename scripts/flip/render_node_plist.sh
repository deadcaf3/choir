#!/bin/sh
# Pure LaunchAgent renderer for install_node.sh. Keeping rendering separate
# makes the policy-bearing argument list testable without building, minting
# secrets, or touching launchd.
set -eu

if [ "$#" -lt 11 ] || [ "$#" -gt 22 ]; then
  echo "usage: render_node_plist.sh <label> <bin> <root> <port> <auth> <keys> <reviewers> <log> <repos-file> <newcomer-audit> <newcomer-adjudications> [protected-refs] [require-scope] [tls-cert] [tls-key] [acl] [accounts] [behind-tls-proxy] [webauthn] [site-repo] [unused-on-macos] [downloads-dir]" >&2
  exit 2
fi

LABEL=$1
BIN=$2
ROOT=$3
PORT=$4
AUTH=$5
KEYS=$6
REVIEWERS=$7
LOG=$8
REPOS_FILE=$9
NEWCOMER_AUDIT=${10}
NEWCOMER_ADJUDICATIONS=${11}
PROTECTED_REFS=${12:-}
# Any non-empty 13th argument turns on D26 replay containment: the node
# then admits only ops whose signed payload names this node's log and a
# head still in the window. Positional like [protected-refs], and empty
# means absent for the same reason: an installer branch can always pass
# both slots and let the marker files decide.
REQUIRE_SCOPE=${13:-}
# TLS is one decision, not two: cert and key arrive together or not at
# all, and their presence is what flips the bind from loopback to
# 0.0.0.0. Half a pair is refused rather than defaulted, because the
# node itself will refuse a non-loopback bind without TLS (invariant 9)
# and the renderer failing here beats a unit that crash-loops there.
TLS_CERT=${14:-}
TLS_KEY=${15:-}
# A non-empty 16th argument is the D29 ACL path. Positional and
# empty-means-absent like the policy slots above, so an installer branch
# can always pass the slot and let the file's existence decide. Absent is
# not a safe default here in the way it is for TLS — without it every
# credential reaches every repository — but the node says so at startup,
# and refusing to render would lock out every single-operator node that
# has never needed one.
ACL=${16:-}
# A non-empty 17th argument is the D36 accounts file, same position as
# the systemd renderer's.
ACCOUNTS=${17:-}
# A non-empty 18th argument says a TLS-terminating proxy is in front,
# same position and same meaning as the systemd renderer's.
BEHIND_TLS_PROXY=${18:-}
# A non-empty 19th argument turns on WebAuthn (D71), same position
# and same meaning as the systemd renderer's.
WEBAUTHN=${19:-}
# A non-empty 20th argument is the one repository this node presents as
# its site (D78), same position and same meaning as the systemd
# renderer's. Presentation and not a grant: it decides what `/` renders,
# never who may read it, and a repository it hides is still clonable by
# whoever could clone it before.
SITE_REPO=${20:-}
# The release shelf, at 22 on both platforms so the shared arguments
# keep the same position (slot 21 is the Linux-only service user).
DOWNLOADS=${22:-}
if [ -n "$TLS_CERT$TLS_KEY" ] && { [ -z "$TLS_CERT" ] || [ -z "$TLS_KEY" ]; }; then
  echo "render_node_plist.sh: tls-cert and tls-key must be given together" >&2
  exit 2
fi
# `serve_single_repository` refuses a name this node could not hold, and
# that refusal is a node that will not start. Checking the shape here
# turns it into a renderer that will not render, on the same reasoning
# as the TLS pair above.
if [ -n "$SITE_REPO" ]; then
  case "$SITE_REPO" in
    */*/*|/*|*/) echo "render_node_plist.sh: site-repo must be owner/name" >&2; exit 2 ;;
    */*) : ;;
    *) echo "render_node_plist.sh: site-repo must be owner/name" >&2; exit 2 ;;
  esac
fi
# The shelf is a directory the node serves unauthenticated (D79), so the
# renderer refuses a value that is not an absolute path with no
# whitespace in it: this lands in an argument list beside the others, and
# a value that word-splits is a daemon launched with arguments nobody
# wrote.
if [ -n "$DOWNLOADS" ]; then
  case "$DOWNLOADS" in
    /*) : ;;
    *) echo "render_node_plist.sh: downloads-dir must be an absolute path with no spaces" >&2; exit 2 ;;
  esac
  # `[[:space:]]`, not `[\ \t]`: inside a shell bracket expression `\t`
  # is the letter t, so that spelling refused every path with a t in it.
  case "$DOWNLOADS" in
    *[[:space:]]*) echo "render_node_plist.sh: downloads-dir must be an absolute path with no spaces" >&2; exit 2 ;;
  esac
fi

# The served repos come from a file (one `owner/name.git` per line,
# `#` comments and blank lines skipped) rather than a single positional
# argument, so adding a repo is an appended line and a reinstall instead
# of a renderer signature change. An empty list is refused: a dogfood
# node with no --create serves nothing and installs no pre-receive hook,
# which reads like success until the first push is never sequenced.
[ -f "$REPOS_FILE" ] || { echo "render_node_plist.sh: no repos file at $REPOS_FILE" >&2; exit 1; }
repo_count=0
while IFS= read -r repo; do
  case $repo in ''|\#*) continue ;; esac
  repo_count=$((repo_count + 1))
done < "$REPOS_FILE"
[ "$repo_count" -gt 0 ] || { echo "render_node_plist.sh: $REPOS_FILE lists no repos" >&2; exit 1; }

cat <<PLIST_HEAD
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN</string>
    <string>$ROOT</string>
    <string>$PORT</string>
    <string>--auth-file</string><string>$AUTH</string>
    <string>--keys-file</string><string>$KEYS</string>
    <string>--reviewers-file</string><string>$REVIEWERS</string>
    <string>--newcomer-audit</string><string>$NEWCOMER_AUDIT</string>
    <string>--newcomer-adjudications</string><string>$NEWCOMER_ADJUDICATIONS</string>
PLIST_HEAD

if [ -n "$ACL" ]; then
  printf '    <string>--acl-file</string><string>%s</string>\n' "$ACL"
fi

if [ -n "$ACCOUNTS" ]; then
  printf '    <string>--accounts-file</string><string>%s</string>\n' "$ACCOUNTS"
fi

if [ -n "$SITE_REPO" ]; then
  printf '    <string>--site-repo</string><string>%s</string>\n' "$SITE_REPO"
fi
if [ -n "$DOWNLOADS" ]; then
  printf '    <string>--downloads-dir</string><string>%s</string>\n' "$DOWNLOADS"
fi

if [ -n "$BEHIND_TLS_PROXY" ]; then
  echo '    <string>--behind-tls-proxy</string>'
fi

if [ -n "$WEBAUTHN" ]; then
  printf '    <string>%s</string>\n' '--passkeys'
fi

if [ -n "$PROTECTED_REFS" ]; then
  cat <<PLIST_POLICY
    <string>--require-assignment</string>
    <string>--protected-refs</string><string>$PROTECTED_REFS</string>
    <string>--require-review</string>
PLIST_POLICY
fi

if [ -n "$REQUIRE_SCOPE" ]; then
  echo '    <string>--require-scope</string>'
fi

if [ -n "$TLS_CERT" ]; then
  cat <<PLIST_TLS
    <string>--bind</string><string>0.0.0.0</string>
    <string>--tls-cert</string><string>$TLS_CERT</string>
    <string>--tls-key</string><string>$TLS_KEY</string>
PLIST_TLS
else
  echo '    <string>--bind</string><string>127.0.0.1</string>'
fi
while IFS= read -r repo; do
  case $repo in ''|\#*) continue ;; esac
  printf '    <string>--create</string><string>%s</string>\n' "$repo"
done < "$REPOS_FILE"

cat <<PLIST_TAIL
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$LOG</string>
  <key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
PLIST_TAIL
