use std::rc::Rc;

use futures::channel::mpsc;
use warpui::elements::{
    ChildView, Container, ConstrainedBox, CrossAxisAlignment, Empty, Expanded, Fill, Flex,
    MainAxisSize, MouseStateHandle, ParentElement,
};
use warpui::text_layout::ClipConfig;
use warpui::ui_components::components::UiComponent;
use warpui::webview_host::{ElementSelection, WebViewHost};
use warpui::{
    AppContext, Element, Entity, ModelHandle, SingletonEntity, TypedActionView, View, ViewContext,
    ViewHandle,
};

use super::PaneId;
use crate::app_state::LeafContents;
use crate::appearance::Appearance;
use crate::pane_group::pane::view;
use crate::pane_group::pane::view::header_content::{StandardHeader, StandardHeaderOptions};
use crate::pane_group::pane::{ShareableLink, ShareableLinkError};
use crate::pane_group::{BackingView, PaneConfiguration, PaneContent, PaneEvent, PaneGroup, PaneView};
use crate::ui_components::buttons::icon_button;
use crate::ui_components::icons::Icon;
use crate::view_components::{SubmittableTextInput, SubmittableTextInputEvent};
use crate::workspace::WorkspaceAction;

/// The browser pane's start page: shown instead of navigating anywhere when the pane opens with
/// no real target (see `BrowserView::new`). Self-contained HTML/CSS/JS (dev.dev branding, static
/// localhost:3000/5173/8080 suggestion cards — no live port-watching), loaded via wry's
/// `with_html` (`WebViewHost::new_start_page`). Bundled with `include_str!` rather than a Rust
/// raw string literal: the page's own CSS uses hex colors and `"#"`-adjacent sequences that would
/// require careful raw-string hash-fencing (`r#"..."#` breaks on an embedded `"#`), so a sibling
/// asset file is the least error-prone way to embed it.
const START_PAGE_HTML: &str = include_str!("start_page.html");

/// Actions dispatched from `BrowserView`'s own toolbar row (back/forward/reload/URL bar/etc, see
/// `render`). Dispatched directly via `ctx.dispatch_typed_action` from the toolbar buttons'
/// `on_click` handlers and routed to `handle_action` below — `BrowserView` is its own
/// `TypedActionView`, since the toolbar lives in its own render tree (not the pane header, which
/// has a separate, unrelated `PaneHeaderAction::CustomAction` mechanism `BackingView::CustomAction`
/// is for; this pane doesn't use it, see `type CustomAction = ()` below).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAction {
    /// Capture the webview's current page and attach it (screenshot + URL) to the active AI
    /// conversation's pending context, same as pasting an image.
    AttachToAI,
    GoBack,
    GoForward,
    Reload,
    OpenDevTools,
    /// Activates the element-selector overlay (hover-highlight, click-to-pick); the picked
    /// element is reported later via the `on_element_selected` callback registered in `new`, not
    /// synchronously from this action.
    StartElementSelection,
}

/// Leaf view backing a browser pane. Holds the `WebViewHost` across `render()` rebuilds since the
/// underlying native WebView2 child window must only be created once.
pub struct BrowserView {
    pane_configuration: ModelHandle<PaneConfiguration>,
    focus_handle: Option<crate::pane_group::focus_state::PaneFocusHandle>,
    host: Rc<WebViewHost>,
    /// The toolbar's URL text field. Shows the page's URL at construction time and whatever the
    /// user last typed/submitted; it does *not* live-update on in-page navigation (clicked
    /// links) — see the module-level limitation note on `navigate`.
    url_input: ViewHandle<SubmittableTextInput>,
    back_mouse_state: MouseStateHandle,
    forward_mouse_state: MouseStateHandle,
    reload_mouse_state: MouseStateHandle,
    attach_to_ai_mouse_state: MouseStateHandle,
    devtools_mouse_state: MouseStateHandle,
    select_element_mouse_state: MouseStateHandle,
}

