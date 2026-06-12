//! Desktop launcher for the TokenOS dashboard (feature `native`).
//!
//! The launcher starts the existing Axum control plane on an ephemeral
//! loopback port and opens it in the user's system browser. It intentionally
//! avoids bundling a webview stack, so the optional native build does not pull
//! GTK/WebKitGTK dependencies into the supply chain.

use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::engine::Engine;
use crate::webui;

/// Launches the dashboard server on an ephemeral loopback port, opens the
/// system browser, and blocks until Ctrl+C.
///
/// This keeps `tokenos app` as a desktop-friendly entry point while using the
/// same audited dashboard bytes and auth/API model as `tokenos serve`.
pub fn run_app(engine: Arc<Engine>) -> Result<()> {
    let dry_run = engine.dry_run;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.spawn(async move {
        if let Err(e) = webui::serve_with_ready(engine, "127.0.0.1", 0, None, ready_tx).await {
            eprintln!("tokenos app: control plane terminated: {e:#}");
        }
    });

    let addr = rt
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), ready_rx).await
        })
        .map_err(|_| anyhow!("control plane did not become ready within 10s"))?
        .map_err(|_| anyhow!("control plane aborted before binding"))?;
    let url = format!("http://{addr}/");

    open_external(&url)?;
    eprintln!(
        "TokenOS app: opened {url} in the system browser (dry-run={dry_run}); press Ctrl+C to stop"
    );

    rt.block_on(async {
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| anyhow!("waiting for Ctrl+C: {e}"))
    })?;
    Ok(())
}

/// Best-effort "open in the system browser" without extra dependencies.
fn open_external(url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Ok(());
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    let (cmd, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);
    #[cfg(target_os = "macos")]
    let (cmd, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let (cmd, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);

    std::process::Command::new(cmd)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("opening {url}: {e}"))
}
