# TokenOS HTTP API Status

The TokenOS browser dashboard and HTTP API have been retired.

Active builds no longer include:

- `tokenos serve`;
- `src/webui.rs`;
- embedded `static/` dashboard assets;
- direct Axum server module and explicit web-control dependencies;
- `/api/*`, `/metrics`, or OpenAI-compatible proxy endpoints.

TokenOS is now operated through the native desktop app, CLI, or Rust library
embedding.

## Replacement Surface

| Retired HTTP use case | Supported replacement |
|---|---|
| Route preview | `tokenos route "<task>"` or the native Run Console |
| Execute a task | `tokenos run "<task>" [--workspace <path>]` or the native Run Console |
| Headline telemetry | `tokenos telemetry` or the native Dashboard |
| Provider attempts | `tokenos attempts` or the native Operations/Executions views |
| Task state | `tokenos tasks` or the native Tasks view |
| Flight-recorder timeline | `tokenos trace <task-id> [--blobs]` or the native Tasks view |
| Store/config health | `tokenos doctor [--json]` or the native Configuration view |
| Provider/model filter checks | `tokenos providers` or the native Configuration view |
| Route evaluation dataset | `tokenos eval --dataset <file>` or the native Calibration view |

## Embedding

Applications that need a programmable control surface should embed the library
crate and call `Engine`/`Store` APIs directly. The crate intentionally keeps the
kernel public through `src/lib.rs` so hosts can build their own process,
identity, transport, and authorization boundary without carrying a retired
HTTP server.