impl BrowserView {
    pub fn new(url: Option<String>, ctx: &mut ViewContext<Self>) -> Self {
        let pane_configuration = ctx.add_model(|_ctx| PaneConfiguration::new("Browser"));
        // No real target (fresh pane, no last-URL to restore) shows the local start page instead
        // of eagerly loading a hardcoded placeholder URL over the network; see `START_PAGE_HTML`.
        // Any caller with an actual URL (dev-server auto-open, typed URL, restored last-open URL)
        // always takes the normal `WebViewHost::new` path below.
        let initial_url = url.clone();
        let host = match initial_url {
            Some(url) => WebViewHost::new(ctx.window_id(), url),
            None => WebViewHost::new_start_page(ctx.window_id(), START_PAGE_HTML.to_string()),
        };
        // The URL field starts blank for the start page (nothing to show yet — it fills in once
        // the user navigates) rather than the old hardcoded `DEFAULT_BROWSER_URL` placeholder.
        let url_input_initial_text = url.unwrap_or_default();

        let url_input = ctx.add_typed_action_view(|ctx| {
            let mut input = SubmittableTextInput::new(ctx);
            // The toolbar row supplies its own vertical spacing; the input's default outer
            // margins (meant for standalone use in forms) would just add unwanted height here.
            input.set_outer_margins(0., 0., ctx);
            let editor = input.editor().clone();
            editor.update(ctx, |editor, ctx| {
                editor.set_buffer_text_ignoring_undo(&url_input_initial_text, ctx);
            });
            input
        });
        ctx.subscribe_to_view(&url_input, |browser_view, _, event, ctx| {
            if let SubmittableTextInputEvent::Submit(text) = event {
                browser_view.navigate(text.clone(), ctx);
            }
        });

        // Bridges `WebViewHost::set_on_element_selected` (a plain `Fn`, called from the ipc
        // handler on the webview's UI thread, with no `ViewContext` in scope) into the view tree:
        // the callback just pushes onto an unbounded channel, and `spawn_stream_local` polls that
        // channel on the foreground executor and dispatches from there, same bridge shape as
        // `attach_to_ai`'s oneshot channel but repeatable (selection can fire many times, a
        // oneshot can only fire once).
        let (element_selected_tx, element_selected_rx) = mpsc::unbounded::<ElementSelection>();
        host.set_on_element_selected(move |selection| {
            let _ = element_selected_tx.unbounded_send(selection);
        });
        ctx.spawn_stream_local(
            element_selected_rx,
            Self::handle_element_selected,
            |_, _| {},
        );

        // Same channel bridge as `element_selected` above, for `WebViewHost::is_loading`: the
        // page-load handler that flips it lives off the view tree (owned by the `wry::WebView`),
        // so this is the only way to get `BrowserView` to re-render its toolbar's loading bar
        // when navigation starts/finishes.
        let (loading_tx, loading_rx) = mpsc::unbounded::<bool>();
        host.set_on_loading_changed(move |loading| {
            let _ = loading_tx.unbounded_send(loading);
        });
        ctx.spawn_stream_local(loading_rx, Self::handle_loading_changed, |_, _| {});

        // Same channel bridge again, for `WebViewHost::current_favicon`: drives the URL field's
        // leading adornment (see `render_url_field_adornment`). We don't actually fetch/decode the
        // favicon bitmap here (warpui has no remote-image component, see that method's doc
        // comment) — this just triggers a re-render so the adornment can flip between "no icon
        // yet" and "page loaded" states.
        let (favicon_tx, favicon_rx) = mpsc::unbounded::<Option<String>>();
        host.set_on_favicon_changed(move |favicon| {
            let _ = favicon_tx.unbounded_send(favicon);
        });
        ctx.spawn_stream_local(favicon_rx, Self::handle_favicon_changed, |_, _| {});

        Self {
            pane_configuration,
            focus_handle: None,
            host,
            url_input,
            back_mouse_state: MouseStateHandle::default(),
            forward_mouse_state: MouseStateHandle::default(),
            reload_mouse_state: MouseStateHandle::default(),
            attach_to_ai_mouse_state: MouseStateHandle::default(),
            devtools_mouse_state: MouseStateHandle::default(),
            select_element_mouse_state: MouseStateHandle::default(),
        }
    }

