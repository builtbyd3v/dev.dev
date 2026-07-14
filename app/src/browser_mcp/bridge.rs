//! WarpUI-touching half of the browser-control MCP bridge: resolves the target
//! `BrowserPane` through the model/view graph and drives its `WebViewHost`.
use std::future::Future;

use browser_mcp::BrowserBridge;
use warpui::{AppContext, Entity, ModelContext, ModelSpawner, SingletonEntity, ViewHandle};

use crate::pane_group::BrowserPane;
use crate::pane_group::pane::browser_pane::BrowserView;
use crate::workspace::{Workspace, WorkspaceRegistry};

/// Minimal singleton whose only job is to vend a `ModelSpawner<Self>` — the mechanism
/// `local_control`'s bridge uses to cross from the MCP server's tokio thread back onto the UI
/// thread (see `app/src/local_control/mod.rs`'s `ControlServerState::bridge_spawner`). Holds no
/// state of its own; `WarpBrowserBridge` (below) is the thing that actually implements
/// `browser_mcp::BrowserBridge` and gets handed to the generic MCP tool router.
pub struct AppBrowserBridge;

impl Entity for AppBrowserBridge {
    type Event = ();
}

impl SingletonEntity for AppBrowserBridge {}

impl AppBrowserBridge {
    pub fn new(_ctx: &mut ModelContext<Self>) -> Self {
        Self
    }
}

const NO_PANE_ERROR: &str = "No browser pane is open. Call browser_navigate first.";

/// Implements `browser_mcp::BrowserBridge` by crossing onto the UI thread via `ModelSpawner`
/// for each call. Two call shapes, matching the design doc's two-hop pattern:
///
/// - Synchronous `WebViewHost` getters/`load_url` (navigate, current_url, console): resolved
///   and returned entirely inside the spawned closure — no CDP round-trip involved.
/// - CDP-callback-based `WebViewHost` methods (screenshot, click, type, evaluate): the spawned
///   closure creates a `futures::channel::oneshot`, fires the CDP call with a callback that
///   fulfils it, and returns the *receiver* — so `ModelSpawner::spawn`'s returned future
///   resolves as soon as the receiver exists (i.e. once the UI-thread work registering the CDP
///   call is done), not once the CDP round-trip itself completes. The caller then awaits that
///   receiver afterward, on the tokio side, off the UI thread — the CDP completion callback
///   fires later from WebView2's COM-STA loop and fulfils it then.
pub struct WarpBrowserBridge {
    spawner: ModelSpawner<AppBrowserBridge>,
}

impl WarpBrowserBridge {
    pub fn new(spawner: ModelSpawner<AppBrowserBridge>) -> Self {
        Self { spawner }
    }
}

impl BrowserBridge for WarpBrowserBridge {
    fn navigate(&self, url: String) -> impl Future<Output = anyhow::Result<String>> + Send {
        let spawner = self.spawner.clone();
        async move {
            spawner
                .spawn(move |_, ctx| {
                    if let Some(browser_view) = resolve_browser_view(ctx) {
                        browser_view.update(ctx, |view, ctx| view.navigate_to(url.clone(), ctx));
                        Ok(browser_view.as_ref(ctx).webview_host().current_url())
                    } else {
                        open_browser_pane_for(ctx, url.clone());
                        Ok(format!(
                            "No browser pane was open; opening one now for {url}. Call \
                             browser_navigate or browser_current_url again in a moment to confirm."
                        ))
                    }
                })
                .await
                .map_err(|_| anyhow::anyhow!("browser-control MCP bridge is unavailable"))?
        }
    }

    fn current_url(&self) -> impl Future<Output = anyhow::Result<String>> + Send {
        let spawner = self.spawner.clone();
        async move {
            spawner
                .spawn(move |_, ctx| {
                    resolve_browser_view(ctx)
                        .map(|browser_view| browser_view.as_ref(ctx).webview_host().current_url())
                        .ok_or_else(|| anyhow::anyhow!(NO_PANE_ERROR))
                })
                .await
                .map_err(|_| anyhow::anyhow!("browser-control MCP bridge is unavailable"))?
        }
    }

