// Phase 5a bootstrap: console capture + element selector. Injected via
// `WebViewBuilder::with_initialization_script`, so it re-runs on every navigation before page
// scripts (webview_host.rs). Talks back to Rust over `window.ipc.postMessage(JSON.stringify(...))`
// using a `{type: "..."}` envelope (see `WebViewHost::ensure_webview`'s ipc handler).
(function () {
  if (window.__warpInit) return;
  window.__warpInit = true;

  var CAP = 200;
  var ring = [];
  window.__warpConsoleBuffer = ring; // client-side ring buffer, mirrors the Rust-side cap

  function post(obj) {
    try {
      window.ipc.postMessage(JSON.stringify(obj));
    } catch (e) {
      // ipc unavailable (e.g. not yet ready) - drop silently, nothing to recover into.
    }
  }

  function stringifyArg(a) {
    if (typeof a === "string") return a;
    try {
      return JSON.stringify(a);
    } catch (e) {
      return String(a);
    }
  }

  function pushConsole(level, message, stack) {
    var entry = { type: "console", level: level, message: message, stack: stack };
    ring.push(entry);
    if (ring.length > CAP) ring.shift();
    post(entry);
  }

  ["log", "warn", "error", "info"].forEach(function (level) {
    var original = console[level] ? console[level].bind(console) : function () {};
    console[level] = function () {
      pushConsole(level, Array.prototype.map.call(arguments, stringifyArg).join(" "));
      original.apply(console, arguments);
    };
  });

  window.addEventListener("error", function (event) {
    pushConsole("error", event.message, event.error && event.error.stack);
  });

  window.addEventListener("unhandledrejection", function (event) {
    var reason = event.reason;
    var message = reason && reason.message ? reason.message : String(reason);
    pushConsole("error", "Unhandled promise rejection: " + message, reason && reason.stack);
  });

  // Favicon: reported once per page load (and again on late <link> insertion, e.g. SPA route
  // changes that patch the head after initial render) so the toolbar can show it next to the URL
  // field (WebViewHost's ipc handler -> on_favicon_changed -> BrowserView).
  function reportFavicon() {
    var link = document.querySelector(
      "link[rel~='icon'], link[rel='shortcut icon'], link[rel='apple-touch-icon']"
    );
    var href = link && link.href ? link.href : location.origin + "/favicon.ico";
    if (href === lastFavicon) return;
    lastFavicon = href;
    post({ type: "favicon", url: href });
  }
  var lastFavicon = null;
  if (document.readyState === "complete") {
    reportFavicon();
  } else {
    window.addEventListener("load", reportFavicon);
  }
  // Cheap late-check for SPA head mutations; a full MutationObserver is more machinery than this
  // is worth (favicon changes are rare and one extra poll costs nothing).
  setTimeout(reportFavicon, 1500);

  // Element selector: activated from Rust via `window.__warpSelector.start()`
  // (WebViewHost::start_element_selection -> evaluate_script).
  //
  // Accent is a fixed constant, not the app's real accent color: this JS runs inside the
  // page's own document, which has no access to the host app's `Appearance` (that's a Rust-side
  // theme object, not exposed over the ipc bridge). #2dd4bf (teal) matches the app's unified
  // accent and reads fine over light or dark pages.
  var ACCENT = "#2dd4bf";
  var overlay = null;
  var label = null;
  var active = false;
  var hovered = null;

  function ensureOverlay() {
    if (overlay) return overlay;
    overlay = document.createElement("div");
    overlay.style.cssText =
      "position:fixed;pointer-events:none;z-index:2147483647;" +
      "border:1.5px solid " + ACCENT + ";" +
      "background:rgba(45,212,191,0.12);" +
      "box-shadow:0 0 0 1px rgba(45,212,191,0.25),0 2px 8px rgba(0,0,0,0.25);" +
      "border-radius:2px;" +
      "transition:left 60ms ease-out,top 60ms ease-out,width 60ms ease-out,height 60ms ease-out;" +
      "box-sizing:border-box;";
    document.documentElement.appendChild(overlay);
    return overlay;
  }

  function ensureLabel() {
    if (label) return label;
    label = document.createElement("div");
    label.style.cssText =
      "position:fixed;pointer-events:none;z-index:2147483647;" +
      "background:rgba(17,24,39,0.92);color:#f9fafb;" +
      "font:500 11px/1.4 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;" +
      "padding:3px 7px;border-radius:4px;white-space:nowrap;" +
      "box-shadow:0 2px 6px rgba(0,0,0,0.3);" +
      "transition:left 60ms ease-out,top 60ms ease-out;";
    document.documentElement.appendChild(label);
    return label;
  }

  function describeElement(el, rect) {
    var tag = el.nodeName.toLowerCase();
    var firstClass = el.classList && el.classList.length ? "." + el.classList[0] : "";
    var dims = Math.round(rect.width) + "×" + Math.round(rect.height);
    return tag + firstClass + " — " + dims;
  }

  function cssPath(el) {
    if (!(el instanceof Element)) return "";
    var path = [];
    while (el && el.nodeType === Node.ELEMENT_NODE) {
      var part = el.nodeName.toLowerCase();
      if (el.id) {
        path.unshift(part + "#" + el.id);
        break;
      }
      var nth = 1;
      var sibling = el;
      while ((sibling = sibling.previousElementSibling)) {
        if (sibling.nodeName.toLowerCase() === part) nth++;
      }
      path.unshift(part + ":nth-of-type(" + nth + ")");
      el = el.parentElement;
    }
    return path.join(" > ");
  }

  function onMouseMove(e) {
    if (!active || e.target === hovered) return;
    hovered = e.target;
    var rect = hovered.getBoundingClientRect();
    var ov = ensureOverlay();
    ov.style.left = rect.left + "px";
    ov.style.top = rect.top + "px";
    ov.style.width = rect.width + "px";
    ov.style.height = rect.height + "px";

    // Chip floats just above the highlighted element; flips below when there's no room above
    // (e.g. element pinned to the top of the viewport) so it's never clipped off-screen.
    var lb = ensureLabel();
    lb.textContent = describeElement(hovered, rect);
    var chipHeight = 22;
    var top = rect.top - chipHeight - 4;
    if (top < 0) top = rect.bottom + 4;
    lb.style.left = Math.max(0, rect.left) + "px";
    lb.style.top = top + "px";
  }

  function onClick(e) {
    if (!active) return;
    e.preventDefault();
    e.stopPropagation();
    var el = e.target;
    var rect = el.getBoundingClientRect();
    var html = el.outerHTML || "";
    var text = el.innerText || "";
    post({
      type: "elementSelected",
      html: html.length > 8192 ? html.slice(0, 8192) : html,
      selector: cssPath(el),
      classes: el.className ? String(el.className).split(/\s+/).filter(Boolean) : [],
      rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
      text: text.length > 1024 ? text.slice(0, 1024) : text,
    });
    stop();
  }

  function onKeyDown(e) {
    if (active && e.key === "Escape") stop();
  }

  function start() {
    if (active) return;
    active = true;
    document.addEventListener("mousemove", onMouseMove, true);
    document.addEventListener("click", onClick, true);
    document.addEventListener("keydown", onKeyDown, true);
  }

  function stop() {
    active = false;
    hovered = null;
    document.removeEventListener("mousemove", onMouseMove, true);
    document.removeEventListener("click", onClick, true);
    document.removeEventListener("keydown", onKeyDown, true);
    if (overlay && overlay.parentNode) overlay.parentNode.removeChild(overlay);
    if (label && label.parentNode) label.parentNode.removeChild(label);
    overlay = null;
    label = null;
  }

  window.__warpSelector = { start: start, stop: stop };
})();
