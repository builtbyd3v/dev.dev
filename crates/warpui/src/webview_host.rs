use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

#[cfg(not(target_os = "windows"))]
use pathfinder_color::ColorU;
#[cfg(not(target_os = "windows"))]
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::Vector2F;
#[cfg(not(target_os = "windows"))]
use warpui_core::elements::Fill;
use warpui_core::elements::{Element, Point};
use warpui_core::event::DispatchedEvent;
use warpui_core::{
    AfterLayoutContext, AppContext, EventContext, LayoutContext, PaintContext, SizeConstraint,
    WindowId,
};

/// Console-capture + element-selector bootstrap, injected into every navigation via
/// `with_initialization_script` (see `ensure_webview`). Talks back to Rust through
/// `window.ipc.postMessage(JSON.stringify({type: "...", ...}))`; the envelope types
/// (`console`, `elementSelected`, `favicon`) are parsed by the ipc handler below into
/// `ConsoleEntry` / `ElementSelection` / the favicon cell.
const INIT_SCRIPT: &str = include_str!("webview_init.js");

/// Max number of `ConsoleEntry` records kept per webview (mirrors the JS-side ring buffer cap in
/// `webview_init.js`, which keeps its own copy for future client-side pulls).
const CONSOLE_BUFFER_CAP: usize = 200;

#[cfg(target_os = "windows")]
thread_local! {
    /// Weak handles to every live [`WebViewHost`] created on this (UI) thread, so
    /// [`sweep_unpainted_webviews`] can find webviews whose element didn't paint in the frame
    /// that was just built. Dead entries are pruned during each sweep.
    static WEBVIEW_HOSTS: RefCell<Vec<std::rc::Weak<WebViewHost>>> = RefCell::new(Vec::new());
}

/// Hides any webview belonging to `window_id` whose element wasn't painted in the scene that was
/// just built (e.g. its tab is backgrounded). The webview is a native child HWND layered over the
/// window's client area, so without this it would keep floating over whatever replaced its pane.
/// Call once per window, right after a full scene build (a scene build is the one complete paint
/// pass, so "not painted" reliably means "not on screen"). Re-showing happens in
/// [`WebViewHostElement::paint`] when the pane becomes visible again.
#[cfg(target_os = "windows")]
pub fn sweep_unpainted_webviews(window_id: WindowId) {
    WEBVIEW_HOSTS.with(|hosts| {
        hosts.borrow_mut().retain(|weak| {
            let Some(host) = weak.upgrade() else {
                return false;
            };
            if host.window_id.get() == window_id
                && !host.painted_this_frame.replace(false)
                && !host.hidden.get()
            {
                host.set_hidden(true);
            }
            true
        });
    });
}

/// One console.log/warn/error/info call or uncaught error/rejection, captured by the init script.
#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    /// `"log"`, `"warn"`, `"error"`, or `"info"`.
    pub level: String,
    pub message: String,
    pub stack: Option<String>,
}

/// An element picked via `start_element_selection`, reported by the init script's click handler.
#[derive(Debug, Clone)]
pub struct ElementSelection {
    /// `outerHTML`, truncated to 8KB by the JS side.
    pub html: String,
    /// Computed CSS selector path (e.g. `div#app > section:nth-of-type(2) > button`).
    pub selector: String,
    pub classes: Vec<String>,
    pub rect: ElementRect,
    /// `innerText`, truncated to 1KB by the JS side.
    pub text: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ElementRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The `{type: "..."}` envelope JS posts over IPC; see `webview_init.js`.
#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum IpcEnvelope {
    #[serde(rename = "console")]
    Console {
        level: String,
        message: String,
        stack: Option<String>,
    },
    #[serde(rename = "elementSelected")]
    ElementSelected {
        html: String,
        selector: String,
        classes: Vec<String>,
        rect: ElementRect,
        text: String,
    },
    #[serde(rename = "favicon")]
    Favicon { url: String },
}

/// Wraps a [`raw_window_handle::RawWindowHandle`] so it can be handed to wry's
/// `HasWindowHandle`-bound APIs. `raw_window_handle::WindowHandle::borrow_raw` is unsafe because
/// it can't verify the handle stays valid for the borrow's lifetime; we only ever call it just
/// before immediately using the handle to build/resize a child webview, while the parent
/// `platform::Window` is still alive, so the borrow is sound.
struct RawHandleWrapper(raw_window_handle::RawWindowHandle);

impl raw_window_handle::HasWindowHandle for RawHandleWrapper {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(self.0) })
    }
}

