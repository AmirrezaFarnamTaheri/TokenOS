# TokenOS Risk Acceptance Register

Date: 2026-06-13

This register separates completed local remediation from items that are external, deployment-specific, or intentionally risk-accepted for the current local-first release posture.

## Accepted Or External Items

| ID | Item | Status | Rationale | Required Operator Action |
|---|---|---|---|---|
| RA-01 | Optional native GTK3-family dependency advisories | Accepted for optional `native` feature | `cargo audit` reports no known vulnerabilities, but reports unmaintained GTK3-family crates and one `glib` unsound advisory pulled through optional desktop webview dependencies. Default/headless builds do not enable `native`. | Treat native builds as a separately reviewed release artifact; monitor `wry`/GTK ecosystem updates before broad Linux desktop distribution. |
| RA-02 | Hosted GitHub branch protection / required checks | External verification required | The workflow exists locally and now triggers for `main`, `development`, `feat/**`, `fix/**`, and `codex/**`, but branch-protection settings live in GitHub. This checkout has no authenticated GitHub CLI token, so enforcement cannot be proven locally. | Repository admin must require the CI checks in GitHub branch protection or rulesets. |
| RA-03 | Live provider API compatibility | Out of local scope | Local tests use the mock provider and do not spend live provider tokens. Live API schemas, model IDs, and pricing can drift. | Verify OpenAI, Anthropic, Gemini, and proxy adapters with real credentials in a controlled staging environment before production use. |
| RA-04 | Encryption at rest | Deployment-specific | TokenOS masks durable secrets where designed, supports trace disablement, startup retention pruning, and Unix owner-only permissions. It does not embed SQLCipher or OS keychain encryption. | Use OS disk encryption or add SQLCipher/keychain integration for sensitive deployments. |
| RA-05 | Native TLS | Deployment-specific | TokenOS intentionally serves plain HTTP locally and relies on loopback binding plus bearer auth. | Terminate TLS at a reverse proxy such as Nginx, Caddy, Apache, or a cloud load balancer for remote access. |
| RA-06 | Distributed rate limits and multi-process quota coordination | Future production work | Current controls are process-local concurrency limits, scoped bearer tokens, and SQLite daily/monthly spend ceilings. They do not coordinate quotas across multiple TokenOS processes or hosts. | Use an external gateway/rate limiter for distributed deployments, or implement shared quota storage before multi-tenant use. |

## Release Position

TokenOS is finalized as a local-first, single-user execution kernel with verified local quality gates. It is not claimed to be a native multi-tenant cloud platform without the external controls listed above.
