//! App-side half of the embedded browser-control MCP server.
//!
//! Mirrors `app/src/local_control/mod.rs`'s shape: this module owns the SingletonEntity that
//! binds a loopback listener on its own single-worker tokio runtime, while the WarpUI-touching
//! bridge logic (resolving a `BrowserPane` and driving its `WebViewHost`) lives in `bridge.rs`.
//! Everything protocol/transport-generic (token generation+storage, the rmcp tool router, the
//! axum router with loopback+bearer hardening) lives in the standalone `browser_mcp` crate,
//! which has no WarpUI dependency — same split as `local_control` (crate) vs
//! `app/src/local_control` (this module's sibling).
mod bridge;

use std::net::SocketAddr;
use std::sync::Arc;

use warpui::{Entity, ModelContext, ModelSpawner, SingletonEntity};

pub use bridge::AppBrowserBridge;
use bridge::WarpBrowserBridge;

/// Master switch for the embedded browser-control MCP server. Flip to `false` to pull it out of
/// the singleton graph entirely without touching the `lib.rs` call site.
///
/// TODO(follow-up): promote to a real Settings > Scripting-style user toggle once this ships
/// past v1, mirroring `local_control`'s `LocalControlSettings` gate — v1 ships always-on (when
/// this const is `true`) since there's no settings UI wired up yet.
pub const BROWSER_MCP_ENABLED: bool = true;

/// Process-local listener for one Warp instance's browser-control MCP server. Holding the
/// runtime alive keeps the listener alive; dropping it (e.g. on app shutdown) stops serving.
pub struct BrowserMcpServer {
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Entity for BrowserMcpServer {
    type Event = ();
}

impl SingletonEntity for BrowserMcpServer {}

impl BrowserMcpServer {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        let bridge_spawner: ModelSpawner<AppBrowserBridge> =
            AppBrowserBridge::handle(ctx).update(ctx, |_, ctx| ctx.spawner());
        match Self::start(bridge_spawner) {
            Ok(runtime) => Self {
                _runtime: Some(runtime),
            },
            Err(error) => {
                log::warn!("Failed to start browser-control MCP server: {error:#}");
                Self { _runtime: None }
            }
        }
    }

    /// Binds the fixed loopback port, generates a fresh bearer token, and serves the MCP
    /// endpoint. Fixed (not ephemeral) port so a `claude mcp add` registration is a one-time
    /// setup step rather than something the user has to redo on every Warp restart.
    fn start(
        bridge_spawner: ModelSpawner<AppBrowserBridge>,
    ) -> anyhow::Result<tokio::runtime::Runtime> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_io()
            .enable_time()
            .build()?;
        let port = browser_mcp::BROWSER_MCP_PORT;
        let listener = runtime.block_on(tokio::net::TcpListener::bind(SocketAddr::from((
            [127, 0, 0, 1],
            port,
        ))))?;
        let (token, token_path) = browser_mcp::generate_and_store_token()?;
        let expected_host = format!("127.0.0.1:{port}");
        let bridge = Arc::new(WarpBrowserBridge::new(bridge_spawner));
        let router = browser_mcp::build_router(bridge, token.clone(), expected_host);
        runtime.spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                log::warn!("browser-control MCP listener stopped: {err:#}");
            }
        });
        log::info!(
            "browser-control MCP server listening at http://127.0.0.1:{port}/mcp (bearer token \
             stored at {}). Register it once with:\n{}",
            token_path.display(),
            browser_mcp::claude_mcp_add_command(&token, port)
        );
        Ok(runtime)
    }
}
