#!/bin/sh
# Render the complete reverse-proxy boundary for the private beta.
# The node remains loopback-only; this is the only listener exposed by
# the firewall. Paths are printed into the configuration, never read here.
#
# This boundary handles no client address at all (D59). It writes no
# access log, keeps no per-address limit zone, and strips the forwarded
# address header rather than adding to it. What replaces the per-client
# ceiling is the node's own node-wide pre-auth counter
# (`choir_node::limits::PublicLimiter`), which needs no key.
set -eu

[ "$#" -eq 4 ] || {
  echo "usage: render_beta_nginx.sh <domain> <node-port> <tls-cert> <tls-key>" >&2
  exit 2
}

DOMAIN=$1
PORT=$2
TLS_CERT=$3
TLS_KEY=$4

case "$PORT" in
  ''|*[!0-9]*) echo "render_beta_nginx.sh: node port must be numeric" >&2; exit 2 ;;
esac
case "$DOMAIN" in
  ''|*/*|*' '*) echo "render_beta_nginx.sh: domain must be one hostname" >&2; exit 2 ;;
esac

cat <<NGINX
worker_processes auto;
pid /run/nginx-choir-beta.pid;

events {
    worker_connections 1024;
}

http {
    server_tokens off;

    # No access log. Every log format nginx can write for a request
    # begins with the client, and there is no variable spelling of "the
    # request without who made it". The record that survives is the
    # node's own \`--request-log\`, which captures method, path, user,
    # status, size and duration, and never had an address in it.
    access_log off;

    # The error log's line format is not configurable: nginx appends
    # \`client: <address>\` to request-level errors itself, so there is no
    # spelling of this log that keeps the diagnosis and drops the address.
    # Discarding it is the fail-closed choice. A level filter is not:
    # that would rest on no message above the threshold ever carrying a
    # client, which nginx does not document and we cannot check.
    #
    # Scoped to \`http\` deliberately, not to the main context. Request and
    # TLS-handshake errors are the ones that name a client, and they
    # inherit from here. Startup errors -- a bad certificate path, a
    # refused bind, a config that will not parse -- are raised in the main
    # context, name no client, and keep their default destination, so the
    # failures an operator actually has to read still land somewhere.
    # Moving this line above \`http\` would silence those too.
    error_log /dev/null crit;

    upstream choir_beta_node {
        server 127.0.0.1:$PORT;
        keepalive 16;
    }

    server {
        listen 80;
        listen [::]:80;
        server_name $DOMAIN;
        return 308 https://\$host\$request_uri;
    }

    server {
        listen 443 ssl;
        listen [::]:443 ssl;
        http2 on;
        server_name $DOMAIN;

        ssl_certificate $TLS_CERT;
        ssl_certificate_key $TLS_KEY;
        ssl_protocols TLSv1.2 TLSv1.3;
        ssl_session_cache shared:choir_tls:10m;
        ssl_session_timeout 1d;

        add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;
        add_header X-Content-Type-Options nosniff always;
        add_header X-Frame-Options DENY always;
        add_header Referrer-Policy same-origin always;
        add_header Permissions-Policy "camera=(), microphone=(), geolocation=()" always;

        # No Content-Security-Policy here, deliberately. \`add_header\`
        # appends rather than replaces, so a policy set here would arrive
        # alongside the node's own and both would be enforced, directive by
        # directive, at their strictest. The node sends a policy per page
        # and one of them is looser on purpose: the passkey pages carry
        # \`script-src 'self'; connect-src 'self'\` for the one script this
        # node has (D39). A blanket \`default-src 'none'\` here silently
        # revokes exactly that, and would do it only on the pages that
        # needed the exception. The node's own note gives the rule:
        # per page, never node-wide.
        #
        # \`X-Content-Type-Options\` above is the opposite case and stays:
        # it is genuinely node-wide, the node sends the identical value,
        # and a duplicate of a single-valued directive cannot narrow
        # anything.

        client_header_timeout 10s;
        client_body_timeout 30s;
        keepalive_timeout 65s;
        send_timeout 60s;
        client_max_body_size 1m;

        # Git smart HTTP has a separately chosen upload ceiling and must
        # stream to git-http-backend rather than buffer a pack on proxy disk.
        location ~ ^/[^/]+/[^/]+[.]git(?:/|\$) {
            client_max_body_size 512m;
            client_body_timeout 10m;
            proxy_read_timeout 10m;
            proxy_send_timeout 10m;
            proxy_request_buffering off;
            proxy_buffering off;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host \$host;
            proxy_set_header Authorization \$http_authorization;
            # Empty means nginx does not pass the field at all. Set rather
            # than omitted: omitting it forwards whatever the client sent,
            # so a caller could smuggle an address in under this name.
            proxy_set_header X-Forwarded-For "";
            proxy_set_header X-Forwarded-Proto https;
            proxy_pass http://choir_beta_node;
        }

        location / {
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host \$host;
            proxy_set_header Authorization \$http_authorization;
            proxy_set_header X-Forwarded-For "";
            proxy_set_header X-Forwarded-Proto https;
            proxy_connect_timeout 5s;
            proxy_read_timeout 60s;
            proxy_send_timeout 60s;
            proxy_pass http://choir_beta_node;
        }
    }
}
NGINX
