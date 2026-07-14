# dev.dev

An unofficial fork of Warp that adds a Cursor-style integrated browser wired to your terminal and CLI coding agents.

> **Unofficial fork notice.** This project is an independent fork of [warpdotdev/warp](https://github.com/warpdotdev/warp). It is **not affiliated with, endorsed by, or sponsored by Warp** (Warp Terminal / Warp.dev). "Warp" is a trademark of its respective owner; this fork is not an official Warp product. Licensed under **AGPL-3.0**, the same license as upstream.

## What it adds

On top of the stock Warp terminal, this fork adds a docked browser pane and an MCP server that lets CLI coding agents (Claude Code and others) drive it:

- **Integrated WebView2 browser pane (Windows).** A native WebView2 child window rendered inside a standard Warp pane — resizable and splittable like any other pane in the layout.
- **Auto-open on dev-server URLs.** Detects `localhost` URLs printed by dev-server processes and opens the browser pane automatically, even for backgrounded processes. Uses two detection paths: an instant terminal-output scan, and a polling grid-scan / port-watcher fallback that catches servers whose startup banner scrolled out of view.
- **Browser toolbar.** Back / forward / reload, a URL bar, a button to open the DevTools window, a screenshot button, and an element-selector button.
- **Element selector → terminal.** Pick any element in the page and its HTML plus CSS selector are inserted directly into the active terminal's input, ready for your CLI coding agent to read.
- **Screenshot → terminal.** Captures the page, saves it to a temp file, and inserts the file path into the active terminal input so an agent (or you) can reference it immediately.
- **Toggle anywhere.** Click the globe icon in the titlebar or press `Ctrl+Shift+B` to show or hide the pane.
- **Draggable, resizable pane.** The browser lives in Warp's normal pane-split tree — drag borders to resize, split it alongside terminal panes.
- **MCP server for agents.** A local MCP server exposes the browser to CLI agents like Claude Code, so they can navigate, click, type, and inspect the page as part of their own workflow.

## MCP setup

Warp starts a local MCP server on a fixed port and prints a ready-to-paste registration command (and a bearer token) at startup. The token is persisted at `~/.warp/browser-mcp/token`, so this is a one-time setup — it survives restarts.

```
claude mcp add --transport http warp-browser http://127.0.0.1:9287/mcp --header "Authorization: Bearer <token>"
```

Tools exposed:

| Tool | Description |
|---|---|
| `browser_navigate` | Navigate the pane to a URL |
| `browser_current_url` | Read the current URL |
| `browser_screenshot` | Capture a screenshot of the page |
| `browser_click` | Click an element |
| `browser_type` | Type into an element |
| `browser_console` | Read browser console output |
| `browser_evaluate` | Evaluate arbitrary JavaScript in the page |

## Platform

The browser pane and MCP server are **Windows only** for now (built on WebView2 and CDP). macOS and Linux support is planned but not yet implemented.

## Build from source

Known-good path on Windows:

1. Install **VS Build Tools 2022** with the **C++ workload** and **Spectre-mitigated libraries**, plus **CMake** and **protoc**.
2. Run `./script/windows/bootstrap.ps1`.
3. Set the `PROTOC` environment variable to your `protoc.exe`. If you installed protoc via WinGet, it lands under a package-hash path, not a stable one — point `PROTOC` at it explicitly, e.g.:
   ```
   PROTOC=C:\Users\<you>\AppData\Local\Microsoft\WinGet\Packages\Google.Protobuf_Microsoft.Winget.Source_8wekyb3d8bbwe\bin\protoc.exe
   ```
4. Build:
   ```
   cargo build --bin warp-oss --features gui,release_bundle
   ```

## Credit

Built on top of [warpdotdev/warp](https://github.com/warpdotdev/warp) — all credit for the base terminal goes to the Warp team. This fork only adds the browser pane and MCP integration described above.