/// Owns the native WebView2 child window for a browser pane. Created once by the owning `View`
/// and held as an `Rc` across `render()` calls, since `View::render` is called on every rebuild
/// but the underlying webview must only be created once (it's a real native child HWND, not
/// something we want to tear down and recreate every frame).
///
/// The webview itself is created lazily, on the first `paint`, because that's the first point at
/// which we have access to the `AppContext` needed to resolve the window's raw handle.
pub struct WebViewHost {
    window_id: Cell<WindowId>,
    url: RefCell<String>,
    /// Tracks the page actually loaded in the webview, kept current by wry's page-load handler
    /// (see `with_on_page_load_handler` in `ensure_webview`) so it reflects in-page navigation
    /// (clicked links, redirects), not just the last `load_url` call. Also updated optimistically
    /// by `load_url` itself so callers see the target URL immediately, before the navigation
    /// completes. Separate `Rc<RefCell<_>>` (rather than reusing `url`) so the page-load handler
    /// closure can capture just this cell, not the whole host — avoids an `Rc` cycle since the
    /// closure is owned by the `wry::WebView`, which is itself owned by this host.
    current_url: Rc<RefCell<String>>,
    /// Self-contained HTML for the browser pane's start page (see `WebViewHost::new_start_page`),
    /// loaded via wry's `with_html` instead of `with_url` when set. Only consulted once, by
    /// `ensure_webview` at webview-creation time; cleared by `load_url` so a *rebuilt* webview
    /// (e.g. after `rebind_window`'s cross-window teardown/recreate) restores `url` rather than
    /// the start page. Real in-page navigation (clicking a start-page suggestion link) doesn't
    /// clear it — same "rebind loses in-page nav state, falls back to the last `load_url`/
    /// construction target" limitation already documented on `rebind_window`.
    start_html: RefCell<Option<String>>,
    /// True while a navigation is in flight, kept current by the page-load handler (see
    /// `with_on_page_load_handler` in `ensure_webview`): set on `PageLoadEvent::Started`, cleared
    /// on `PageLoadEvent::Finished`. `Rc<Cell<_>>` (not a plain `Cell` on the host) so the handler
    /// closure — owned by the `wry::WebView`, itself owned by this host — can hold a clone without
    /// an `Rc` cycle back to the host, same reasoning as `current_url`.
    loading: Rc<Cell<bool>>,
    /// Set via `set_on_loading_changed`; invoked by the page-load handler whenever `loading`
    /// changes, so `BrowserView` can `ctx.notify()` and re-render the toolbar's progress bar. Same
    /// `Rc`-not-host-capture shape as `on_element_selected`.
    on_loading_changed: Rc<RefCell<Option<Box<dyn Fn(bool)>>>>,
    hidden: Cell<bool>,
    /// Console entries reported by the init script's IPC messages, capped at
    /// `CONSOLE_BUFFER_CAP`. `Rc` so the ipc handler closure (built in `ensure_webview`, owned by
    /// the `wry::WebView`, itself owned by this host) can hold a clone without an `Rc` cycle back
    /// to the host.
    console_buffer: Rc<RefCell<VecDeque<ConsoleEntry>>>,
    /// Set via `set_on_element_selected`; invoked by the ipc handler when the init script reports
    /// a completed element pick. Same `Rc`-not-host-capture reasoning as `console_buffer`.
    on_element_selected: Rc<RefCell<Option<Box<dyn Fn(ElementSelection)>>>>,
    /// The current page's favicon URL, reported by the init script (see `webview_init.js`'s
    /// `reportFavicon`). `None` until the first report arrives for the loaded page. `Rc` for the
    /// same "ipc handler captures the cell, not the host" reasoning as `console_buffer`.
    favicon: Rc<RefCell<Option<String>>>,
    /// Set via `set_on_favicon_changed`; invoked by the ipc handler whenever `favicon` changes.
    /// Same one-callback shape as `on_loading_changed`.
    on_favicon_changed: Rc<RefCell<Option<Box<dyn Fn(Option<String>)>>>>,
    #[cfg(target_os = "windows")]
    webview: RefCell<Option<wry::WebView>>,
    /// Holds the wry [`WebContext`](wry::WebContext) that pins the WebView2 user-data directory to
    /// a writable per-user path (see `webview_data_directory`). WebView2 otherwise defaults its
    /// user-data folder to a directory *next to the executable*; for an installed build under
    /// `C:\Program Files` that path isn't writable, so webview creation fails with `0x80070005
    /// Access is denied` and the pane renders blank. wry requires the `WebContext` to outlive the
    /// `WebView`, so it's stored here rather than dropped after `ensure_webview`.
    #[cfg(target_os = "windows")]
    web_context: RefCell<Option<wry::WebContext>>,
    /// Set by [`WebViewHostElement::paint`], consumed (and reset) by
    /// [`sweep_unpainted_webviews`] after each full scene build for this host's window; a host
    /// whose flag is still `false` at sweep time wasn't painted this frame and gets hidden.
    #[cfg(target_os = "windows")]
    painted_this_frame: Cell<bool>,
    /// The parent (main winit window) HWND the webview was built as a child of, recorded by
    /// `ensure_webview` so `destroy` can hand keyboard focus back to it — destroying the child
    /// HWND while it holds focus would otherwise leave focus nowhere and keyboard input dead.
    #[cfg(target_os = "windows")]
    parent_hwnd: Cell<Option<isize>>,
}

/// A writable directory for WebView2's user-data folder, under `%LOCALAPPDATA%`. Created if
/// missing. Returns `None` only if `%LOCALAPPDATA%` is unset or the directory can't be created, in
/// which case wry falls back to its default (next-to-exe) path — fine for a dev build run from a
/// writable target dir, but the whole reason this exists is that the default is not writable for an
/// installed build under `C:\Program Files`.
#[cfg(target_os = "windows")]
fn webview_data_directory() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let dir = std::path::PathBuf::from(base).join("dev.dev").join("WebView2");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

