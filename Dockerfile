# A choir node in a container, built from source in a builder stage.
#
# The native path is `choir host` and it is the one to reach for first:
# it supervises the node with what the machine already has, and it can
# obtain a certificate, because certbot runs on the host where port 80
# and /etc/letsencrypt are. This file is for people whose machine is a
# container host and nothing else.
#
# It covers two of `choir host`'s three modes:
#
#   1. loopback, published to the host and reached over an ssh tunnel or
#      a proxy of your own
#   2. a name you own, with a certificate you already hold, mounted in
#
# It deliberately does not cover mode 3. There is no ACME client in the
# daemon and adding one is a real dependency against this workspace's
# bar, so issuing a certificate means certbot, and certbot inside a
# container means either a second container or port 80 bound through to
# it — a Caddy sidecar and a compose file to hold the two together. The
# honest arrangement for a container host is: terminate TLS at whatever
# already terminates it on that host, and run this on loopback behind
# it. That is mode 1 plus your own proxy, and it needs nothing here.
#
# Renewal, for mode 2: the daemon reads its certificate once, when it
# binds, and has no reload. A rotated pair reaches it only through a
# restart, so the host's certbot deploy hook must run
# `docker restart choir` (or your runtime's equivalent) after it copies
# the new pair into the mounted directory.
#
# Build:
#   docker build -t choir .
#
# Mode 1 — loopback on the host, nothing exposed to the network:
#   docker volume create choir-state
#   docker run -d --name choir \
#     -v choir-state:/var/lib/choir \
#     -p 127.0.0.1:8417:8417 \
#     choir
#
#   The first run creates the credential and keys in the volume. Read the
#   token, and create a repository:
#     docker exec choir cat /var/lib/choir/auth
#     docker exec choir choir --auth-file /var/lib/choir/auth \
#       repo create http://127.0.0.1:8417 me/thing.git
#
# Mode 2 — a name you own, with a certificate you already hold. Write the
# two-line marker into the volume naming the paths *as the container sees
# them*, and the daemon binds 0.0.0.0 with that pair. Nothing else
# changes; the marker is the same file `choir node tls` writes natively.
#   docker run -d --name choir \
#     -v choir-state:/var/lib/choir \
#     -v /etc/letsencrypt/live/node.example:/tls:ro \
#     -p 443:8417 \
#     choir
#   docker exec choir sh -c \
#     'printf "/tls/fullchain.pem\n/tls/privkey.pem\n" > /var/lib/choir/tls.enabled'
#   docker restart choir
#
# There is no compose file. The volume and the port are one flag each,
# which is what a compose file would have held.

# ---- build ------------------------------------------------------------
# From source, not from a release binary: this is the audited path, and a
# container that pulls a binary it did not build is a container whose
# provenance is an argument rather than a build log. The version is
# pinned to `rust-toolchain.toml`; bump them together.
FROM rust:1.97.1-bookworm AS build

# Every build in this workspace needs it, and the failure it prevents
# does not name itself. Same reason `.cargo/config.toml` sets it.
ENV LIBSQLITE3_FLAGS="-DSQLITE_ENABLE_BATCH_ATOMIC_WRITE"

WORKDIR /src
COPY . .
RUN cargo build --release -p choir-cli -p choir-node

# ---- run --------------------------------------------------------------
FROM debian:bookworm-slim

# git, because the node serves git smart-HTTP by execing
# `git http-backend`; curl and openssl, because every outbound request
# and every secret this workspace handles goes through one of them and
# `choir doctor` checks for all three. ca-certificates so an outbound
# https request can verify anything at all.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git curl openssl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/choir      /usr/local/bin/choir
COPY --from=build /src/target/release/choir-node /usr/local/bin/choir-node

# Unprivileged, and the state directory is its own rather than a home
# under /root. Nothing here needs root: the node binds 8417, not 443 —
# publish it to 443 on the host with `-p 443:8417` instead of giving the
# process a capability so it can bind a low port itself.
RUN useradd --system --create-home --home-dir /var/lib/choir --shell /usr/sbin/nologin choir
USER choir
WORKDIR /var/lib/choir

# The one thing that must outlive the container: keys, repositories, the
# op log, the credential, and the TLS marker. Everything else here is
# rebuildable from this Dockerfile.
VOLUME ["/var/lib/choir"]

EXPOSE 8417

# `choir host --foreground`, not `choir node serve`: the first run has no
# node in the volume, and `host` is the command that creates one. It
# skips what is already there, so every restart after the first is just a
# serve — and `--foreground` means it execs the daemon rather than
# looking for a service manager this image does not have. PID 1 is
# `choir-node` itself, so the runtime's stop signal and the daemon's
# exit 75 both reach the real process.
#
# `--yes` because there is nobody at a terminal to answer, and nothing to
# answer anyway: linger is a systemd --user question and there is no
# systemd here.
ENTRYPOINT ["choir", "host", "--state", "/var/lib/choir", "--foreground", "--yes"]
