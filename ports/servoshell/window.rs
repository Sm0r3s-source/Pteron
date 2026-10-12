/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use euclid::Scale;
use log::warn;
use servo::{
    AuthenticationRequest, BluetoothDeviceSelectionRequest, ConsoleLogLevel,
    CreateNewWebViewRequest, Cursor, DeviceIndependentIntRect, DeviceIndependentPixel,
    DeviceIntPoint, DeviceIntSize, DevicePixel, EmbedderControl, EmbedderControlId, InputEventId,
    InputEventResult, MediaSessionEvent, MediaSessionPlaybackState, PermissionRequest, RenderingContext, ScreenGeometry,
    WebView, WebViewBuilder, WebViewId,
};
use url::Url;

use crate::parser::location_bar_input_to_url;
use crate::running_app_state::{RunningAppState, UserInterfaceCommand, WebViewCollection};

// This should vary by zoom level and maybe actual text size (focused or under cursor)
#[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
pub(crate) const LINE_HEIGHT: f32 = 76.0;
#[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
pub(crate) const LINE_WIDTH: f32 = 76.0;

/// <https://github.com/web-platform-tests/wpt/blob/9320b1f724632c52929a3fdb11bdaf65eafc7611/webdriver/tests/classic/set_window_rect/set.py#L287-L290>
/// "A window size of 10x10px shouldn't be supported by any browser."
#[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
pub(crate) const MIN_WINDOW_INNER_SIZE: DeviceIntSize = DeviceIntSize::new(100, 100);

static SERVOSHELL_WINDOW_ID: AtomicU64 = AtomicU64::new(0);

