# choir agent environment — source from your shell profile or harness
# config. Every template in this directory keys off these variables.
#
# Only CHOIR_API is required; the rest have sensible defaults or are
# optional until the daemon has auth/signing enabled.

# Base URL of the choir-node daemon (git + platform API live here).
export CHOIR_API="${CHOIR_API:-http://127.0.0.1:8417}"

# Git basic-auth credentials used by choir_remote. CHOIR_TOKEN_FILE contains
# the token only. The token comes from the operator; never commit it.
#export CHOIR_USER="agent-name"
#export CHOIR_TOKEN_FILE="$HOME/.choir/token"

# The agent's own ed25519 key (32 secret bytes) for signed ops and
# `git push --signed`. Created by the operator; chmod 600.
#export CHOIR_KEY_FILE="$HOME/.choir/agent.key"

# ssh-format signing key for push certificates (same curve; used by
# `git push --signed` with gpg.format=ssh).
#export CHOIR_SSH_KEY="$HOME/.choir/agent_ed25519"

# Convenience: remote URL for a repo on this node.
# Usage: git clone "$(choir_remote owner/repo)"
choir_remote() {
    if [ -n "$CHOIR_USER" ] && [ -n "$CHOIR_TOKEN_FILE" ] && [ -f "$CHOIR_TOKEN_FILE" ]; then
        host="${CHOIR_API#*://}"
        scheme="${CHOIR_API%%://*}"
        printf '%s://%s:%s@%s/%s.git\n' "$scheme" "$CHOIR_USER" "$(cat "$CHOIR_TOKEN_FILE")" "$host" "$1"
    else
        printf '%s/%s.git\n' "$CHOIR_API" "$1"
    fi
}
