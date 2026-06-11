//! Native desktop shell for the TokenOS dashboard (feature `native`).
//!
//! Architecture: the existing axum control plane binds an EPHEMERAL loopback
//! port (127.0.0.1:0) on a background tokio runtime, then a system-webview
//! window (wry over WebKitGTK / WebView2 / WKWebView) is pointed at it. The
//! dashboard, its API, auth model and engine are byte-identical to
//! `tokenos serve` — the shell adds a native window, not a second frontend.
//!
//! Security posture:
//!   * loopback-only bind — the kernel never faces a network in app mode
//!   * ephemeral port — no fixed local port for other software to squat or
//!     probe ahead of launch
//!   * the window closing tears the whole process (and thus the server) down
//!
//! Headless/server builds never compile this module (no GTK linkage): the
//! `native` cargo feature gates both the module and its dependencies.

use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::engine::Engine;
use crate::webui;

/// Launches the dashboard server on an ephemeral loopback port and opens a
/// native webview window over it. Blocks until the window is closed.
///
/// Must be called from a plain (non-tokio) main thread: the tao event loop
/// owns the thread, and the axum server runs on its own runtime thread.
pub fn run_app(engine: Arc<Engine>) -> Result<()> {
    use tao::{
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoop},
        window::WindowBuilder,
    };
    use wry::WebViewBuilder;

    let dry_run = engine.dry_run;

    // 1. Control plane on a background runtime, ephemeral loopback port.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    // The server future is owned by the runtime; dropping the runtime at
    // the end of this function aborts it deterministically.
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

    // 2. Native window + system webview over the loopback dashboard.
    let event_loop = EventLoop::new();
    let title = format!(
        "TokenOS {} — {}",
        env!("CARGO_PKG_VERSION"),
        if dry_run { "Dry-Run (offline)" } else { "Live" }
    );
    let window = WindowBuilder::new()
        .with_title(&title)
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 860.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(720.0, 480.0))
        .build(&event_loop)
        .map_err(|e| anyhow!("window creation failed: {e}"))?;

    let nav_url = url.clone();
    let builder = WebViewBuilder::new()
        .with_url(&url)
        // The shell is a kiosk over OUR loopback server: external links the
        // model might emit must open in the system browser, never navigate
        // the control panel away from the kernel.
        .with_navigation_handler(move |target: String| {
            let internal = target.starts_with(&nav_url);
            if !internal {
                let _ = open_external(&target);
            }
            internal
        });

    // Linux/tao: wry must attach to the window's GTK container (the
    // raw-window-handle path is X11-only and panics under Wayland);
    // everywhere else the plain window handle is the supported path.
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    let _webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window
            .default_vbox()
            .ok_or_else(|| anyhow!("tao window has no GTK vbox"))?;
        builder
            .build_gtk(vbox)
            .map_err(|e| anyhow!("webview creation failed: {e}"))?
    };
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    let _webview = builder
        .build(&window)
        .map_err(|e| anyhow!("webview creation failed: {e}"))?;

    eprintln!("TokenOS app: dashboard at {url} (loopback only)");

    // 3. Event loop owns the thread; window close exits the process, which
    // drops the runtime and with it the server.
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
        }
    });
}

/// Best-effort "open in the system browser" without extra dependencies.
fn open_external(url: &str) -> Result<()> {
    // Only ever hand http(s) URLs to the OS opener — anything else (file://,
    // custom schemes) is dropped. Defense against a model emitting links
    // that would invoke local handlers.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    std::process::Command::new(cmd)
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow!("opening {url}: {e}"))
}