    fn screenshot(&self) -> impl Future<Output = anyhow::Result<Vec<u8>>> + Send {
        let spawner = self.spawner.clone();
        async move {
            let rx = spawner
                .spawn(move |_, ctx| {
                    let (tx, rx) = futures::channel::oneshot::channel();
                    match resolve_browser_view(ctx) {
                        Some(browser_view) => {
                            let host = browser_view.as_ref(ctx).webview_host().clone();
                            host.capture_screenshot(move |result| {
                                let _ = tx.send(result);
                            });
                        }
                        None => {
                            let _ = tx.send(Err(anyhow::anyhow!(NO_PANE_ERROR)));
                        }
                    }
                    rx
                })
                .await
                .map_err(|_| anyhow::anyhow!("browser-control MCP bridge is unavailable"))?;
            rx.await
                .map_err(|_| anyhow::anyhow!("browser pane dropped before completing"))?
        }
    }

    fn click(&self, selector: String) -> impl Future<Output = anyhow::Result<()>> + Send {
        let spawner = self.spawner.clone();
        async move {
            let rx = spawner
                .spawn(move |_, ctx| {
                    let (tx, rx) = futures::channel::oneshot::channel();
                    let Some(browser_view) = resolve_browser_view(ctx) else {
                        let _ = tx.send(Err(anyhow::anyhow!(NO_PANE_ERROR)));
                        return rx;
                    };
                    let host = browser_view.as_ref(ctx).webview_host().clone();
                    let host_for_click = host.clone();
                    let selector_for_error = selector.clone();
                    host.element_center(&selector, move |result| match result {
                        Ok(Some((x, y))) => {
                            host_for_click.send_click(x, y, move |click_result| {
                                let _ = tx.send(click_result);
                            });
                        }
                        Ok(None) => {
                            let _ = tx.send(Err(anyhow::anyhow!(
                                "No element matched selector {selector_for_error:?}"
                            )));
                        }
                        Err(err) => {
                            let _ = tx.send(Err(err));
                        }
                    });
                    rx
                })
                .await
                .map_err(|_| anyhow::anyhow!("browser-control MCP bridge is unavailable"))?;
            rx.await
                .map_err(|_| anyhow::anyhow!("browser pane dropped before completing"))?
        }
    }

    fn type_text(
        &self,
        text: String,
        selector: Option<String>,
    ) -> impl Future<Output = anyhow::Result<()>> + Send {
        let spawner = self.spawner.clone();
        async move {
            let rx = spawner
                .spawn(move |_, ctx| {
                    let (tx, rx) = futures::channel::oneshot::channel();
                    let Some(browser_view) = resolve_browser_view(ctx) else {
                        let _ = tx.send(Err(anyhow::anyhow!(NO_PANE_ERROR)));
                        return rx;
                    };
                    let host = browser_view.as_ref(ctx).webview_host().clone();
                    match selector {
                        Some(selector) => {
                            let host_for_insert = host.clone();
                            // Best-effort focus before inserting: CDP `Input.insertText` types at
                            // whatever currently has focus, so an unfocused target selector would
                            // otherwise silently insert nowhere useful.
                            let focus_js = format!(
                                "(() => {{ const el = document.querySelector({}); if (el) el.focus(); }})()",
                                serde_json::to_string(&selector)
                                    .unwrap_or_else(|_| "\"\"".to_string())
                            );
                            host.evaluate_js(&focus_js, move |focus_result| {
                                if let Err(err) = focus_result {
                                    let _ = tx.send(Err(err));
                                    return;
                                }
                                host_for_insert.insert_text(&text, move |result| {
                                    let _ = tx.send(result);
                                });
                            });
                        }
                        None => {
                            host.insert_text(&text, move |result| {
                                let _ = tx.send(result);
                            });
                        }
                    }
                    rx
                })
                .await
                .map_err(|_| anyhow::anyhow!("browser-control MCP bridge is unavailable"))?;
            rx.await
                .map_err(|_| anyhow::anyhow!("browser pane dropped before completing"))?
        }
    }