impl WebViewHost {
    pub fn new(window_id: WindowId, url: impl Into<String>) -> Rc<Self> {
        let url = url.into();
        let host = Rc::new(Self {
            window_id: Cell::new(window_id),
            current_url: Rc::new(RefCell::new(url.clone())),
            url: RefCell::new(url),
            start_html: RefCell::new(None),
            loading: Rc::new(Cell::new(false)),
            on_loading_changed: Rc::new(RefCell::new(None)),
            hidden: Cell::new(false),
            console_buffer: Rc::new(RefCell::new(VecDeque::new())),
            on_element_selected: Rc::new(RefCell::new(None)),
            favicon: Rc::new(RefCell::new(None)),
            on_favicon_changed: Rc::new(RefCell::new(None)),
            #[cfg(target_os = "windows")]
            webview: RefCell::new(None),
            #[cfg(target_os = "windows")]
            web_context: RefCell::new(None),
            #[cfg(target_os = "windows")]
            painted_this_frame: Cell::new(false),
            #[cfg(target_os = "windows")]
            parent_hwnd: Cell::new(None),
        });
        #[cfg(target_os = "windows")]
        WEBVIEW_HOSTS.with(|hosts| hosts.borrow_mut().push(Rc::downgrade(&host)));
        host
    }

    /// Like `new`, but the webview loads `html` directly (via wry's `with_html`) instead of
    /// navigating to a URL — used for the browser pane's local start page (see
    /// `BrowserView::new`), which has no real URL to fetch. `url` is still tracked as an empty
    /// string and gets a real value once the user navigates (typed URL or `load_url`, which clears
    /// `start_html` — see its doc comment).
    pub fn new_start_page(window_id: WindowId, html: impl Into<String>) -> Rc<Self> {
        let host = Self::new(window_id, String::new());
        *host.start_html.borrow_mut() = Some(html.into());
        host
    }

    /// Navigates the webview to `url`. If the native webview hasn't been created yet, it will be
    /// created with this URL on the next paint.
    pub fn load_url(&self, url: &str) {
        *self.url.borrow_mut() = url.to_string();
        *self.current_url.borrow_mut() = url.to_string();
        // Explicit navigation permanently leaves the start page behind (see `start_html`'s doc
        // comment for the rebind-time caveat).
        self.start_html.borrow_mut().take();
        // Clear the stale favicon immediately (new page hasn't reported its own yet) rather than
        // waiting for the page-load handler, so the toolbar drops the old icon right away.
        if self.favicon.borrow_mut().take().is_some() {
            if let Some(callback) = self.on_favicon_changed.borrow().as_ref() {
                callback(None);
            }
        }
        #[cfg(target_os = "windows")]
        if let Some(webview) = self.webview.borrow().as_ref() {
            let _ = webview.load_url(url);
        }
    }

    /// The webview's actual current URL, kept live by the page-load handler (reflects in-page
    /// navigation like clicked links, not just the last `load_url` call).
    pub fn current_url(&self) -> String {
        self.current_url.borrow().clone()
    }

    /// Whether a navigation is currently in flight, kept live by the page-load handler. Drives
    /// the toolbar's loading bar (see `BrowserView::render`).
    pub fn is_loading(&self) -> bool {
        self.loading.get()
    }

    /// The current page's favicon URL, if the init script has reported one for the loaded page
    /// yet (see `webview_init.js`'s `reportFavicon`). Drives the toolbar's URL-field adornment
    /// (see `BrowserView::render_toolbar`).
    pub fn current_favicon(&self) -> Option<String> {
        self.favicon.borrow().clone()
    }