    pub fn pane_configuration(&self) -> ModelHandle<PaneConfiguration> {
        self.pane_configuration.clone()
    }

    /// Exposes the underlying `WebViewHost` for the browser-control MCP bridge
    /// (`app/src/browser_mcp`), which drives it off the UI thread via `ModelSpawner`.
    pub(crate) fn webview_host(&self) -> &Rc<WebViewHost> {
        &self.host
    }

    /// Navigates this (already-open) pane to `url`. Used when `Workspace::open_browser_pane`
    /// finds an existing browser pane instead of creating a new one — see the call site in
    /// `workspace/view.rs` for why this is needed (without it, redirecting a dev-server URL to
    /// an already-open pane was silently swallowed).
    pub fn navigate_to(&mut self, url: String, ctx: &mut ViewContext<Self>) {
        self.navigate(url, ctx);
    }

    /// Navigates to the URL typed into the toolbar's URL field, prefixing `https://` if the user
    /// didn't type a scheme (e.g. typing `example.com` rather than `https://example.com`).
    ///
    /// Limitation: this input's displayed text is only ever set here (on submit) and once at
    /// construction — it does *not* live-update when the user navigates inside the page (clicked
    /// links, redirects, JS `history.pushState`), even though `WebViewHost::current_url` does
    /// track those via wry's page-load handler. Wiring that back into this field would mean
    /// re-rendering this view from a non-view context (the handler lives in `WebViewHost`, off
    /// the view tree) — left as a follow-up if live URL display becomes a priority.
    fn navigate(&mut self, text: String, ctx: &mut ViewContext<Self>) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let url = if trimmed.contains("://") {
            trimmed.to_string()
        } else {
            format!("https://{trimmed}")
        };
        self.host.load_url(&url);
        // Bug fix: SubmittableTextInput clears its own buffer on submit (see
        // `on_try_submit`), so without this the URL field goes blank right after Enter.
        // Re-set it to the URL we just navigated to.
        self.set_url_input_text(&url, ctx);
    }

    /// Sets the toolbar URL field's displayed text without affecting undo history. Used both
    /// after a user-submitted navigation (see `navigate`) and when an already-open pane is
    /// redirected to a new URL from outside (see `BrowserPane::navigate`).
    fn set_url_input_text(&self, url: &str, ctx: &mut ViewContext<Self>) {
        let editor = self.url_input.as_ref(ctx).editor().clone();
        editor.update(ctx, |editor, ctx| {
            editor.set_buffer_text_ignoring_undo(url, ctx);
        });
    }

    /// Captures a screenshot of the page via CDP and dispatches it (+ the current URL) as pending
    /// AI context, mirroring how a pasted image is attached. The capture itself completes
    /// asynchronously (COM callback), so it's bridged into a `ctx.spawn`'d future via a oneshot
    /// channel.
    fn attach_to_ai(&mut self, ctx: &mut ViewContext<Self>) {
        let url = self.host.current_url();
        let (tx, rx) = futures::channel::oneshot::channel();
        self.host.capture_screenshot(move |result| {
            let _ = tx.send(result);
        });
        ctx.spawn(async move { rx.await }, move |_this, result, ctx| {
            let png_bytes = match result {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(err)) => {
                    log::warn!("Failed to capture browser pane screenshot: {err:?}");
                    return;
                }
                Err(_canceled) => {
                    log::warn!("Browser pane screenshot capture was dropped before completing");
                    return;
                }
            };
            ctx.dispatch_typed_action(&WorkspaceAction::AttachBrowserScreenshot {
                png_bytes,
                url,
            });
        });
    }

    /// Dispatches a picked element (from the `spawn_stream_local` bridge set up in `new`) as
    /// pending AI context.
    fn handle_element_selected(&mut self, selection: ElementSelection, ctx: &mut ViewContext<Self>) {
        ctx.dispatch_typed_action(&WorkspaceAction::AttachBrowserElement {
            html: selection.html,
            selector: selection.selector,
            url: self.host.current_url(),
        });
    }

    /// Re-renders the toolbar when `WebViewHost::is_loading` changes (from the channel bridge set
    /// up in `new`), so the loading bar (see `render_loading_bar`) shows/hides in step with
    /// navigation. The value itself is read straight off `self.host.is_loading()` at render time,
    /// not stored here — this handler only exists to trigger the re-render.
    fn handle_loading_changed(&mut self, _loading: bool, ctx: &mut ViewContext<Self>) {
        ctx.notify();
    }

    /// Re-renders the toolbar when `WebViewHost::current_favicon` changes (from the channel
    /// bridge set up in `new`), same shape as `handle_loading_changed`. The value itself is read
    /// straight off `self.host.current_favicon()` at render time, not stored here.
    fn handle_favicon_changed(&mut self, _favicon: Option<String>, ctx: &mut ViewContext<Self>) {
        ctx.notify();
    }

    /// Builds a single toolbar icon button that dispatches `action` on click.
    fn toolbar_button(
        appearance: &Appearance,
        icon: Icon,
        mouse_state: MouseStateHandle,
        tooltip: &'static str,
        action: BrowserAction,
    ) -> Box<dyn Element> {
        let ui_builder = appearance.ui_builder().clone();
        icon_button(appearance, icon, false, mouse_state)
            .with_tooltip(move || ui_builder.tool_tip(tooltip.to_string()).build().finish())
            .build()
            .on_click(move |ctx, _, _| ctx.dispatch_typed_action(action.clone()))
            .finish()
    }

    /// Builds a thin 1px vertical rule, used to separate the toolbar's button groups from the URL
    /// field. Not a reusable `Divider` widget because warpui doesn't have one (dividers elsewhere
    /// in this codebase are hand-built the same way, as a `Border` on a `Container`); here it's a
    /// standalone element instead, since it sits between siblings rather than around one of them.
    /// Height is inset a little from the toolbar's full height so it doesn't touch the row edges.
    fn toolbar_divider(appearance: &Appearance) -> Box<dyn Element> {
        ConstrainedBox::new(
            Container::new(Empty::new().finish())
                .with_background(appearance.theme().outline())
                .finish(),
        )
        .with_width(1.)
        .with_height(20.)
        .finish()
    }

    /// Builds the small leading icon shown at the left of the URL field: a spinner while a
    /// navigation is in flight, a globe once the page has loaded, nothing before either has
    /// happened yet.
    ///
    /// This is not the page's actual favicon bitmap. `WebViewHost::current_favicon` gives us the
    /// favicon *URL* (reported by the init script), but warpui's renderer has no component for
    /// loading an arbitrary remote image (checked: no `NetworkImage`/URL-backed image element
    /// anywhere in `warpui`/`ui_components` — the only image display paths, e.g.
    /// `lightbox.rs`, work off already-decoded local bytes). Building an async fetch-decode-cache
    /// pipeline just for a 16px favicon is more machinery than this is worth, so we fall back to
    /// a static icon that still reads as "this row has a page" without the real bitmap.
    fn render_url_field_adornment(&self, appearance: &Appearance) -> Box<dyn Element> {
        let icon = if self.host.is_loading() {
            Icon::ClockLoader
        } else if self.host.current_favicon().is_some() {
            Icon::Globe
        } else {
            return Empty::new().finish();
        };
        let color = appearance.theme().sub_text_color(appearance.theme().background());
        Container::new(
            ConstrainedBox::new(icon.to_warpui_icon(color).finish())
                .with_width(14.)
                .with_height(14.)
                .finish(),
        )
        .with_padding_right(6.)
        .finish()
    }

    /// Builds the toolbar row rendered above the webview: back/forward/reload, the URL field, and
    /// attach-screenshot/select-element/attach-console/DevTools.
    fn render_toolbar(&self, appearance: &Appearance) -> Box<dyn Element> {
        let mut row = Flex::row()
            .with_cross_axis_alignment(CrossAxisAlignment::Center)
            .with_main_axis_size(MainAxisSize::Max)
            .with_spacing(2.);

        row.add_child(Self::toolbar_button(
            appearance,
            Icon::ArrowLeft,
            self.back_mouse_state.clone(),
            "Back",
            BrowserAction::GoBack,
        ));
        row.add_child(Self::toolbar_button(
            appearance,
            Icon::ArrowRight,
            self.forward_mouse_state.clone(),
            "Forward",
            BrowserAction::GoForward,
        ));
        row.add_child(Self::toolbar_button(
            appearance,
            Icon::Refresh,
            self.reload_mouse_state.clone(),
            "Reload",
            BrowserAction::Reload,
        ));
        row.add_child(Self::toolbar_divider(appearance));
        row.add_child(
            Expanded::new(
                1.,
                Container::new(
                    Flex::row()
                        .with_cross_axis_alignment(CrossAxisAlignment::Center)
                        .with_child(self.render_url_field_adornment(appearance))
                        .with_child(Expanded::new(1., ChildView::new(&self.url_input).finish()).finish())
                        .finish(),
                )
                .with_padding_left(8.)
                .with_padding_right(8.)
                .finish(),
            )
            .finish(),
        );
        row.add_child(Self::toolbar_divider(appearance));
        row.add_child(Self::toolbar_button(
            appearance,
            Icon::Image,
            self.attach_to_ai_mouse_state.clone(),
            "Attach screenshot → terminal",
            BrowserAction::AttachToAI,
        ));
        row.add_child(Self::toolbar_button(
            appearance,
            Icon::SelectElement,
            self.select_element_mouse_state.clone(),
            "Select element → terminal",
            BrowserAction::StartElementSelection,
        ));
        row.add_child(Self::toolbar_button(
            appearance,
            Icon::Code2,
            self.devtools_mouse_state.clone(),
            "Open DevTools",
            BrowserAction::OpenDevTools,
        ));

        Container::new(row.finish())
            .with_padding_left(6.)
            .with_padding_right(6.)
            .with_padding_top(4.)
            .with_padding_bottom(4.)
            .finish()
    }

    /// Builds the thin loading indicator shown between the toolbar and the webview: a full-width
    /// 2px bar, filled with the theme's accent color while a navigation is in flight (see
    /// `WebViewHost::is_loading`) and transparent otherwise. The 2px of vertical space is always
    /// reserved (rather than the row collapsing to 0 height when idle) so the toolbar doesn't
    /// jump as navigations start and finish.
    fn render_loading_bar(&self, appearance: &Appearance) -> Box<dyn Element> {
        let fill: Fill = if self.host.is_loading() {
            appearance.theme().accent().into()
        } else {
            Fill::None
        };
        ConstrainedBox::new(
            Flex::row()
                .with_main_axis_size(MainAxisSize::Max)
                .with_child(
                    Expanded::new(1., Container::new(Empty::new().finish()).with_background(fill).finish())
                        .finish(),
                )
                .finish(),
        )
        .with_height(2.)
        .finish()
    }
}