/// Pteron: how long a background tab may sit unused before it is put to sleep.
/// Set `PTERON_TAB_SLEEP_SECS` to change it; `0` turns tab sleeping off. Default: 300 s.
#[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
pub(crate) fn tab_sleep_timeout() -> Option<Duration> {
    // `PTERON_TAB_SLEEP_SECS`, when set, wins over the setting (useful for tests).
    static ENV_SECS: OnceLock<Option<u64>> = OnceLock::new();
    let env_secs = *ENV_SECS.get_or_init(|| {
        std::env::var("PTERON_TAB_SLEEP_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
    });
    let secs = env_secs.unwrap_or_else(|| {
        crate::memsaver::tab_sleep_secs(u64::from(crate::pteron::tab_sleep_minutes()) * 60)
    });
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Pteron: ask Windows to take back memory that Servo has freed but the heap still holds.
/// Closing a tab frees memory inside Servo, but the Windows heap keeps it reserved, so
/// nothing shows up in Task Manager. Compacting the heap and trimming the working set
/// hands it back. Does nothing on other platforms.
#[cfg(windows)]
pub(crate) fn trim_process_memory() {
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact};
    use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: these calls only take handles for the current process and flags, and have no
    // preconditions beyond a valid handle, which the OS gives us.
    unsafe {
        HeapCompact(GetProcessHeap(), 0);
        K32EmptyWorkingSet(GetCurrentProcess());
    }
}

#[cfg(not(windows))]
pub(crate) fn trim_process_memory() {}

/// Pteron: a background tab whose `WebView` has been closed to free memory.
/// Only its address and title are kept; clicking it creates a fresh `WebView`.
#[derive(Clone, Debug)]
pub(crate) struct SleepingTab {
    pub(crate) url: Url,
    pub(crate) title: Option<String>,
    /// Pteron: pinned tabs are listed first and survive restarts.
    pub(crate) pinned: bool,
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub(crate) struct ServoShellWindowId(u64);

thread_local! {
    /// Pteron: tab order chosen by dragging (empty until the user drags a tab).
    static TAB_ORDER: RefCell<Vec<WebViewId>> = const { RefCell::new(Vec::new()) };
}

impl From<u64> for ServoShellWindowId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl ServoShellWindowId {
    #[cfg_attr(not(any(target_os = "android", target_env = "ohos")), expect(unused))]
    pub(crate) fn next() -> ServoShellWindowId {
        ServoShellWindowId(SERVOSHELL_WINDOW_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }
}

pub(crate) struct ServoShellWindow {
    /// The [`WebView`]s that have been added to this window.
    pub(crate) webview_collection: RefCell<WebViewCollection>,
    /// A handle to the [`PlatformWindow`] that servoshell is rendering in.
    platform_window: Rc<dyn PlatformWindow>,
    /// Whether or not this window should be closed at the end of the spin of the next event loop.
    close_scheduled: Cell<bool>,
    /// Whether or not the application interface needs to be updated.
    needs_update: Cell<bool>,
    /// Whether or not Servo needs to repaint its display. Currently this is global
    /// because every `WebView` shares a `RenderingContext`.
    needs_repaint: Cell<bool>,
    /// List of webviews that have favicon textures which are not yet uploaded
    /// to the GPU by egui.
    pending_favicon_loads: RefCell<Vec<WebViewId>>,
    /// Pending [`UserInterfaceCommand`] that have yet to be processed by the main loop.
    pending_commands: RefCell<Vec<UserInterfaceCommand>>,
    /// Pteron: tabs that were put to sleep to save memory.
    sleeping_tabs: RefCell<Vec<SleepingTab>>,
    /// Pteron: when each background tab was last in the foreground.
    last_active: RefCell<HashMap<WebViewId, Instant>>,
    /// Pteron: times at which to hand freed memory back to the OS after tabs went to sleep.
    /// Servo finishes tearing a tab down a little after it is closed, so we trim twice.
    pending_trims: RefCell<Vec<Instant>>,
    /// Pteron: tabs that are currently playing audio/video. These never sleep.
    media_playing: RefCell<HashSet<WebViewId>>,
    /// Pteron: pinned tabs.
    pinned: RefCell<HashSet<WebViewId>>,
    /// Pteron: when the session file was last refreshed.
    last_session_save: Cell<Option<Instant>>,
    /// Pteron: addresses of recently closed tabs, newest last (Ctrl+Shift+T reopens one).
    closed_tabs: RefCell<Vec<Url>>,
}

pub(crate) enum TopLevelWebViewCreationRequest {
    WithUrl(Url),
    WithCreateRequest(CreateNewWebViewRequest),
}

impl ServoShellWindow {
    pub(crate) fn new(platform_window: Rc<dyn PlatformWindow>) -> Self {
        Self {
            webview_collection: Default::default(),
            platform_window,
            close_scheduled: Default::default(),
            needs_update: Default::default(),
            needs_repaint: Default::default(),
            pending_favicon_loads: Default::default(),
            pending_commands: Default::default(),
            sleeping_tabs: Default::default(),
            last_active: Default::default(),
            pending_trims: Default::default(),
            media_playing: Default::default(),
            pinned: Default::default(),
            last_session_save: Default::default(),
            closed_tabs: Default::default(),
        }
    }

    pub(crate) fn id(&self) -> ServoShellWindowId {
        self.platform_window().id()
    }

    /// Must be called *after* `self` is in `state.windows`, otherwise it will panic.
    pub(crate) fn create_and_activate_toplevel_webview(
        self: &Rc<Self>,
        state: Rc<RunningAppState>,
        creation_type: TopLevelWebViewCreationRequest,
    ) -> WebView {
        let webview = self.create_toplevel_webview(state, creation_type);
        self.activate_webview(webview.id());
        webview
    }

    /// Must be called *after* `self` is in `state.windows`, otherwise it will panic.
    #[servo::servo_tracing::instrument(skip(self, state, creation_type))]
    pub(crate) fn create_toplevel_webview(
        &self,
        state: Rc<RunningAppState>,
        creation_type: TopLevelWebViewCreationRequest,
    ) -> WebView {
        let mut webview_builder = match creation_type {
            TopLevelWebViewCreationRequest::WithUrl(url) => {
                WebViewBuilder::new(state.servo(), self.platform_window.rendering_context())
                    .url(url)
            },
            TopLevelWebViewCreationRequest::WithCreateRequest(request) => {
                request.builder(self.platform_window.rendering_context())
            },
        };
        webview_builder = webview_builder
            .hidpi_scale_factor(self.platform_window.hidpi_scale_factor())
            .user_content_manager(state.user_content_manager.clone())
            .delegate(state.clone());

        #[cfg(all(
            feature = "gamepad",
            not(any(target_os = "android", target_env = "ohos"))
        ))]
        if let Some(gamepad_delegate) = state.gamepad_delegate() {
            webview_builder = webview_builder.gamepad_delegate(gamepad_delegate);
        }

        let webview = webview_builder.build();
        self.add_webview(webview.clone());

        // If `self` is not in `state.windows`, our notify_accessibility_tree_update() will panic.
        if state.accessibility_active() {
            // Activate accessibility in the WebView.
            // There are two sites like this; this is the WebView creation site.
            webview.set_accessibility_active(true);
        }
        webview
    }

    /// Repaint the active [`WebView`].
    pub(crate) fn repaint_webviews(&self) {
        let Some(webview) = self.active_webview() else {
            return;
        };

        self.platform_window()
            .rendering_context()
            .make_current()
            .expect("Could not make PlatformWindow RenderingContext current");
        webview.paint();
        self.platform_window().rendering_context().present();
    }

    /// Whether or not this [`ServoShellWindow`] has any [`WebView`]s.
    pub(crate) fn should_close(&self) -> bool {
        (self.webview_collection.borrow().is_empty() && self.sleeping_tabs.borrow().is_empty()) ||
            self.close_scheduled.get()
    }

    pub(crate) fn webview_by_id(&self, id: WebViewId) -> Option<WebView> {
        self.webview_collection.borrow().get(id).cloned()
    }

    pub(crate) fn set_needs_update(&self) {
        self.needs_update.set(true);
    }

    pub(crate) fn set_needs_repaint(&self) {
        self.needs_repaint.set(true)
    }

    #[cfg_attr(target_os = "android", expect(dead_code))]
    pub(crate) fn schedule_close(&self) {
        self.close_scheduled.set(true)
    }

    pub(crate) fn platform_window(&self) -> Rc<dyn PlatformWindow> {
        self.platform_window.clone()
    }

    pub(crate) fn add_webview(&self, webview: WebView) {
        webview.notify_theme_change(self.platform_window.theme());
        self.last_active
            .borrow_mut()
            .insert(webview.id(), Instant::now());
        self.webview_collection.borrow_mut().add(webview);
        self.set_needs_update();
        self.set_needs_repaint();
    }

    pub(crate) fn webview_ids(&self) -> Vec<WebViewId> {
        self.webview_collection.borrow().creation_order.clone()
    }

    /// Returns all [`WebView`]s in creation order.
    pub(crate) fn webviews(&self) -> Vec<(WebViewId, WebView)> {
        let mut list: Vec<(WebViewId, WebView)> = self
            .webview_collection
            .borrow()
            .all_in_creation_order()
            .map(|(id, webview)| (id, webview.clone()))
            .collect();
        // Pteron: the order the user dragged the tabs into (new tabs go at the end).
        TAB_ORDER.with(|order| {
            let order = order.borrow();
            if !order.is_empty() {
                list.sort_by_key(|(id, _)| order.iter().position(|x| x == id).unwrap_or(usize::MAX));
            }
        });
        list
    }

    /// Pteron: moves the tab `from` to where the tab `to` is now.
    pub(crate) fn move_tab(&self, from: WebViewId, to: WebViewId) {
        let mut ids: Vec<WebViewId> = self.webviews().into_iter().map(|(id, _)| id).collect();
        let (Some(from_at), Some(to_at)) = (
            ids.iter().position(|id| *id == from),
            ids.iter().position(|id| *id == to),
        ) else {
            return;
        };
        let moved = ids.remove(from_at);
        ids.insert(to_at, moved);
        TAB_ORDER.with(|order| *order.borrow_mut() = ids);
    }

    pub(crate) fn activate_webview(&self, webview_id: WebViewId) {
        // Pteron: remember when the tab we are leaving went to the background.
        let previous = self.webview_collection.borrow().active_id();
        if let Some(previous) = previous &&
            previous != webview_id
        {
            self.last_active
                .borrow_mut()
                .insert(previous, Instant::now());
        }
        self.webview_collection
            .borrow_mut()
            .activate_webview(webview_id);
        self.set_needs_update();
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn activate_webview_by_index(&self, index_to_activate: usize) {
        self.webview_collection
            .borrow_mut()
            .activate_webview_by_index(index_to_activate);
        self.set_needs_update();
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn get_active_webview_index(&self) -> Option<usize> {
        let active_id = self.webview_collection.borrow().active_id()?;
        self.webviews()
            .iter()
            .position(|webview| webview.0 == active_id)
    }

    pub(crate) fn update_and_request_repaint_if_necessary(&self, state: &RunningAppState) {
        let updated_user_interface = self.needs_update.take() &&
            self.platform_window
                .update_user_interface_state(state, self);

        // Delegate handlers may have asked us to present or update painted WebView contents.
        // Currently, egui-file-dialog dialogs need to be constantly redrawn or animations aren't fluid.
        let needs_repaint = self.needs_repaint.take();
        if updated_user_interface || needs_repaint {
            self.platform_window.request_repaint(self);
        }
    }

    /// Close the given [`WebView`] via its [`WebViewId`].
    ///
    /// Note: This can happen because we can trigger a close with a UI action and then get
    /// the close notification via the [`WebViewDelegate`] later.
    pub(crate) fn close_webview(&self, webview_id: WebViewId) {
        let mut webview_collection = self.webview_collection.borrow_mut();
        let Some(closed) = webview_collection.remove(webview_id) else {
            return;
        };
        if let Some(url) = closed.url().filter(|url| matches!(url.scheme(), "http" | "https")) {
            let mut list = self.closed_tabs.borrow_mut();
            list.push(url);
            if list.len() > 20 {
                list.remove(0);
            }
        }
        self.last_active.borrow_mut().remove(&webview_id);
        self.media_playing.borrow_mut().remove(&webview_id);
        self.pinned.borrow_mut().remove(&webview_id);
        self.platform_window
            .dismiss_embedder_controls_for_webview(webview_id);

        self.set_needs_update();
        self.set_needs_repaint();
    }

    /// Pteron: remember whether a tab is playing media (playing tabs never sleep).
    pub(crate) fn note_media_state(&self, webview_id: WebViewId, event: &MediaSessionEvent) {
        if let MediaSessionEvent::PlaybackStateChange(state) = event {
            let mut playing = self.media_playing.borrow_mut();
            if matches!(state, MediaSessionPlaybackState::Playing) {
                playing.insert(webview_id);
            } else {
                playing.remove(&webview_id);
            }
        }
    }

    /// Pteron: the tabs that are currently asleep, in the order they went to sleep.
    pub(crate) fn sleeping_tabs(&self) -> Vec<SleepingTab> {
        self.sleeping_tabs.borrow().clone()
    }

    /// Pteron: wake the sleeping tab at `index` by loading its page in a new `WebView`.
    fn wake_sleeping_tab(self: &Rc<Self>, state: &Rc<RunningAppState>, index: usize) {
        let tab = {
            let mut sleeping = self.sleeping_tabs.borrow_mut();
            if index >= sleeping.len() {
                return;
            }
            sleeping.remove(index)
        };
        let webview = self.create_and_activate_toplevel_webview(
            state.clone(),
            TopLevelWebViewCreationRequest::WithUrl(tab.url),
        );
        if tab.pinned {
            self.pinned.borrow_mut().insert(webview.id());
        }
        self.set_needs_update();
        self.set_needs_repaint();
    }

    /// Pteron: is this tab pinned?
    pub(crate) fn is_pinned(&self, id: WebViewId) -> bool {
        self.pinned.borrow().contains(&id)
    }

    /// Pteron: pin or unpin a live tab.
    pub(crate) fn toggle_pin(&self, id: WebViewId) {
        let mut pinned = self.pinned.borrow_mut();
        if !pinned.remove(&id) {
            pinned.insert(id);
        }
        drop(pinned);
        self.set_needs_update();
    }

    /// Pteron: pin or unpin a sleeping tab.
    pub(crate) fn toggle_pin_sleeping(&self, index: usize) {
        if let Some(tab) = self.sleeping_tabs.borrow_mut().get_mut(index) {
            tab.pinned = !tab.pinned;
        }
        self.set_needs_update();
    }

    /// Pteron: bring back tabs from the last session without loading them.
    pub(crate) fn add_sleeping_tab(&self, tab: SleepingTab) {
        self.sleeping_tabs.borrow_mut().push(tab);
        self.set_needs_update();
    }

    /// Pteron: write the open tabs to the session file (at most every 2 seconds).
    fn save_session(&self) {
        // Give restored tabs a few seconds to start loading before the first save.
        static STARTED: OnceLock<Instant> = OnceLock::new();
        let now = Instant::now();
        if now.duration_since(*STARTED.get_or_init(Instant::now)) < Duration::from_secs(5) {
            return;
        }
        if self
            .last_session_save
            .get()
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(2))
        {
            return;
        }
        self.last_session_save.set(Some(now));
        let active_id = self.webview_collection.borrow().active_id();
        let pinned = self.pinned.borrow();
        let mut tabs: Vec<crate::pteron::SavedTab> = self
            .webviews()
            .into_iter()
            .filter_map(|(id, webview)| {
                let url = webview.url().filter(|url| matches!(url.scheme(), "http" | "https"))?;
                Some(crate::pteron::SavedTab {
                    url: url.to_string(),
                    title: webview.page_title().unwrap_or_default(),
                    pinned: pinned.contains(&id),
                    active: Some(id) == active_id,
                })
            })
            .collect();
        for tab in self.sleeping_tabs.borrow().iter() {
            tabs.push(crate::pteron::SavedTab {
                url: tab.url.to_string(),
                title: tab.title.clone().unwrap_or_default(),
                pinned: tab.pinned,
                active: false,
            });
        }
        // Keep the last good session when nothing worth saving is open.
        if !tabs.is_empty() {
            crate::pteron::save_session(&tabs);
        }
    }

    /// Pteron: runs once per event-loop spin. Wakes a tab if none is left open and puts
    /// long-idle background tabs to sleep.
    fn maintain_tabs(self: &Rc<Self>, state: &Rc<RunningAppState>) {
        crate::memsaver::set_tabs(self.webviews().len());
        self.save_session();
        crate::library::flush();

        // Hand freed memory back to the OS once Servo has finished closing slept tabs.
        {
            let now = Instant::now();
            let mut pending = self.pending_trims.borrow_mut();
            if pending.iter().any(|at| *at <= now) {
                pending.retain(|at| *at > now);
                trim_process_memory();
            }
        }

        // Never leave a window with only sleeping tabs and nothing on screen.
        if self.webview_collection.borrow().is_empty() {
            let last = self.sleeping_tabs.borrow().len().checked_sub(1);
            if let Some(last) = last {
                self.wake_sleeping_tab(state, last);
            }
            return;
        }

        let Some(timeout) = tab_sleep_timeout() else {
            return;
        };
        let active_id = self.webview_collection.borrow().active_id();
        let now = Instant::now();

        let to_sleep: Vec<WebViewId> = {
            let last_active = self.last_active.borrow();
            let playing = self.media_playing.borrow();
            self.webviews()
                .into_iter()
                .filter(|(id, _)| Some(*id) != active_id)
                .filter(|(id, _)| !playing.contains(id))
                .filter(|(id, _)| {
                    last_active
                        .get(id)
                        .is_some_and(|since| now.duration_since(*since) >= timeout)
                })
                .map(|(id, _)| id)
                .collect()
        };

        let mut changed = false;
        for id in to_sleep {
            let Some(webview) = self.webview_by_id(id) else {
                continue;
            };
            // Only ordinary web pages are put to sleep. Internal pages (new tab, about:)
            // are cheap and have no address worth coming back to.
            let Some(url) = webview.url().filter(|url| matches!(url.scheme(), "http" | "https"))
            else {
                continue;
            };
            let title = webview.page_title();
            let pinned = self.pinned.borrow().contains(&id);
            drop(webview);
            self.sleeping_tabs
                .borrow_mut()
                .push(SleepingTab { url, title, pinned });
            // Removing the last handle to the `WebView` closes it and frees its memory.
            self.close_webview(id);
            changed = true;
        }

        if changed {
            let now = Instant::now();
            self.pending_trims.borrow_mut().extend([
                now + Duration::from_secs(5),
                now + Duration::from_secs(15),
            ]);
            self.set_needs_update();
            self.platform_window.request_repaint(self);
        }
    }

    pub(crate) fn notify_favicon_changed(&self, webview: WebView) {
        self.pending_favicon_loads.borrow_mut().push(webview.id());
        self.set_needs_repaint();
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn hidpi_scale_factor_changed(&self) {
        let new_scale_factor = self.platform_window.hidpi_scale_factor();
        for webview in self.webview_collection.borrow().values() {
            webview.set_hidpi_scale_factor(new_scale_factor);
        }
    }

    pub(crate) fn active_webview(&self) -> Option<WebView> {
        self.webview_collection.borrow().active().cloned()
    }

    #[cfg_attr(
        not(any(target_os = "android", target_env = "ohos")),
        expect(dead_code)
    )]
    pub(crate) fn active_or_newest_webview(&self) -> Option<WebView> {
        let webview_collection = self.webview_collection.borrow();
        webview_collection
            .active()
            .or(webview_collection.newest())
            .cloned()
    }

    /// Return a list of all webviews that have favicons that have not yet been loaded by egui.
    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn take_pending_favicon_loads(&self) -> Vec<WebViewId> {
        std::mem::take(&mut *self.pending_favicon_loads.borrow_mut())
    }

    pub(crate) fn show_embedder_control(
        &self,
        webview: WebView,
        embedder_control: EmbedderControl,
    ) {
        self.platform_window
            .show_embedder_control(webview.id(), embedder_control);
        self.set_needs_update();
        self.set_needs_repaint();
    }

    pub(crate) fn hide_embedder_control(
        &self,
        webview: WebView,
        embedder_control: EmbedderControlId,
    ) {
        self.platform_window
            .hide_embedder_control(webview.id(), embedder_control);
        self.set_needs_update();
        self.set_needs_repaint();
    }

    pub(crate) fn queue_user_interface_command(&self, command: UserInterfaceCommand) {
        self.pending_commands.borrow_mut().push(command)
    }

    /// Takes any events generated during UI updates and performs their actions.
    pub(crate) fn handle_interface_commands(
        self: &Rc<Self>,
        state: &Rc<RunningAppState>,
        create_platform_window: Option<&dyn Fn(Url) -> Rc<dyn PlatformWindow>>,
    ) {
        let commands = std::mem::take(&mut *self.pending_commands.borrow_mut());
        for event in commands {
            match event {
                UserInterfaceCommand::Go(location) => {
                    self.set_needs_update();
                    let Some(url) = location_bar_input_to_url(
                        &location.clone(),
                        &crate::pteron::search_url(),
                    ) else {
                        warn!("failed to parse location");
                        break;
                    };
                    if let Some(active_webview) = self.active_webview() {
                        active_webview.load(url.into_url());
                    }
                },
                UserInterfaceCommand::Back => {
                    if let Some(active_webview) = self.active_webview() {
                        active_webview.go_back(1);
                    }
                },
                UserInterfaceCommand::Forward => {
                    if let Some(active_webview) = self.active_webview() {
                        active_webview.go_forward(1);
                    }
                },
                UserInterfaceCommand::Reload => {
                    self.set_needs_update();
                    if let Some(active_webview) = self.active_webview() {
                        active_webview.reload();
                    }
                },
                UserInterfaceCommand::ReloadAll => {
                    for window in state.windows().iter() {
                        window.set_needs_update();
                        for (_, webview) in window.webviews() {
                            webview.reload();
                        }
                    }
                },
                UserInterfaceCommand::NewWebView => {
                    self.set_needs_update();
                    let url = Url::parse("servo:newtab").expect("Should always be able to parse");
                    self.create_and_activate_toplevel_webview(
                        state.clone(),
                        TopLevelWebViewCreationRequest::WithUrl(url),
                    );
                },
                UserInterfaceCommand::CloseWebView(id) => {
                    self.set_needs_update();
                    self.close_webview(id);
                },
                UserInterfaceCommand::NewTabWithUrl(url) => {
                    self.set_needs_update();
                    self.create_and_activate_toplevel_webview(
                        state.clone(),
                        TopLevelWebViewCreationRequest::WithUrl(url),
                    );
                },
                UserInterfaceCommand::ReopenClosedTab => {
                    let url = self.closed_tabs.borrow_mut().pop();
                    if let Some(url) = url {
                        self.set_needs_update();
                        self.create_and_activate_toplevel_webview(
                            state.clone(),
                            TopLevelWebViewCreationRequest::WithUrl(url),
                        );
                    }
                },
                UserInterfaceCommand::TogglePin(id) => self.toggle_pin(id),
                UserInterfaceCommand::TogglePinSleeping(index) => {
                    self.toggle_pin_sleeping(index)
                },
                UserInterfaceCommand::WakeTab(index) => {
                    self.wake_sleeping_tab(state, index);
                },
                UserInterfaceCommand::CloseSleepingTab(index) => {
                    let mut sleeping = self.sleeping_tabs.borrow_mut();
                    if index < sleeping.len() {
                        sleeping.remove(index);
                    }
                    drop(sleeping);
                    self.set_needs_update();
                },
                UserInterfaceCommand::NewWindow(creation_request) => {
                    let Some(create_platform_window) = create_platform_window else {
                        continue;
                    };
                    let url = match &creation_request {
                        TopLevelWebViewCreationRequest::WithUrl(url) => url.clone(),
                        TopLevelWebViewCreationRequest::WithCreateRequest(..) => {
                            Url::parse("servo:newtab").unwrap()
                        },
                    };
                    let platform_window = create_platform_window(url);
                    state.open_window(platform_window, creation_request);
                },
            }
        }

        self.maintain_tabs(state);
    }
}