    /// Navigates back in the webview's history. wry has no cross-platform back/forward API, so
    /// this drops to the WebView2 COM layer directly (`ICoreWebView2::GoBack`), same escape hatch
    /// `capture_screenshot` uses for CDP.
    #[cfg(target_os = "windows")]
    pub fn go_back(&self) {
        use wry::WebViewExtWindows;
        if let Some(webview) = self.webview.borrow().as_ref() {
            let _ = unsafe { webview.webview().GoBack() };
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn go_back(&self) {}

    /// Navigates forward in the webview's history. See `go_back` for why this is a COM call.
    #[cfg(target_os = "windows")]
    pub fn go_forward(&self) {
        use wry::WebViewExtWindows;
        if let Some(webview) = self.webview.borrow().as_ref() {
            let _ = unsafe { webview.webview().GoForward() };
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn go_forward(&self) {}

    /// Reloads the current page. Unlike back/forward, wry exposes this natively on `WebView`
    /// (backed by `ICoreWebView2::Reload` under the hood on Windows), so no COM call needed here.
    #[cfg(target_os = "windows")]
    pub fn reload(&self) {
        if let Some(webview) = self.webview.borrow().as_ref() {
            let _ = webview.reload();
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn reload(&self) {}

    /// Opens the WebView2 DevTools window for this webview (`ICoreWebView2::OpenDevToolsWindow`).
    /// No wry-native equivalent; same COM escape-hatch pattern as `go_back`/`go_forward`.
    #[cfg(target_os = "windows")]
    pub fn open_devtools(&self) {
        use wry::WebViewExtWindows;
        if let Some(webview) = self.webview.borrow().as_ref() {
            let _ = unsafe { webview.webview().OpenDevToolsWindow() };
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn open_devtools(&self) {}

    /// Captures a screenshot of the current page via the WebView2 CDP endpoint
    /// (`Page.captureScreenshot`) and hands decoded PNG bytes to `callback`.
    ///
    /// The CDP completion handler is a COM callback that WebView2 always invokes on the same
    /// COM-STA/UI thread that issued the call (i.e. the thread pumping this window's message
    /// loop), so `callback` runs synchronously on that thread too — no cross-thread handoff, no
    /// `Send` bound needed.
    #[cfg(target_os = "windows")]
    pub fn capture_screenshot(&self, callback: impl FnOnce(anyhow::Result<Vec<u8>>) + 'static) {
        use wry::WebViewExtWindows;

        let cw2 = {
            let webview_ref = self.webview.borrow();
            let Some(webview) = webview_ref.as_ref() else {
                drop(webview_ref);
                callback(Err(anyhow::anyhow!(
                    "capture_screenshot called before the webview was created"
                )));
                return;
            };
            webview.webview()
        };

        // The completed-handler closure only fires once; wrap `callback` so the synchronous
        // error path below and the async completion path can't both try to call it.
        let callback = Rc::new(RefCell::new(Some(callback)));
        let callback_for_handler = callback.clone();
        let handler = webview2_com::CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
            move |hr, json| {
                if let Some(callback) = callback_for_handler.borrow_mut().take() {
                    let result = hr
                        .map_err(|err| anyhow::anyhow!("Page.captureScreenshot failed: {err}"))
                        .and_then(|()| decode_capture_screenshot_result(&json));
                    callback(result);
                }
                Ok(())
            },
        ));

        let (_method_buf, method) = win32_pcwstr("Page.captureScreenshot");
        let (_params_buf, params) = win32_pcwstr("{}");
        // Safety: `method`/`params` point into `_method_buf`/`_params_buf`, which outlive this
        // call (CallDevToolsProtocolMethod reads them synchronously before returning; the async
        // part is only the *result*, delivered later via `handler`).
        let call_result =
            unsafe { cw2.CallDevToolsProtocolMethod(method, params, &handler) };
        if let Err(err) = call_result {
            if let Some(callback) = callback.borrow_mut().take() {
                callback(Err(anyhow::anyhow!(
                    "Failed to invoke Page.captureScreenshot: {err}"
                )));
            }
        }
    }

    /// No webview support on this platform yet; always reports an error.
    #[cfg(not(target_os = "windows"))]
    pub fn capture_screenshot(&self, callback: impl FnOnce(anyhow::Result<Vec<u8>>) + 'static) {
        callback(Err(anyhow::anyhow!(
            "Screenshot capture is only implemented on Windows"
        )));
    }

    /// Dispatches a synthetic left click at CSS-pixel coordinates `(x, y)` via the CDP
    /// `Input.dispatchMouseEvent` method: a `mousePressed` event immediately followed by a
    /// `mouseReleased` event at the same point, same button, `clickCount: 1`. Used by the
    /// browser-automation MCP tools to click at a resolved element center (see `element_center`).
    ///
    /// Same COM-STA/UI-thread reentrancy story as `capture_screenshot`: both CDP calls and their
    /// completion handlers run on the thread that called this method, chained (release fires from
    /// inside press's completion handler), so `callback` fires exactly once, after the release
    /// completes (or as soon as either call fails).
    #[cfg(target_os = "windows")]
    pub fn send_click(&self, x: f64, y: f64, callback: impl FnOnce(anyhow::Result<()>) + 'static) {
        use wry::WebViewExtWindows;

        let cw2 = {
            let webview_ref = self.webview.borrow();
            let Some(webview) = webview_ref.as_ref() else {
                drop(webview_ref);
                callback(Err(anyhow::anyhow!(
                    "send_click called before the webview was created"
                )));
                return;
            };
            webview.webview()
        };

        // Outer guard: fires `callback` if the *press* call fails synchronously (before any
        // completion handler runs). Cloned again below for the nested release call.
        let callback = Rc::new(RefCell::new(Some(callback)));
        let callback_for_press_handler = callback.clone();
        let cw2_for_release = cw2.clone();

        let press_handler = webview2_com::CallDevToolsProtocolMethodCompletedHandler::create(
            Box::new(move |hr, _json| {
                if let Err(err) = hr {
                    if let Some(cb) = callback_for_press_handler.borrow_mut().take() {
                        cb(Err(anyhow::anyhow!(
                            "Input.dispatchMouseEvent (press) failed: {err}"
                        )));
                    }
                    return Ok(());
                }

                let release_params = serde_json::json!({
                    "type": "mouseReleased",
                    "x": x,
                    "y": y,
                    "button": "left",
                    "clickCount": 1,
                })
                .to_string();

                // Same guard-clone split as above, one level deeper: `callback_release_outer` for
                // the release call's own synchronous-failure path, `callback_release` moved into
                // its completion handler.
                let callback_release_outer = callback_for_press_handler.clone();
                let callback_release = callback_for_press_handler.clone();
                let release_handler =
                    webview2_com::CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
                        move |hr, _json| {
                            if let Some(cb) = callback_release.borrow_mut().take() {
                                let result = hr.map_err(|err| {
                                    anyhow::anyhow!(
                                        "Input.dispatchMouseEvent (release) failed: {err}"
                                    )
                                });
                                cb(result);
                            }
                            Ok(())
                        },
                    ));

                let (_method_buf, method) = win32_pcwstr("Input.dispatchMouseEvent");
                let (_params_buf, params) = win32_pcwstr(&release_params);
                // Safety: same as `capture_screenshot` — `method`/`params` outlive this
                // synchronous call into `CallDevToolsProtocolMethod`.
                let call_result = unsafe {
                    cw2_for_release.CallDevToolsProtocolMethod(method, params, &release_handler)
                };
                if let Err(err) = call_result {
                    if let Some(cb) = callback_release_outer.borrow_mut().take() {
                        cb(Err(anyhow::anyhow!(
                            "Failed to invoke Input.dispatchMouseEvent (release): {err}"
                        )));
                    }
                }
                Ok(())
            }),
        );

        let press_params = serde_json::json!({
            "type": "mousePressed",
            "x": x,
            "y": y,
            "button": "left",
            "clickCount": 1,
        })
        .to_string();
        let (_method_buf, method) = win32_pcwstr("Input.dispatchMouseEvent");
        let (_params_buf, params) = win32_pcwstr(&press_params);
        let call_result = unsafe { cw2.CallDevToolsProtocolMethod(method, params, &press_handler) };
        if let Err(err) = call_result {
            if let Some(cb) = callback.borrow_mut().take() {
                cb(Err(anyhow::anyhow!(
                    "Failed to invoke Input.dispatchMouseEvent (press): {err}"
                )));
            }
        }
    }

    /// No webview support on this platform yet; always reports an error.
    #[cfg(not(target_os = "windows"))]
    pub fn send_click(&self, _x: f64, _y: f64, callback: impl FnOnce(anyhow::Result<()>) + 'static) {
        callback(Err(anyhow::anyhow!("send_click is only implemented on Windows")));
    }

    /// Inserts `text` at the current focus/caret via CDP `Input.insertText` (assumes the page has
    /// already focused an editable element, e.g. after a prior `send_click`). `text` is embedded
    /// as a `serde_json`-escaped JSON string, so it's safe for any input including quotes/newlines.
    #[cfg(target_os = "windows")]
    pub fn insert_text(&self, text: &str, callback: impl FnOnce(anyhow::Result<()>) + 'static) {
        use wry::WebViewExtWindows;

        let cw2 = {
            let webview_ref = self.webview.borrow();
            let Some(webview) = webview_ref.as_ref() else {
                drop(webview_ref);
                callback(Err(anyhow::anyhow!(
                    "insert_text called before the webview was created"
                )));
                return;
            };
            webview.webview()
        };

        let callback = Rc::new(RefCell::new(Some(callback)));
        let callback_for_handler = callback.clone();
        let handler = webview2_com::CallDevToolsProtocolMethodCompletedHandler::create(Box::new(
            move |hr, _json| {
                if let Some(cb) = callback_for_handler.borrow_mut().take() {
                    cb(hr.map_err(|err| anyhow::anyhow!("Input.insertText failed: {err}")));
                }
                Ok(())
            },
        ));

        let params = serde_json::json!({ "text": text }).to_string();
        let (_method_buf, method) = win32_pcwstr("Input.insertText");
        let (_params_buf, params) = win32_pcwstr(&params);
        let call_result = unsafe { cw2.CallDevToolsProtocolMethod(method, params, &handler) };
        if let Err(err) = call_result {
            if let Some(cb) = callback.borrow_mut().take() {
                cb(Err(anyhow::anyhow!("Failed to invoke Input.insertText: {err}")));
            }
        }
    }

    /// No webview support on this platform yet; always reports an error.
    #[cfg(not(target_os = "windows"))]
    pub fn insert_text(&self, _text: &str, callback: impl FnOnce(anyhow::Result<()>) + 'static) {
        callback(Err(anyhow::anyhow!("insert_text is only implemented on Windows")));
    }

    /// Runs `js` in the page and hands the JSON-serialized result to `callback`. Public wrapper
    /// over wry's native `WebView::evaluate_script_with_callback` (not CDP, unlike the other
    /// automation primitives here) — used both directly by the MCP `browser_evaluate` tool and by
    /// `element_center` below.
    ///
    /// wry's callback bound requires `Fn(String) + Send`, but WebView2 only ever invokes it
    /// synchronously on this same COM-STA/UI thread (never across threads), so wrapping the
    /// non-`Send` `Rc` guard in `AssertSendOnUiThread` is sound — see its doc comment.
    #[cfg(target_os = "windows")]
    pub fn evaluate_js(&self, js: &str, callback: impl FnOnce(anyhow::Result<String>) + 'static) {
        let webview_ref = self.webview.borrow();
        let Some(webview) = webview_ref.as_ref() else {
            drop(webview_ref);
            callback(Err(anyhow::anyhow!(
                "evaluate_js called before the webview was created"
            )));
            return;
        };

        let callback = Rc::new(RefCell::new(Some(callback)));
        let callback_for_sync_error = callback.clone();
        let wrapped = AssertSendOnUiThread(callback);
        let call_result = webview.evaluate_script_with_callback(js, move |result| {
            // Force capture of the whole `wrapped` (not just its `.0` field) — Rust 2021's
            // disjoint closure captures would otherwise capture the inner `Rc` directly, which
            // isn't `Send`, defeating the wrapper.
            let wrapped = &wrapped;
            if let Some(cb) = wrapped.0.borrow_mut().take() {
                cb(Ok(result));
            }
        });
        if let Err(err) = call_result {
            if let Some(cb) = callback_for_sync_error.borrow_mut().take() {
                cb(Err(anyhow::anyhow!("Failed to evaluate script: {err}")));
            }
        }
    }

    /// No webview support on this platform yet; always reports an error.
    #[cfg(not(target_os = "windows"))]
    pub fn evaluate_js(&self, _js: &str, callback: impl FnOnce(anyhow::Result<String>) + 'static) {
        callback(Err(anyhow::anyhow!("evaluate_js is only implemented on Windows")));
    }

    /// Resolves `selector` (a CSS selector) to its element's viewport-center point in CSS pixels,
    /// scrolling it into view first — the coordinate space `send_click` expects. Returns `Ok(None)`
    /// if no element matches. Built on `evaluate_js`, not CDP directly; pairs with `send_click` to
    /// let MCP's `browser_click { selector }` tool resolve-then-click.
    pub fn element_center(
        &self,
        selector: &str,
        callback: impl FnOnce(anyhow::Result<Option<(f64, f64)>>) + 'static,
    ) {
        // `selector` is JSON-encoded (not string-interpolated raw) so quotes/backslashes in it
        // can't break out of the JS string literal.
        let selector_json = serde_json::to_string(selector)
            .unwrap_or_else(|_| "\"\"".to_string());
        let js = format!(
            "(() => {{ \
                const el = document.querySelector({selector_json}); \
                if (!el) return null; \
                el.scrollIntoView({{block: 'center', inline: 'center'}}); \
                const r = el.getBoundingClientRect(); \
                return JSON.stringify([r.left + r.width / 2, r.top + r.height / 2]); \
            }})()"
        );

        self.evaluate_js(&js, move |result| {
            let result = result.and_then(|json| parse_element_center_result(&json));
            callback(result);
        });
    }

    /// Activates the element selector overlay (see `webview_init.js`): hovering highlights
    /// elements, clicking reports one via `set_on_element_selected`, Escape cancels. Just an
    /// `evaluate_script` call into the init script's global `__warpSelector.start()` — no new COM
    /// plumbing needed, unlike `capture_screenshot`.
    #[cfg(target_os = "windows")]
    pub fn start_element_selection(&self) {
        if let Some(webview) = self.webview.borrow().as_ref() {
            let _ = webview.evaluate_script("window.__warpSelector && window.__warpSelector.start();");
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn start_element_selection(&self) {}

    /// Clones out the current console ring buffer (most recent `CONSOLE_BUFFER_CAP` entries,
    /// oldest first).
    pub fn recent_console(&self) -> Vec<ConsoleEntry> {
        self.console_buffer.borrow().iter().cloned().collect()
    }

    /// Formats recent `"error"`-level console entries for AI context (e.g. the "Attach console"
    /// toolbar button). Not just `recent_console` filtered by the caller because this is the one
    /// property callers actually want (errors, not routine logs) and formatting it once here
    /// keeps that judgment call in one place.
    pub fn console_errors_text(&self) -> String {
        let buffer = self.console_buffer.borrow();
        let lines: Vec<String> = buffer
            .iter()
            .filter(|entry| entry.level == "error")
            .map(|entry| match &entry.stack {
                Some(stack) => format!("[error] {}\n{stack}", entry.message),
                None => format!("[error] {}", entry.message),
            })
            .collect();
        if lines.is_empty() {
            "No console errors captured.".to_string()
        } else {
            lines.join("\n\n")
        }
    }

    /// Registers the callback invoked when the init script reports a completed element pick (see
    /// `start_element_selection`). Only one callback is kept; a later call replaces the earlier
    /// one (matches `BrowserView` calling this once at construction).
    pub fn set_on_element_selected(&self, callback: impl Fn(ElementSelection) + 'static) {
        *self.on_element_selected.borrow_mut() = Some(Box::new(callback));
    }

    /// Registers the callback invoked whenever `is_loading` changes (see `loading`'s doc comment
    /// for why this can't just be observed directly from `BrowserView::render` — the page-load
    /// handler lives off the view tree). Same one-callback-only shape as `set_on_element_selected`.
    pub fn set_on_loading_changed(&self, callback: impl Fn(bool) + 'static) {
        *self.on_loading_changed.borrow_mut() = Some(Box::new(callback));
    }

    /// Registers the callback invoked whenever `current_favicon` changes (reported by the init
    /// script, see `webview_init.js`'s `reportFavicon`). Same one-callback-only shape as
    /// `set_on_loading_changed`.
    pub fn set_on_favicon_changed(&self, callback: impl Fn(Option<String>) + 'static) {
        *self.on_favicon_changed.borrow_mut() = Some(Box::new(callback));
    }

    /// Hides (or re-shows) the native webview without destroying it, e.g. while its pane is
    /// backgrounded.
    pub fn set_hidden(&self, hidden: bool) {
        self.hidden.set(hidden);
        #[cfg(target_os = "windows")]
        if let Some(webview) = self.webview.borrow().as_ref() {
            let _ = webview.set_visible(!hidden);
        }
    }

    /// Tears down the native webview, e.g. when the owning pane is detached/closed. Safe to call
    /// even if the webview was never created.
    pub fn destroy(&self) {
        #[cfg(target_os = "windows")]
        {
            let had_webview = self.webview.borrow_mut().take().is_some();
            // Dropping the webview destroys a child HWND that may currently hold keyboard
            // focus, which would leave focus nowhere and keyboard input dead until the user
            // alt-tabs. Hand focus back to the main window. `GetFocus` returns null when focus
            // sits in another process's window (the WebView2 child HWNDs belong to
            // msedgewebview2.exe) or nowhere at all, so `!= parent` covers both cases; skip the
            // call when the main window already has focus.
            if had_webview {
                if let Some(hwnd) = self.parent_hwnd.get() {
                    use windows::Win32::Foundation::HWND;
                    use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
                    let parent = HWND(hwnd as _);
                    if unsafe { GetFocus() } != parent {
                        let _ = unsafe { SetFocus(Some(parent)) };
                    }
                }
            }
        }
    }

    /// Re-parents this host to a different window, e.g. when the owning pane is dragged into a
    /// new OS window (see `View::on_window_transferred`). wry/WebView2 has no supported
    /// cross-HWND reparenting API, so this destroys the existing child HWND (same teardown as
    /// `destroy`) and lets the next `paint` lazily recreate it under `new_window_id`'s HWND via
    /// `ensure_webview`, at whatever URL is current (`self.url`, unaffected by this call) —
    /// history is lost, same limitation noted on `LeafContents::Browser`.
    #[cfg(target_os = "windows")]
    pub fn rebind_window(&self, new_window_id: WindowId) {
        if self.window_id.get() == new_window_id {
            return;
        }
        self.destroy();
        self.window_id.set(new_window_id);
    }

    #[cfg(not(target_os = "windows"))]
    pub fn rebind_window(&self, new_window_id: WindowId) {
        self.window_id.set(new_window_id);
    }

    /// Builds the `Element` that lays out and syncs this webview's bounds. Call this from the
    /// owning `View::render` on every rebuild; it's cheap since it doesn't touch the native
    /// webview itself.
    pub fn element(self: &Rc<Self>) -> WebViewHostElement {
        WebViewHostElement {
            host: self.clone(),
            size: None,
            origin: None,
        }
    }

    #[cfg(target_os = "windows")]
    fn ensure_webview(&self, app: &AppContext) {
        if self.webview.borrow().is_some() {
            return;
        }
        let Some(window) = app.windows().platform_window(self.window_id.get()) else {
            return;
        };
        let Some(raw_handle) = window.raw_window_handle() else {
            return;
        };
        if let raw_window_handle::RawWindowHandle::Win32(win32) = raw_handle {
            self.parent_hwnd.set(Some(win32.hwnd.get()));
        }
        let handle = RawHandleWrapper(raw_handle);
        // Non-Send closure (wry requires only `'static`, not `Send`, for this handler) built on
        // the same thread as the webview; captures a clone of just the `current_url` cell, not
        // `self`/the host `Rc`, so the `WebView` (owned by the host) doesn't end up owning a
        // strong ref back to its own host.
        let current_url = self.current_url.clone();
        // Same "capture the cells, not `self`" reasoning as `current_url`.
        let loading = self.loading.clone();
        let on_loading_changed = self.on_loading_changed.clone();
        // Same "capture the cells, not `self`" reasoning as `current_url` above: the ipc handler
        // is owned by the `wry::WebView`, which this host owns, so capturing `self`/an `Rc<Self>`
        // here would be a reference cycle. `Rc` clones of just the two cells the handler needs.
        let console_buffer = self.console_buffer.clone();
        let on_element_selected = self.on_element_selected.clone();
        let favicon = self.favicon.clone();
        let on_favicon_changed = self.on_favicon_changed.clone();
        // Pin WebView2's user-data folder to a writable per-user directory. Without this it
        // defaults to a folder next to the .exe, which fails with "Access is denied" for an
        // installed build under C:\Program Files (see the `web_context` field doc).
        let mut web_context = wry::WebContext::new(webview_data_directory());
        // Start page (see `new_start_page`) loads inline HTML instead of navigating to a URL.
        let builder = match self.start_html.borrow().as_ref() {
            Some(html) => {
                wry::WebViewBuilder::new_with_web_context(&mut web_context).with_html(html.clone())
            }
            None => wry::WebViewBuilder::new_with_web_context(&mut web_context)
                .with_url(self.url.borrow().as_str()),
        };
        let webview = builder
            // Don't yank keyboard focus from the terminal when the webview is created.
            .with_focused(false)
            .with_on_page_load_handler(move |event, url| {
                *current_url.borrow_mut() = url;
                let now_loading = matches!(event, wry::PageLoadEvent::Started);
                if loading.replace(now_loading) != now_loading {
                    if let Some(callback) = on_loading_changed.borrow().as_ref() {
                        callback(now_loading);
                    }
                }
            })
            .with_initialization_script(INIT_SCRIPT)
            .with_ipc_handler(move |request| {
                let Ok(envelope) = serde_json::from_str::<IpcEnvelope>(request.body()) else {
                    return;
                };
                match envelope {
                    IpcEnvelope::Console { level, message, stack } => {
                        let mut buffer = console_buffer.borrow_mut();
                        buffer.push_back(ConsoleEntry { level, message, stack });
                        while buffer.len() > CONSOLE_BUFFER_CAP {
                            buffer.pop_front();
                        }
                    }
                    IpcEnvelope::ElementSelected { html, selector, classes, rect, text } => {
                        if let Some(callback) = on_element_selected.borrow().as_ref() {
                            callback(ElementSelection { html, selector, classes, rect, text });
                        }
                    }
                    IpcEnvelope::Favicon { url } => {
                        if favicon.borrow().as_deref() != Some(url.as_str()) {
                            *favicon.borrow_mut() = Some(url.clone());
                            if let Some(callback) = on_favicon_changed.borrow().as_ref() {
                                callback(Some(url));
                            }
                        }
                    }
                }
            })
            .with_bounds(wry::Rect {
                position: wry::dpi::PhysicalPosition::new(0, 0).into(),
                size: wry::dpi::PhysicalSize::new(1, 1).into(),
            })
            .build_as_child(&handle);
        match webview {
            Ok(webview) => {
                let _ = webview.set_visible(!self.hidden.get());
                *self.webview.borrow_mut() = Some(webview);
                // wry requires the WebContext to outlive the WebView; keep it alive here.
                *self.web_context.borrow_mut() = Some(web_context);
            }
            Err(err) => {
                log::error!("Failed to create WebView2 child webview: {err}");
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn sync_bounds(&self, origin: Vector2F, size: Vector2F, scale_factor: f32) {
        let webview_ref = self.webview.borrow();
        let Some(webview) = webview_ref.as_ref() else {
            return;
        };
        let physical_origin = origin * scale_factor;
        let physical_size = size * scale_factor;
        let _ = webview.set_bounds(wry::Rect {
            position: wry::dpi::PhysicalPosition::new(
                physical_origin.x() as i32,
                physical_origin.y() as i32,
            )
            .into(),
            size: wry::dpi::PhysicalSize::new(
                physical_size.x().max(0.) as u32,
                physical_size.y().max(0.) as u32,
            )
            .into(),
        });
    }
}

/// Wraps a value that isn't `Send` (an `Rc`) so it can be moved into a closure whose trait bound
/// (e.g. wry's `evaluate_script_with_callback`, which requires `Fn(String) + Send`) demands `Send`
/// even though the closure only ever actually runs on the thread that created it. Sound here
/// because WebView2 always invokes these completion callbacks synchronously on the calling
/// COM-STA/UI thread, never on another thread — same guarantee `capture_screenshot`'s doc comment
/// relies on for its CDP handler.
#[cfg(target_os = "windows")]
struct AssertSendOnUiThread<T>(T);

#[cfg(target_os = "windows")]
unsafe impl<T> Send for AssertSendOnUiThread<T> {}

/// Parses the JSON string `evaluate_js` hands back from the `element_center` JS snippet: either
/// the JSON literal `null` (no element matched) or a JSON string containing a `[x, y]` array
/// (wry double-encodes JS string return values into JSON). Not windows-gated (pure JSON parsing,
/// no platform API) since `element_center` itself isn't gated — it just delegates to
/// `evaluate_js`, whose non-Windows stub short-circuits before this ever runs.
fn parse_element_center_result(json: &str) -> anyhow::Result<Option<(f64, f64)>> {
    let outer: Option<String> = serde_json::from_str(json).map_err(|err| {
        anyhow::anyhow!("Failed to parse element_center result JSON: {err}")
    })?;
    let Some(inner) = outer else {
        return Ok(None);
    };
    let [x, y]: [f64; 2] = serde_json::from_str(&inner).map_err(|err| {
        anyhow::anyhow!("Failed to parse element_center coordinates JSON: {err}")
    })?;
    Ok(Some((x, y)))
}

/// Encodes `s` as a null-terminated UTF-16 buffer and a `PCWSTR` pointing into it. The returned
/// `Vec<u16>` must outlive any use of the `PCWSTR` (it's just a borrowed raw pointer).
#[cfg(target_os = "windows")]
fn win32_pcwstr(s: &str) -> (Vec<u16>, webview2_com_windows_core::PCWSTR) {
    let buf: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = webview2_com_windows_core::PCWSTR::from_raw(buf.as_ptr());
    (buf, pcwstr)
}

/// Parses the `{"data": "<base64 PNG>"}` JSON body returned by CDP's `Page.captureScreenshot`
/// and decodes the base64 payload.
#[cfg(target_os = "windows")]
fn decode_capture_screenshot_result(json: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;

    #[derive(serde::Deserialize)]
    struct CaptureScreenshotResult {
        data: String,
    }

    let parsed: CaptureScreenshotResult = serde_json::from_str(json).map_err(|err| {
        anyhow::anyhow!("Failed to parse Page.captureScreenshot result JSON: {err}")
    })?;
    base64::engine::general_purpose::STANDARD
        .decode(parsed.data)
        .map_err(|err| anyhow::anyhow!("Failed to decode screenshot base64: {err}"))
}

/// The `Element` half of [`WebViewHost`]: rebuilt every `render()`, but only ever mutates the
/// shared `WebViewHost` (lazily creating the webview, syncing its bounds on paint). On
/// non-Windows platforms (not yet supported by this host) it paints a placeholder rect instead.
pub struct WebViewHostElement {
    host: Rc<WebViewHost>,
    size: Option<Vector2F>,
    origin: Option<Point>,
}

impl Element for WebViewHostElement {
    fn layout(
        &mut self,
        constraint: SizeConstraint,
        _ctx: &mut LayoutContext,
        _app: &AppContext,
    ) -> Vector2F {
        let size = constraint.max;
        self.size = Some(size);
        size
    }

    fn after_layout(&mut self, _ctx: &mut AfterLayoutContext, _app: &AppContext) {}

    fn paint(&mut self, origin: Vector2F, ctx: &mut PaintContext, app: &AppContext) {
        self.origin = Some(Point::from_vec2f(origin, ctx.scene.z_index()));
        let size = self.size.unwrap_or_default();

        #[cfg(target_os = "windows")]
        {
            // Mark this host as painted so the post-frame sweep (`sweep_unpainted_webviews`)
            // leaves it visible, and re-show it if the sweep hid it while its pane was
            // backgrounded (paint running again means the pane is back on screen).
            self.host.painted_this_frame.set(true);
            if self.host.hidden.get() {
                self.host.set_hidden(false);
            }
            self.host.ensure_webview(app);
            let scale_factor = app
                .windows()
                .platform_window(self.host.window_id.get())
                .map_or(1.0, |window| window.as_ctx().backing_scale_factor());
            self.host.sync_bounds(origin, size, scale_factor);
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = app;
            ctx.scene
                .draw_rect_with_hit_recording(RectF::new(origin, size))
                .with_background(Fill::Solid(ColorU::new(30, 30, 30, 255)));
        }
    }

    fn dispatch_event(
        &mut self,
        _event: &DispatchedEvent,
        _ctx: &mut EventContext,
        _app: &AppContext,
    ) -> bool {
        false
    }

    fn size(&self) -> Option<Vector2F> {
        self.size
    }

    fn origin(&self) -> Option<Point> {
        self.origin
    }
}