impl Entity for BrowserView {
    type Event = PaneEvent;
}

impl TypedActionView for BrowserView {
    type Action = BrowserAction;

    fn handle_action(&mut self, action: &Self::Action, ctx: &mut ViewContext<Self>) {
        match action {
            BrowserAction::AttachToAI => self.attach_to_ai(ctx),
            BrowserAction::GoBack => self.host.go_back(),
            BrowserAction::GoForward => self.host.go_forward(),
            BrowserAction::Reload => self.host.reload(),
            BrowserAction::OpenDevTools => self.host.open_devtools(),
            BrowserAction::StartElementSelection => self.host.start_element_selection(),
        }
    }
}

impl View for BrowserView {
    fn ui_name() -> &'static str {
        "BrowserView"
    }

    fn render(&self, app: &AppContext) -> Box<dyn Element> {
        let appearance = Appearance::as_ref(app);
        Flex::column()
            .with_children([
                self.render_toolbar(appearance),
                self.render_loading_bar(appearance),
                Expanded::new(1., Box::new(self.host.element()) as Box<dyn Element>).finish(),
            ])
            .finish()
    }

    /// Called by the framework when this view (e.g. via a cross-window tab drag) is moved to a
    /// different OS window. The native WebView2 child HWND is parented to whatever window it was
    /// created under and can't be reparented across windows, so re-bind the host: this tears down
    /// the old child HWND and lets the next `paint` lazily recreate it under the new window, at
    /// the same URL. In-page navigation history is lost — acceptable, same tradeoff as the
    /// pane not surviving app restart (see `LeafContents::Browser`).
    fn on_window_transferred(
        &mut self,
        _source_window_id: warpui::WindowId,
        target_window_id: warpui::WindowId,
        _ctx: &mut ViewContext<Self>,
    ) {
        self.host.rebind_window(target_window_id);
    }
}