    fn console(&self, level: Option<String>) -> impl Future<Output = anyhow::Result<String>> + Send {
        let spawner = self.spawner.clone();
        async move {
            spawner
                .spawn(move |_, ctx| {
                    let browser_view =
                        resolve_browser_view(ctx).ok_or_else(|| anyhow::anyhow!(NO_PANE_ERROR))?;
                    let host = browser_view.as_ref(ctx).webview_host();
                    let text = if level.as_deref() == Some("error") {
                        host.console_errors_text()
                    } else {
                        let entries = host.recent_console();
                        if entries.is_empty() {
                            "No console entries captured.".to_string()
                        } else {
                            entries
                                .iter()
                                .map(|entry| match &entry.stack {
                                    Some(stack) => {
                                        format!("[{}] {}\n{stack}", entry.level, entry.message)
                                    }
                                    None => format!("[{}] {}", entry.level, entry.message),
                                })
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        }
                    };
                    Ok(text)
                })
                .await
                .map_err(|_| anyhow::anyhow!("browser-control MCP bridge is unavailable"))?
        }
    }

    fn evaluate(&self, js: String) -> impl Future<Output = anyhow::Result<String>> + Send {
        let spawner = self.spawner.clone();
        async move {
            let rx = spawner
                .spawn(move |_, ctx| {
                    let (tx, rx) = futures::channel::oneshot::channel();
                    match resolve_browser_view(ctx) {
                        Some(browser_view) => {
                            let host = browser_view.as_ref(ctx).webview_host().clone();
                            host.evaluate_js(&js, move |result| {
                                let _ = tx.send(result);
                            });
                        }
                        None => {
                            let _ = tx.send(Err(anyhow::anyhow!(NO_PANE_ERROR)));
                        }
                    }
                    rx
                })
                .await
                .map_err(|_| anyhow::anyhow!("browser-control MCP bridge is unavailable"))?;
            rx.await
                .map_err(|_| anyhow::anyhow!("browser pane dropped before completing"))?
        }
    }
}

/// Finds the `BrowserPane` in the active window's active tab, if any, and returns a handle to
/// its backing `BrowserView`. Falls back to the first live workspace when no window is
/// currently focused (e.g. the MCP call races window-focus state) — same fallback shape as
/// `root_view::active_workspace`, just without requiring the caller be that module.
fn resolve_browser_view(ctx: &mut AppContext) -> Option<ViewHandle<BrowserView>> {
    let workspace_handle = active_workspace_handle(ctx)?;
    let pane_group_handle = workspace_handle.as_ref(ctx).active_tab_pane_group().clone();
    let pane_id = pane_group_handle
        .as_ref(ctx)
        .visible_pane_ids()
        .into_iter()
        .find(|&pane_id| {
            pane_group_handle
                .as_ref(ctx)
                .downcast_pane_by_id::<BrowserPane>(pane_id)
                .is_some()
        })?;
    let browser_view = pane_group_handle
        .as_ref(ctx)
        .downcast_pane_by_id::<BrowserPane>(pane_id)?
        .browser_view(ctx);
    Some(browser_view)
}

/// Opens a browser pane in the active (or first live) workspace, mirroring the open-or-focus
/// behavior `Workspace::open_browser_pane` already gives the toolbar globe button and
/// dev-server auto-detection.
fn open_browser_pane_for(ctx: &mut AppContext, url: String) {
    let Some(workspace_handle) = active_workspace_handle(ctx) else {
        log::warn!("browser-control MCP: no open Warp window to open a browser pane in");
        return;
    };
    workspace_handle.update(ctx, |workspace, ctx| {
        workspace.open_browser_pane(Some(url), ctx)
    });
}

fn active_workspace_handle(ctx: &mut AppContext) -> Option<ViewHandle<Workspace>> {
    if let Some(window_id) = ctx.windows().active_window() {
        if let Some(handle) = WorkspaceRegistry::as_ref(ctx).get(window_id, ctx) {
            return Some(handle);
        }
    }
    WorkspaceRegistry::as_ref(ctx)
        .all_workspaces(ctx)
        .into_iter()
        .next()
        .map(|(_, handle)| handle)
}