/// A `PlatformWindow` abstracts away the differents kinds of platform windows that might
/// be used in a servoshell execution. This currently includes headed (winit) and headless
/// windows.
pub(crate) trait PlatformWindow {
    /// Whether or not the platform manages focus. This is used to decide whether
    /// servoshell should focus another window when one closes.
    fn platform_manages_focus(&self) -> bool {
        false
    }

    fn id(&self) -> ServoShellWindowId;
    fn screen_geometry(&self) -> ScreenGeometry;
    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    fn device_hidpi_scale_factor(&self) -> Scale<f32, DeviceIndependentPixel, DevicePixel>;
    fn hidpi_scale_factor(&self) -> Scale<f32, DeviceIndependentPixel, DevicePixel>;
    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    fn get_fullscreen(&self) -> bool;
    /// Inform the `Window` that the state of a `WebView` has changed and that it should
    /// do an incremental update of user interface state. Returns `true` if the user
    /// interface actually changed and a rebuild  and repaint is needed, `false` otherwise.
    fn update_user_interface_state(&self, _: &RunningAppState, _: &ServoShellWindow) -> bool {
        false
    }
    /// Request that the window redraw itself. It is up to the window to do this
    /// once the windowing system is ready. If this is a headless window, the redraw
    /// will happen immediately.
    fn request_repaint(&self, _: &ServoShellWindow);
    /// Request a new outer size for the window, including external decorations.
    /// This should be the same as `window.outerWidth` and `window.outerHeight``
    fn request_resize(&self, webview: &WebView, outer_size: DeviceIntSize)
    -> Option<DeviceIntSize>;
    fn set_position(&self, _point: DeviceIntPoint) {}
    fn set_fullscreen(&self, _state: bool) {}
    fn set_cursor(&self, _cursor: Cursor) {}
    #[cfg(all(
        feature = "webxr",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    fn new_glwindow(
        &self,
        event_loop: &winit::event_loop::ActiveEventLoop,
    ) -> Rc<dyn servo::webxr::GlWindow>;
    /// This returns [`RenderingContext`] matching the viewport.
    fn rendering_context(&self) -> Rc<dyn RenderingContext>;
    fn theme(&self) -> servo::Theme {
        servo::Theme::Light
    }
    fn window_rect(&self) -> DeviceIndependentIntRect;
    fn maximize(&self, _: &WebView) {}
    fn has_platform_focus(&self) -> bool {
        true
    }