/// Uninhabited marker so `PaneHeaderAction<BrowserPaneHeaderAction, ()>` gets a unique
/// `TypeId`. With `type PaneHeaderOverflowMenuAction = ()` this pane registered under the
/// same `PaneHeaderAction<(), ()>` action type as `GetStartedView`, and the typed-action
/// registry (keyed by `TypeId`, see `warpui_core::core::action::ActionType`) kept only one
/// handler, so browser pane header drops were silently swallowed.
#[derive(Debug, Clone)]
pub enum BrowserPaneHeaderAction {}

impl BackingView for BrowserView {
    type PaneHeaderOverflowMenuAction = BrowserPaneHeaderAction;
    // The toolbar row (back/forward/reload/URL/attach-screenshot/select-element/attach-console/
    // DevTools) covers everything this pane needs; nothing dispatches through the pane-header
    // custom-action path.
    type CustomAction = ();
    type AssociatedData = ();

    fn handle_pane_header_overflow_menu_action(
        &mut self,
        _action: &Self::PaneHeaderOverflowMenuAction,
        _ctx: &mut ViewContext<Self>,
    ) {
        // No overflow menu items yet.
    }

    fn handle_custom_action(&mut self, _action: &Self::CustomAction, _ctx: &mut ViewContext<Self>) {
        // No pane-header custom actions; see `type CustomAction = ()` above.
    }

