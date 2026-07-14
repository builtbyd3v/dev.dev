//! Generic (WarpUI-agnostic) half of the embedded browser-control MCP server.
//!
//! This crate owns everything that does not need to touch WarpUI's model/view
//! graph: bearer-token generation and on-disk storage, the rmcp tool-router
//! definitions for the seven browser-control tools, and the axum router that
//! wires those tools up behind loopback + bearer-token hardening.
//!
//! What it does *not* own: resolving a `BrowserPane` and driving its
//! `WebViewHost`. That needs `warpui`'s model graph (`ModelSpawner`, view
//! handles, pane groups), which lives in the `app` crate, not here — mirrors
//! how `local_control` (protocol/discovery) is UI-agnostic while
//! `app/src/local_control` owns the WarpUI-touching bridge. Callers implement
//! [`BrowserBridge`] for their own app-side bridge type and pass it to
//! [`build_router`].
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use local_control::AuthToken;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, schemars, tool, tool_handler, tool_router};

/// Fixed loopback port the browser MCP server binds. Fixed (not ephemeral) so a
/// `claude mcp add` registration only has to be done once per machine.
pub const BROWSER_MCP_PORT: u16 = 9287;

/// Everything the MCP tool layer needs from the embedded browser pane. Implemented
/// app-side by a type that resolves a `BrowserPane` through WarpUI's model graph
/// and drives its `WebViewHost`; this trait itself has no WarpUI dependency.
///
/// Uses return-position `impl Future` (stable, no `async-trait` dependency needed)
/// rather than `async fn` in a trait, so implementors can freely capture
/// non-`'static` borrows if ever needed and so this trait stays object-safety-free
/// (callers use it as a static generic bound, e.g. `BrowserMcpTools<B>`, not `dyn`).
pub trait BrowserBridge: Send + Sync + 'static {
    /// Navigates the browser pane to `url`, opening one first if none exists.
    /// Returns the URL the pane reports immediately after navigating.
    fn navigate(&self, url: String) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// Returns the current page URL. Errors if no browser pane is open.
    fn current_url(&self) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// Captures a screenshot of the page as PNG bytes.
    fn screenshot(&self) -> impl Future<Output = anyhow::Result<Vec<u8>>> + Send;
    /// Resolves `selector` to its element center and dispatches a synthetic click there.
    fn click(&self, selector: String) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Focuses `selector` (if given) then inserts `text` at the current caret.
    fn type_text(
        &self,
        text: String,
        selector: Option<String>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Returns recent console entries as formatted text, optionally filtered by level.
    fn console(&self, level: Option<String>) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// Runs arbitrary JS in the page and returns its JSON-serialized result.
    fn evaluate(&self, js: String) -> impl Future<Output = anyhow::Result<String>> + Send;
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct NavigateRequest {
    #[schemars(description = "URL to navigate the browser pane to")]
    pub url: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ClickRequest {
    #[schemars(description = "CSS selector of the element to click")]
    pub selector: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TypeRequest {
    #[schemars(description = "Text to type at the current focus/caret")]
    pub text: String,
    #[schemars(description = "Optional CSS selector to focus before typing")]
    pub selector: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ConsoleRequest {
    #[schemars(description = "Optional level filter, e.g. \"error\"; omit for all recent entries")]
    pub level: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EvaluateRequest {
    #[schemars(description = "JavaScript to evaluate in the page's own context")]
    pub js: String,
}

/// rmcp `ServerHandler` exposing the seven browser-control tools, generic over
/// whatever [`BrowserBridge`] the embedding app provides. Monomorphized per
/// bridge type, so this never needs `dyn` dispatch.
#[derive(Clone)]
pub struct BrowserMcpTools<B: BrowserBridge> {
    bridge: Arc<B>,
    tool_router: ToolRouter<Self>,
}

impl<B: BrowserBridge> BrowserMcpTools<B> {
    pub fn new(bridge: Arc<B>) -> Self {
        Self {
            bridge,
            tool_router: Self::tool_router(),
        }
    }
}

/// Wraps a business-logic failure (pane not found, click missed, CDP error, ...) as
/// a tool-level error result rather than a protocol-level one, so the calling model
/// sees the error text and can retry/adjust instead of the whole request failing.
fn tool_error(err: anyhow::Error) -> CallToolResult {
    CallToolResult::error(vec![Content::text(err.to_string())])
}

#[tool_router]
impl<B: BrowserBridge> BrowserMcpTools<B> {
    #[tool(
        description = "Navigate the embedded browser pane to a URL. Opens a browser pane first if none is open yet."
    )]
    async fn browser_navigate(
        &self,
        Parameters(NavigateRequest { url }): Parameters<NavigateRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(match self.bridge.navigate(url).await {
            Ok(final_url) => CallToolResult::success(vec![Content::text(final_url)]),
            Err(err) => tool_error(err),
        })
    }

    #[tool(description = "Get the embedded browser pane's current page URL.")]
    async fn browser_current_url(&self) -> Result<CallToolResult, ErrorData> {
        Ok(match self.bridge.current_url().await {
            Ok(url) => CallToolResult::success(vec![Content::text(url)]),
            Err(err) => tool_error(err),
        })
    }

    #[tool(description = "Capture a PNG screenshot of the embedded browser pane's current page.")]
    async fn browser_screenshot(&self) -> Result<CallToolResult, ErrorData> {
        Ok(match self.bridge.screenshot().await {
            Ok(png_bytes) => {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD.encode(png_bytes);
                CallToolResult::success(vec![Content::image(data, "image/png")])
            }
            Err(err) => tool_error(err),
        })
    }

    #[tool(
        description = "Click the element matched by a CSS selector in the embedded browser pane (scrolls it into view first)."
    )]
    async fn browser_click(
        &self,
        Parameters(ClickRequest { selector }): Parameters<ClickRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(match self.bridge.click(selector).await {
            Ok(()) => CallToolResult::success(vec![Content::text("clicked")]),
            Err(err) => tool_error(err),
        })
    }

    #[tool(
        description = "Type text at the current focus in the embedded browser pane. Pass `selector` to focus an element first."
    )]
    async fn browser_type(
        &self,
        Parameters(TypeRequest { text, selector }): Parameters<TypeRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(match self.bridge.type_text(text, selector).await {
            Ok(()) => CallToolResult::success(vec![Content::text("typed")]),
            Err(err) => tool_error(err),
        })
    }

    #[tool(
        description = "Read recent browser console entries from the embedded browser pane. Pass level=\"error\" to see only errors."
    )]
    async fn browser_console(
        &self,
        Parameters(ConsoleRequest { level }): Parameters<ConsoleRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(match self.bridge.console(level).await {
            Ok(text) => CallToolResult::success(vec![Content::text(text)]),
            Err(err) => tool_error(err),
        })
    }

    #[tool(
        description = "Evaluate arbitrary JavaScript in the embedded browser pane's page context and return its result. \
        Power tool: runs with the full authority of the current page (cookies, storage, same-origin fetches) — only \
        use it for scripts you trust, and prefer the other tools when they cover what you need."
    )]
    async fn browser_evaluate(
        &self,
        Parameters(EvaluateRequest { js }): Parameters<EvaluateRequest>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(match self.bridge.evaluate(js).await {
            Ok(result) => CallToolResult::success(vec![Content::text(result)]),
            Err(err) => tool_error(err),
        })
    }
}