    fn show_embedder_control(&self, _: WebViewId, _: EmbedderControl) {}
    fn hide_embedder_control(&self, _: WebViewId, _: EmbedderControlId) {}
    fn dismiss_embedder_controls_for_webview(&self, _: WebViewId) {}
    fn show_bluetooth_device_dialog(
        &self,
        _: WebViewId,
        _request: BluetoothDeviceSelectionRequest,
    ) {
    }
    fn show_permission_dialog(&self, _: WebViewId, _: PermissionRequest) {}
    fn show_http_authentication_dialog(&self, _: WebViewId, _: AuthenticationRequest) {}

    fn notify_input_event_handled(
        &self,
        _webview: &WebView,
        _id: InputEventId,
        _result: InputEventResult,
    ) {
    }

    fn notify_media_session_event(&self, _: MediaSessionEvent) {}
    fn notify_crashed(&self, _: WebView, _reason: String, _backtrace: Option<String>) {}
    fn show_console_message(&self, _level: ConsoleLogLevel, _message: &str) {}

    #[cfg(not(any(target_os = "android", target_env = "ohos")))]
    /// If this window is a headed window, access the concrete type.
    fn as_headed_window(&self) -> Option<&crate::desktop::headed_window::HeadedWindow> {
        None
    }

    #[cfg(any(target_os = "android", target_env = "ohos"))]
    /// If this window is a headed window, access the concrete type.
    fn as_headed_window(&self) -> Option<&crate::egl::app::EmbeddedPlatformWindow> {
        None
    }

    fn notify_accessibility_tree_update(&self, _: WebView, _: accesskit::TreeUpdate) {}
}