    fn close(&mut self, ctx: &mut ViewContext<Self>) {
        ctx.emit(PaneEvent::Close);
    }

    fn focus_contents(&mut self, _ctx: &mut ViewContext<Self>) {
        // The native webview manages its own focus once clicked into; nothing to do here yet.
    }

    fn render_header_content(
        &self,
        _ctx: &view::HeaderRenderContext<'_>,
        _app: &AppContext,
    ) -> view::HeaderContent {
        view::HeaderContent::Standard(StandardHeader {
            title: "Browser".to_string(),
            title_secondary: None,
            title_style: None,
            title_clip_config: ClipConfig::start(),
            title_max_width: None,
            left_of_title: None,
            right_of_title: None,
            left_of_overflow: None,
            options: StandardHeaderOptions::default(),
        })
    }

    fn set_focus_handle(
        &mut self,
        focus_handle: crate::pane_group::focus_state::PaneFocusHandle,
        _ctx: &mut ViewContext<Self>,
    ) {
        self.focus_handle = Some(focus_handle);
    }
}

/// `PaneContent` implementation for a browser pane. Not persisted across restarts, since the
/// native WebView2 child window can't be serialized (see `LeafContents::Browser`).
pub struct BrowserPane {
    view: ViewHandle<PaneView<BrowserView>>,
    pane_configuration: ModelHandle<PaneConfiguration>,
}

impl BrowserPane {
    /// Returns a handle to the backing `BrowserView`, so callers outside this module (e.g.
    /// `Workspace::open_browser_pane` redirecting an already-open pane to a new URL) can drive
    /// navigation without reaching into pane-header/`BackingView::CustomAction` plumbing this
    /// pane doesn't use.
    pub fn browser_view(&self, ctx: &AppContext) -> ViewHandle<BrowserView> {
        self.view.as_ref(ctx).child(ctx)
    }

    pub fn new<V: View>(url: Option<String>, ctx: &mut ViewContext<V>) -> Self {
        let browser_view = ctx.add_typed_action_view(|ctx| BrowserView::new(url, ctx));
        let pane_configuration = browser_view.as_ref(ctx).pane_configuration();
        let pane_view = ctx.add_typed_action_view(|ctx| {
            let pane_id = PaneId::from_browser_pane_ctx(ctx);
            PaneView::new(
                pane_id,
                browser_view,
                (),
                pane_configuration.clone(),
                ctx,
            )
        });
        Self {
            view: pane_view,
            pane_configuration,
        }
    }
}

