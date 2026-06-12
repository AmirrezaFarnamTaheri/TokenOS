# TokenOS Production Deployment Guide

This guide describes how to deploy the **TokenOS** local execution kernel securely in a remote or server environment.

By default, TokenOS binds to loopback (`127.0.0.1`). For remote access, configure a non-empty bearer token and either run TokenOS with native HTTPS (`--tls-cert` and `--tls-key`) or place it behind a secure reverse proxy such as Nginx/Caddy/Apache with TLS enabled.

---

## 1. Nginx Reverse Proxy with TLS Configuration

Below is a template Nginx configuration. It terminates TLS (HTTPS), forwards requests to the local TokenOS daemon, and enforces security headers.

Create a site configuration file (e.g., `/etc/nginx/sites-available/tokenos`) with the following content:

```nginx
server {
    listen 80;
    server_name tokenos.example.com;
    
    # Redirect all HTTP traffic to HTTPS
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl http2;
    server_name tokenos.example.com;

    # SSL Certificate Paths (managed by Let's Encrypt / Certbot)
    ssl_certificate /etc/letsencrypt/live/tokenos.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/tokenos.example.com/privkey.pem;

    # Harden SSL configurations
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_prefer_server_ciphers on;
    ssl_ciphers 'ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384';

    # Security Headers (Axum adds these natively, but Nginx reinforces them)
    add_header X-Frame-Options "DENY" always;
    add_header X-Content-Type-Options "nosniff" always;
    add_header Referrer-Policy "no-referrer" always;
    add_header Strict-Transport-Security "max-age=63072000; includeSubDomains; preload" always;

    # Max body payload limit matching the TokenOS backend default (256 KiB)
    client_max_body_size 256k;

    # Location blocks for TokenOS Web UI and REST API
    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;
        
        # /api/run can run for up to 300 seconds server-side.
        proxy_read_timeout 310s;
        proxy_send_timeout 310s;
        
        # Standard proxy headers
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

Enable the configuration and reload Nginx:
```bash
sudo ln -s /etc/nginx/sites-available/tokenos /etc/nginx/sites-enabled/
sudo nginx -t
sudo systemctl reload nginx
```

---

## 1.1 Native HTTPS Serving

TokenOS can also terminate TLS directly when you provide PEM certificate and
key files:

```bash
TOKENOS_AUTH_TOKEN=your-super-secure-token-here \
  tokenos serve \
  --host 0.0.0.0 \
  --port 8443 \
  --public \
  --tls-cert /etc/letsencrypt/live/tokenos.example.com/fullchain.pem \
  --tls-key /etc/letsencrypt/live/tokenos.example.com/privkey.pem
```

Use a reverse proxy when you need HTTP-to-HTTPS redirects, HSTS management,
centralized access logging, WAF controls, or multiple backend services. Native
HTTPS is useful for a minimal single-service deployment where those controls
are handled elsewhere.

---

## 2. Systemd Service Configuration

To run the TokenOS dashboard as a persistent background service daemon on Linux, use systemd.

Create a systemd unit file at `/etc/systemd/system/tokenos.service`:

```ini
[Unit]
Description=TokenOS Orchestration Daemon
After=network.target

[Service]
Type=simple
User=tokenos
Group=tokenos
WorkingDirectory=/var/lib/tokenos

# Load configuration and secrets from environment variables file
EnvironmentFile=/etc/tokenos/tokenos.env

# Execute tokenos server binding only to localhost
ExecStart=/usr/local/bin/tokenos serve --host 127.0.0.1 --port 3000 --auth-token ${TOKENOS_AUTH_TOKEN}

Restart=always
RestartSec=5

# Restrict permissions
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/tokenos
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
```

Create the system user and configuration directories:
```bash
sudo useradd -r -s /bin/false tokenos
sudo mkdir -p /var/lib/tokenos /etc/tokenos
sudo chown -R tokenos:tokenos /var/lib/tokenos
```

Create `/etc/tokenos/tokenos.env` with your authentication token and provider secrets:
```ini
TOKENOS_AUTH_TOKEN=your-super-secure-token-here
# Optional provider credentials (example)
OPENAI_API_KEY=sk-proj-xxxx
ANTHROPIC_API_KEY=sk-ant-xxxx
```

Set secure permissions on the environment file:
```bash
sudo chown tokenos:tokenos /etc/tokenos/tokenos.env
sudo chmod 600 /etc/tokenos/tokenos.env
```

Start and enable the service:
```bash
sudo systemctl daemon-reload
sudo systemctl enable --now tokenos
sudo systemctl status tokenos
```

---

## 3. Remote Serving Best Practices

1. **Never Bind Publicly Without Auth**: If you bind the service directly (e.g. `serve --host 0.0.0.0`), you **must** supply a non-empty bearer token (via `--auth-token`) otherwise the server will refuse to start.
2. **TLS Enforced**: Use either native TokenOS HTTPS (`--tls-cert`/`--tls-key`) or proxy remote traffic through TLS (port 443) using Nginx, Apache, or Caddy. Transmitting bearer tokens or API execution payloads over plain HTTP exposes them to eavesdropping.
3. **Database and trace permissions**: Standard SQLite and traces are stored in the user profile directory. If running as a system service, ensure `/var/lib/tokenos` is locked down with owner-only access permissions (`chmod 700`).
4. **Scoped API Tokens**: Instead of sharing the single master/admin CLI token, use the `security.api_tokens` section in `config.yaml` to define granular, scoped credentials (e.g. `read`-only access for dashboards, `run` access for automation/agents, and `admin` for operations).
5. **Shared API Token Rate Limits**: Set `security.api_token_rate_limit_per_min` to enforce a per-token request ceiling. The ledger is stored in SQLite by token hash, so multiple TokenOS processes using the same DB coordinate the limit.
6. **Live Provider Staging**: Verify provider model names, prices, schemas, and rate-limit behavior with real credentials in staging before allowing production spend.
7. **Backups and Retention**: Back up the SQLite database if task state matters operationally. Configure `security.retention_days` to keep trace and telemetry volume aligned with your data-retention policy.