#[tool_handler]
impl<B: BrowserBridge> ServerHandler for BrowserMcpTools<B> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Controls the browser pane embedded in the Warp terminal app: navigate, read the \
             current URL, screenshot, click, type, read console output, and evaluate JS.",
        )
    }
}

/// Loads the persisted bearer token from `<local_control discovery dir's parent>/browser-mcp/token`
/// if it exists and is non-empty, else generates a fresh one and writes it there (0600,
/// owner-only, best-effort on Windows — see below). Persisting (rather than regenerating on every
/// launch) keeps a one-time `claude mcp add` registration valid across Warp restarts instead of
/// forcing the user to re-run it every time.
///
/// Reuses `local_control`'s `AuthToken` (32 bytes of OS CSPRNG output) rather than rolling a
/// separate token type, and anchors the storage directory off `local_control::discovery_dir()`
/// rather than inventing a new env-var/HOME resolution scheme, since that helper already
/// encodes the right per-platform "where does Warp keep its local state" answer.
///
/// Windows note: `std::fs::Permissions`/`set_mode` is a Unix-only concept. On Windows this
/// function relies solely on the file living under the user's own profile directory tree for
/// protection; it does not set an ACL. That's a real (documented) gap versus the 0600 guarantee
/// unix gets — tightening it would mean pulling in Windows ACL APIs, out of scope for v1.
pub fn generate_and_store_token() -> anyhow::Result<(AuthToken, PathBuf)> {
    let dir = browser_mcp_dir();
    std::fs::create_dir_all(&dir)?;
    let token_path = dir.join("token");
    if let Ok(existing) = std::fs::read_to_string(&token_path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok((AuthToken::from_secret(existing), token_path));
        }
    }
    let token = AuthToken::generate();
    std::fs::write(&token_path, token.secret())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((token, token_path))
}

