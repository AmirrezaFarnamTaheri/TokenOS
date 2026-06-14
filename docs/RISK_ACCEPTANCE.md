# TokenOS Closure And External-Control Register

Date: 2026-06-14

This register separates source-closed items from controls that require
repository administration, live credentials, OS policy, or deployment
infrastructure.

## Closed In Local Source

| ID | Item | Closure |
|---|---|---|
| C-01 | Native UI without webview advisories | `native` builds use egui/eframe with direct engine/store calls. Webview dependencies are not used. |
| C-02 | Browser dashboard retirement | `tokenos serve`, `src/webui.rs`, `static/`, and direct web-control dependencies have been removed. Provider adapters still use `reqwest` and its transitive HTTP stack. |
| C-03 | Native readiness and action workflow | The native app includes Readiness and Action Center panels for SQLite health, provider keys, spend ceilings, trace policy, cache policy, retired HTTP boundary, provider failures, estimator drift, and next operator actions. |
| C-04 | Provider cost forecast | Native route preview, batch planning, and policy simulation surface forecast provider spend and budget blocks before execution. |
| C-05 | Retired audit/report artifacts | Production guidance is maintained in `README.md` and `docs/`. |

## External Or Deployment-Specific Items

| ID | Item | Status | Required Operator Action |
|---|---|---|---|
| E-01 | Hosted GitHub branch protection | External verification required | Repository admin must require CI checks in GitHub branch protection or rulesets. |
| E-02 | Live provider API compatibility | Requires live credentials and spend approval | Verify OpenAI, Anthropic, Gemini, and proxy adapters in staging. |
| E-03 | Encryption at rest | Deployment-specific | Use OS disk encryption or add encrypted storage integration if required. |
| E-04 | Fleet-wide quota governance | External distributed-systems control | Use shared storage/gateway controls for independent hosts or regions. |
| E-05 | Live model IDs, prices, and provider contracts | Provider-controlled | Reconfirm configured model names, price assumptions, rate limits, and schemas before real spend. |
| E-06 | Monitoring, backups, and incident response | Deployment-specific | Add service monitoring, alerting, backup/restore procedures, and incident playbooks. |

## Release Position

TokenOS is finalized as a local-first execution kernel with native desktop UI,
CLI, library embedding, verified local quality gates, provider cost controls,
direct engine/store app integration, and no active browser control plane. It is
not claimed to control hosted repository settings, live provider behavior, OS
disk encryption, or independent fleet governance from inside this checkout.