impl PaneContent for BrowserPane {
    fn id(&self) -> PaneId {
        PaneId::from_browser_pane_view(&self.view)
    }

    fn attach(
        &self,
        _group: &PaneGroup,
        focus_handle: crate::pane_group::focus_state::PaneFocusHandle,
        ctx: &mut ViewContext<PaneGroup>,
    ) {
        self.view
            .update(ctx, |view, ctx| view.set_focus_handle(focus_handle, ctx));
        let child = self.view.as_ref(ctx).child(ctx);

        let pane_id = self.id();
        ctx.subscribe_to_view(&child, move |pane_group, _, event, ctx| {
            pane_group.handle_pane_event(pane_id, event, ctx);
        });

        // Subscribe to the PaneView wrapper itself (not just the browser content view) so
        // header drag/drop events (PaneViewEvent, e.g. MovePaneWithinPaneGroup) reach
        // PaneGroup::handle_pane_view_event. Every other draggable pane type (TerminalPane,
        // CodePane, AIDocumentPane, etc.) does this; BrowserPane was copied from
        // GetStartedPane, which never needed it since the welcome pane isn't dragged.
        ctx.subscribe_to_view(&self.view, move |group, _, event, ctx| {
            group.handle_pane_view_event(pane_id, event, ctx);
        });
    }

    fn detach(
        &self,
        _group: &PaneGroup,
        _detach_type: super::DetachType,
        ctx: &mut ViewContext<PaneGroup>,
    ) {
        let child = self.view.as_ref(ctx).child(ctx);
        let mut closing_url = None;
        child.update(ctx, |view, _ctx| {
            closing_url = Some(view.host.current_url());
            view.host.destroy();
        });
        // Remember the URL so a later reopen (toggle, globe button, Ctrl+Shift+B) picks up where
        // the user left off instead of resetting to the default URL. Handled by `Workspace`, an
        // ancestor of `PaneGroup` in the view tree, so it's reachable via typed-action bubbling.
        //
        // Must be deferred, not dispatched synchronously: `detach` runs nested inside the
        // dispatch of whatever action closed this pane (e.g. `ToggleBrowserPane`), and all
        // `WorkspaceAction` variants share one handler-map slot keyed by the enum's `TypeId`
        // (see `ActionType` in warpui_core). `dispatch_typed_action` temporarily removes that
        // slot for the duration of the outer dispatch, so a synchronous nested dispatch of a
        // *different* `WorkspaceAction` variant finds the slot empty and logs "no handlers"
        // (this is exactly the `StashLastBrowserPaneUrl` warning). Deferring queues it as a
        // pending effect that runs after the outer dispatch finishes and reinserts the slot.
        if let Some(url) = closing_url {
            ctx.dispatch_typed_action_deferred(WorkspaceAction::StashLastBrowserPaneUrl(url));
        }
        ctx.unsubscribe_to_view(&child);
    }

    fn snapshot(&self, _ctx: &AppContext) -> LeafContents {
        LeafContents::Browser
    }

    fn has_application_focus(&self, ctx: &mut ViewContext<PaneGroup>) -> bool {
        self.view.is_self_or_child_focused(ctx)
    }

    fn focus(&self, ctx: &mut ViewContext<PaneGroup>) {
        self.view
            .as_ref(ctx)
            .child(ctx)
            .update(ctx, BackingView::focus_contents)
    }

    fn shareable_link(
        &self,
        _ctx: &mut ViewContext<PaneGroup>,
    ) -> Result<ShareableLink, ShareableLinkError> {
        Ok(ShareableLink::Base)
    }

    fn pane_configuration(&self) -> ModelHandle<PaneConfiguration> {
        self.pane_configuration.clone()
    }

    fn is_pane_being_dragged(&self, ctx: &AppContext) -> bool {
        self.view.as_ref(ctx).is_being_dragged()
    }
}
