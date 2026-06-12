# TokenOS Closure and External-Control Register

Date: 2026-06-13

This register separates items closed in local source from items that require
repository administration, live credentials, certificates, or deployment
infrastructure outside this checkout.

## Closed In Local Source

| ID | Item | Closure |
|---|---|---|
| C-01 | Optional native GTK3-family dependency advisories | Removed `tao`/`wry` webview dependencies. The `native` feature now builds a zero-extra-dependency desktop launcher that opens the loopback dashboard in the system browser. `cargo audit` no longer reports the GTK3-family or `glib` advisories. |
| C-02 | Native HTTPS serving | `tokenos serve` supports `--tls-cert` and `--tls-key` PEM files for direct HTTPS serving. Reverse proxies remain useful for redirects, HSTS policy, WAFs, and centralized logging. |
| C-03 | Shared API-token request limits | `security.api_token_rate_limit_per_min` enables a SQLite-backed per-token per-minute request ledger. Tokens are stored by SHA-256 hash, so multiple TokenOS processes using the same DB coordinate API request limits. |

## External Or Deployment-Specific Items

| ID | Item | Status | Required Operator Action |
|---|---|---|---|
| E-01 | Hosted GitHub branch protection / required checks | External verification required | Repository admin must require CI checks in GitHub branch protection or rulesets. This checkout has no authenticated GitHub CLI token, so hosted enforcement cannot be proven locally. |
| E-02 | Live provider API compatibility | Requires live credentials and spend approval | Verify OpenAI, Anthropic, Gemini, and proxy adapters with real credentials in a controlled staging environment before production use. |
| E-03 | Encryption at rest | Deployment-specific | Use OS disk encryption today, or add SQLCipher/keychain integration if application-level DB encryption is required for the deployment. |
| E-04 | TLS certificate operations | Deployment-specific | Supply valid PEM files to `--tls-cert`/`--tls-key`, or terminate TLS at a reverse proxy/load balancer. |
| E-05 | Fleet-wide quota governance across independent hosts/databases | External distributed-systems control | Use one shared TokenOS DB for the built-in per-token request ledger, or deploy an external gateway/rate limiter for fleets with separate databases or regions. |

## Release Position

TokenOS is finalized as a local-first execution kernel with verified local
quality gates, native HTTPS support, scoped bearer tokens, shared per-token API
request limits for a shared DB, and no accepted native GTK advisory path.
It is not claimed to control GitHub-hosted branch settings, live provider API
behavior, certificate issuance, or independent multi-region quota ledgers from
inside this local checkout.