/// Directory the browser-MCP token file lives in: a `browser-mcp` sibling of
/// `local_control::discovery_dir()` (which resolves to e.g. `~/.warp/local-control`), so both
/// land under the same `~/.warp` root without this crate reimplementing that resolution.
fn browser_mcp_dir() -> PathBuf {
    let discovery_dir = local_control::discovery_dir();
    let warp_dir = discovery_dir.parent().unwrap_or(&discovery_dir);
    warp_dir.join("browser-mcp")
}

/// Formats the exact `claude mcp add` invocation a user can paste to register this server,
/// bearer token baked in so it's a genuine one-shot copy-paste.
pub fn claude_mcp_add_command(token: &AuthToken, port: u16) -> String {
    format!(
        "claude mcp add --transport http warp-browser http://127.0.0.1:{port}/mcp --header \"Authorization: Bearer {}\"",
        token.secret()
    )
}

/// Builds the axum router serving the MCP endpoint at `/mcp`, hardened with loopback-header
/// validation (reject any `Origin`, require exact `Host` match) and bearer-token auth, mirroring
/// `app/src/local_control/mod.rs`'s `validate_loopback_headers` + `Authorization: Bearer` checks —
/// deliberately unconditional here (every request, not just non-loopback-looking ones), since this
/// server has no separate credential-broker handshake local-control uses to scope grants.
pub fn build_router<B: BrowserBridge>(
    bridge: Arc<B>,
    token: AuthToken,
    expected_host: String,
) -> axum::Router {
    let bridge_for_factory = bridge.clone();
    let service_factory = move || Ok(BrowserMcpTools::new(bridge_for_factory.clone()));
    let session_manager = Arc::new(LocalSessionManager::default());
    let config = StreamableHttpServerConfig::default();
    let service = StreamableHttpService::new(service_factory, session_manager, config);

    let auth_state = Arc::new(AuthState {
        token,
        expected_host,
    });
    axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            auth_and_loopback_middleware,
        ))
}

struct AuthState {
    token: AuthToken,
    expected_host: String,
}

async fn auth_and_loopback_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AuthState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::http::header::{AUTHORIZATION, HOST, ORIGIN};
    use axum::response::IntoResponse as _;

    let headers = request.headers();
    if headers.contains_key(ORIGIN) {
        return (
            StatusCode::FORBIDDEN,
            "browser-origin requests are not allowed",
        )
            .into_response();
    }
    let host = headers.get(HOST).and_then(|value| value.to_str().ok());
    if host != Some(state.expected_host.as_str()) {
        return (
            StatusCode::FORBIDDEN,
            "Host header does not match the browser-MCP endpoint",
        )
            .into_response();
    }
    let auth_header = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if let Err(error) = state.token.verify_authorization_header(auth_header) {
        return (StatusCode::UNAUTHORIZED, error.to_string()).into_response();
    }

    next.run(request).await
}
