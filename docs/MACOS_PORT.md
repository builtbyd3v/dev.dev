# macOS Port Plan — dev.dev browser features

The browser features are currently Windows-only (`#[cfg(target_os = "windows")]` on WebView2 + Chrome DevTools Protocol). The base Warp terminal already builds and runs on macOS. This doc is the implementation-ready plan to bring the browser to macOS. **Build and verify on a Mac** — it cannot be cross-compiled or tested from Windows.

## Why it's not automatic
WebView2 (Windows) → **WKWebView** (macOS) via the same `wry` crate. wry supports both. But our agent-control + screenshot were built on **Chrome DevTools Protocol (CDP)**, which WKWebView does **not** have. Those degrade to native/JS equivalents.

## Prereqs (Mac)
- Xcode Command Line Tools (`xcode-select --install`)
- rustup (toolchain pinned by `rust-toolchain.toml`), `cmake`, `protobuf` (`brew install cmake protobuf`)
- `./script/bootstrap` then `./script/macos/run` (or `cargo build --bin warp-oss --features gui`). No Spectre libs / no PROTOC-PATH hacks needed (those were Windows-specific).

## Port matrix — per feature

### Works as-is on macOS (cross-platform already)
- **MCP server** (`crates/browser_mcp`, `app/src/browser_mcp/`) — axum/tokio/rmcp, no OS-specific code. Only the WebViewHost calls it makes degrade (below).
- **`evaluate_js`** (`webview_host.rs`) — uses wry `evaluate_script_with_callback` → WKWebView `evaluateJavaScript`. Cross-platform.
- **`element_center`** — pure JS via evaluate_js. Works.
- **Dev-server auto-open grid-scan** (`terminal_model.rs check_for_dev_server_url`) — scans terminal grid text, no OS code. Works.
- **Element selector overlay + console capture** (`webview_init.js`) — injected JS, cross-platform.
- **Start page** (`with_html`), toolbar, drag/undock, tab-switch visibility (`set_visible` is cross-platform in wry).

### Needs a macOS impl (currently Windows-gated)
1. **Webview creation / child embedding** — `webview_host.rs::ensure_webview` + `raw_window_handle()` seam.
   - Windows: child HWND via `build_as_child`. macOS: wry attaches a `WKWebView` as an `NSView` subview. `raw-window-handle` gives `RawWindowHandle::AppKit(NSView ptr)`. The `platform::Window::raw_window_handle()` impl added in `crates/warpui/src/windowing/winit/window.rs` should already return the AppKit handle on mac (winit provides it) — verify. wry `build_as_child` + `set_bounds` (uses `wry::dpi` logical/physical) is cross-platform. **Likely mostly works; remove the `cfg(windows)` gate on the webview field/creation and test.**
2. **Screenshot** — `capture_screenshot`. Windows: CDP `Page.captureScreenshot`. macOS: `WKWebView.takeSnapshot(with:completionHandler:)` → `NSImage`/`CGImage` → PNG bytes. Use `objc2`/`objc2-web-kit` (wry pulls objc2 in) or reach the WKWebView via `wry::WebViewExtMacOS::webview()` (check wry 0.55 for the mac ext trait) and call takeSnapshot. Callback delivers PNG.
3. **Click / type** — `send_click`, `insert_text`. Windows: CDP `Input.dispatchMouseEvent`/`insertText`. macOS: **no CDP** → synthesize via JS through `evaluate_js`: for click, `element_center` then `el.dispatchMouseEvent`/`el.click()` at coords; for type, `el.focus(); el.value = ...; el.dispatchEvent(new Event('input',{bubbles:true}))`. Weaker than real input events (some React controlled inputs need the native setter trick — document it) but works for most flows. Consider a shared JS `synthClick(x,y)`/`synthType(sel,text)` in `webview_init.js` that both platforms *could* use; keep CDP on Windows for fidelity.
4. **DevTools button** — `open_devtools`. Windows: `OpenDevToolsWindow()`. macOS: WKWebView has no programmatic devtools window. Set `isInspectable = true` (macOS 13.3+, via wry `with_devtools(true)` if exposed, or objc2 on the WKWebView) → user opens Web Inspector via Safari's Develop menu / right-click Inspect. Degrade the button to a no-op-with-tooltip or hide it on mac.
5. **Port watcher** — `app/src/port_watcher.rs`. Windows: `GetExtendedTcpTable`. macOS: enumerate listening TCP ports via `libproc` (`proc_listpids` + `proc_pidfdinfo`/`PROC_PIDFDSOCKETINFO`) or the `netstat2` crate, or shell out to `lsof -nP -iTCP -sTCP:LISTEN -F n` and parse ports (simplest, ponytail). Add a `cfg(target_os="macos")` branch to `snapshot_listening_ports`.
6. **Focus reclaim** — `event_loop reclaim_native_focus` (Win32 `SetFocus`). macOS: WKWebView is an NSView in the responder chain; clicking the terminal should restore first responder naturally. Likely **not needed** on mac — gate the reclaim hack `cfg(windows)` and test whether keyboard returns to terminal after webview click. If not, `makeFirstResponder(nil)` on the window on non-webview mouse-down.
7. **Visibility sweep** — `sweep_unpainted_webviews`. Logic is cross-platform; the hide call is wry `set_visible(false)` (works on mac). The `cfg(windows)` gate on the sweep registration should be widened to include macos.

### Won't have on macOS (accept + document)
- Full CDP fidelity for agent click/type (JS synthesis instead — note in README).
- Programmatic DevTools window (Safari Web Inspector instead).

## Suggested sequence (each verified on Mac before next)
1. Un-gate webview creation + `raw_window_handle` for macOS; get a page rendering in a pane. (Biggest unknown — GPUI/warpui NSView compositing vs wry subview; the "airspace" issue is *easier* on mac since it's a real subview, not an overlay HWND.)
2. Screenshot via takeSnapshot.
3. Port watcher via lsof/libproc.
4. Click/type JS synthesis; wire MCP tools to it on mac.
5. DevTools degrade; focus/visibility gate widening.
6. macOS bundle (`script/macos/`), release.

## Effort
~2-3 focused days on the Mac, most risk in step 1 (compositing) and step 3-4 (input fidelity). Everything else is mechanical cfg-branching. Run it from the MacBook with Claude Code + verify loops — reference this repo's Windows impls as the shape to mirror.
