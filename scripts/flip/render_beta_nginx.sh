#!/bin/sh
# Render the complete reverse-proxy boundary for the private beta.
# The node remains loopback-only; this is the only listener exposed by
# the firewall. Paths are printed into the configuration, never read here.
set -eu

[ "$#" -eq 5 ] || {
  echo "usage: render_beta_nginx.sh <domain> <node-port> <tls-cert> <tls-key> <access-log>" >&2
  exit 2
}

DOMAIN=$1
PORT=$2
TLS_CERT=$3
TLS_KEY=$4
ACCESS_LOG=$5

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
    log_format choir_beta '\$remote_addr - \$remote_user [\$time_local] "\$request" '
                           '\$status \$body_bytes_sent "\$http_referer" "\$http_user_agent" '
                           'rt=\$request_time urt=\$upstream_response_time';
    access_log $ACCESS_LOG choir_beta;

    limit_req_zone \$binary_remote_addr zone=choir_pre_auth:10m rate=10r/s;
    limit_conn_zone \$binary_remote_addr zone=choir_connections:10m;

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
        add_header Referrer-Policy no-referrer always;
        add_header Permissions-Policy "camera=(), microphone=(), geolocation=()" always;
        add_header Content-Security-Policy "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'" always;

        client_header_timeout 10s;
        client_body_timeout 30s;
        keepalive_timeout 65s;
        send_timeout 60s;
        client_max_body_size 1m;
        limit_conn choir_connections 20;
        limit_req zone=choir_pre_auth burst=30 nodelay;

        # Git smart HTTP has a separately chosen upload ceiling and must
        # stream to git-http-backend rather than buffer a pack on proxy disk.
        location ~ ^/[^/]+/[^/]+[.]git(?:/|$) {
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
            proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
            proxy_set_header X-Forwarded-Proto https;
            proxy_pass http://choir_beta_node;
        }

        location / {
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host \$host;
            proxy_set_header Authorization \$http_authorization;
            proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
            proxy_set_header X-Forwarded-Proto https;
            proxy_connect_timeout 5s;
            proxy_read_timeout 60s;
            proxy_send_timeout 60s;
            proxy_pass http://choir_beta_node;
        }
    }
}
NGINX
