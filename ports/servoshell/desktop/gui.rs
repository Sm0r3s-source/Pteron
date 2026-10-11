/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::HashMap;
#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
use std::fs;
#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use accesskit::Affine;
use dpi::PhysicalSize;
use egui::text::{CCursor, CCursorRange};
use egui::text_edit::TextEditState;
use egui::{
    Button, FontDefinitions, Id, Key, Label, LayerId, Modifiers, Order, PaintCallback, Panel, Vec2,
    WidgetInfo, WidgetType, pos2,
};
#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
use egui::{FontData, FontFamily};
use egui_glow::{CallbackFn, EguiGlow};
use egui_winit::EventResponse;
use euclid::{Length, Point2D, Rect, Scale, Size2D};
#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
use log::info;
use log::warn;
use servo::{
    DeviceIndependentPixel, DevicePixel, Image, LoadStatus, OffscreenRenderingContext, PixelFormat,
    RenderingContext, WebView, WebViewId,
};
use url::Url;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::Window;

use crate::desktop::event_loop::AppEvent;
use crate::desktop::{headed_window, settings_page, sidebar};
use crate::library;
use crate::pteron;
use crate::running_app_state::{RunningAppState, UserInterfaceCommand};
use crate::window::{ServoShellWindow, TopLevelWebViewCreationRequest};

/// The user interface of a headed servoshell. Currently this is implemented via
/// egui.
pub struct Gui {
    rendering_context: Rc<OffscreenRenderingContext>,
    context: EguiGlow,
    toolbar_height: Length<f32, DeviceIndependentPixel>,

    /// Pteron: Zen-style sidebar instead of the classic top toolbar.
    sidebar_mode: bool,
    sidebar: sidebar::State,
    settings: pteron::Settings,
    settings_page: settings_page::PageState,
    /// The tab that was active last frame; switching tabs closes the settings page.
    last_active_tab: Option<WebViewId>,

    location: String,

    /// Whether the location has been edited by the user without clicking Go.
    location_dirty: bool,

    /// The [`LoadStatus`] of the active `WebView`.
    load_status: LoadStatus,

    /// The text to display in the status bar on the bottom of the window.
    status_text: Option<String>,

    /// Whether or not the current `WebView` can navigate backward.
    can_go_back: bool,

    /// Whether or not the current `WebView` can navigate forward.
    can_go_forward: bool,

    /// Handle to the GPU texture of the favicon.
    ///
    /// These need to be cached across egui draw calls.
    favicon_textures: HashMap<WebViewId, (egui::TextureHandle, egui::load::SizedTexture)>,

    /// AccessKit tree updates pending the next egui tick.
    /// This allows us to ensure that graft nodes are sent before the subtrees they graft.
    pending_accesskit_updates: Vec<accesskit::TreeUpdate>,
}

fn truncate_with_ellipsis(input: &str, max_length: usize) -> String {
    if input.chars().count() > max_length {
        let truncated: String = input.chars().take(max_length.saturating_sub(1)).collect();
        format!("{}…", truncated)
    } else {
        input.to_string()
    }
}

#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "macos"
))]
fn load_cjk_fonts(font_candidates: &[(&str, &str)]) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let mut loaded_font_names = Vec::new();

    for (path_str, font_name) in font_candidates.iter() {
        let font_path = Path::new(path_str);
        if font_path.exists() {
            match fs::read(font_path) {
                Ok(bytes) => {
                    if !fonts.font_data.contains_key(*font_name) {
                        fonts
                            .font_data
                            .insert(font_name.to_string(), Arc::new(FontData::from_owned(bytes)));
                        loaded_font_names.push(font_name.to_string());
                        info!("Loaded font: {}", font_name);
                    }
                },
                Err(error) => {
                    info!("Failed to read font {}: {}", font_name, error);
                },
            }
        }
    }

    if !loaded_font_names.is_empty() {
        let proportional = fonts.families.get_mut(&FontFamily::Proportional).unwrap();
        for font_name in loaded_font_names.iter() {
            proportional.insert(0, font_name.clone());
        }
    }

    fonts
}

#[cfg(target_os = "windows")]
fn configure_fonts() -> FontDefinitions {
    load_cjk_fonts(&[
        (r"C:\Windows\Fonts\malgun.ttf", "Malgun Gothic"), // Korean
        (r"C:\Windows\Fonts\msyh.ttc", "Microsoft YaHei"), // Chinese + Japanese
    ])
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn configure_fonts() -> FontDefinitions {
    load_cjk_fonts(&[
        (
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "Noto Sans CJK",
        ), // Ubuntu/Debian
        (
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "Noto Sans CJK",
        ), // Fedora/Arch
        // FreeBSD splits the Noto CJK fonts into regional subsets
        (
            "/usr/local/share/fonts/noto/NotoSansCJKhk-Regular.otf",
            "Noto Sans CJK HK",
        ),
        (
            "/usr/local/share/fonts/noto/NotoSansCJKjp-Regular.otf",
            "Noto Sans CJK JP",
        ),
        (
            "/usr/local/share/fonts/noto/NotoSansCJKkr-Regular.otf",
            "Noto Sans CJK KR",
        ),
        (
            "/usr/local/share/fonts/noto/NotoSansCJKsc-Regular.otf",
            "Noto Sans CJK SC",
        ),
        (
            "/usr/local/share/fonts/noto/NotoSansCJKtc-Regular.otf",
            "Noto Sans CJK TC",
        ),
        (
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "WenQuanYi Micro Hei",
        ), // common fallback
        (
            "/usr/local/share/fonts/wqy/wqy-microhei.ttc",
            "WenQuanYi Micro Hei",
        ), // FreeBSD
    ])
}

#[cfg(target_os = "macos")]
fn configure_fonts() -> FontDefinitions {
    load_cjk_fonts(&[
        (
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            "Apple SD Gothic Neo",
        ), // Korean
        ("/System/Library/Fonts/STHeiti Medium.ttc", "STHeiti Medium"), // Chinese + Japanese
    ])
}

impl Drop for Gui {
    fn drop(&mut self) {
        self.rendering_context
            .make_current()
            .expect("Could not make window RenderingContext current");
        self.context.destroy();
    }
}

impl Gui {
    pub(crate) fn new(
        winit_window: &Window,
        event_loop: &ActiveEventLoop,
        event_loop_proxy: EventLoopProxy<AppEvent>,
        rendering_context: Rc<OffscreenRenderingContext>,
        initial_url: Url,
    ) -> Self {
        rendering_context
            .make_current()
            .expect("Could not make window RenderingContext current");
        let mut context = EguiGlow::new(
            event_loop,
            rendering_context.glow_gl_api(),
            None,
            None,
            false,
        );

        let font_definitions = configure_fonts();
        context.egui_ctx.set_fonts(font_definitions);

        context
            .egui_winit
            .init_accesskit(event_loop, winit_window, event_loop_proxy);
        winit_window.set_visible(true);

        context.egui_ctx.options_mut(|options| {
            // Disable the builtin egui handlers for the Ctrl+Plus, Ctrl+Minus and Ctrl+0
            // shortcuts as they don't work well with servoshell's `device-pixel-ratio` CLI argument.
            options.zoom_with_keyboard = false;

            // On platforms where winit fails to obtain a system theme, fall back to a light theme
            // since it is the more common default.
            options.fallback_theme = egui::Theme::Light;
        });
        apply_theme(&context.egui_ctx, &pteron::settings());

        Self {
            rendering_context,
            context,
            toolbar_height: Default::default(),
            sidebar_mode: std::env::var("PTERON_UI").map_or(true, |v| v != "classic"),
            sidebar: Default::default(),
            settings: pteron::settings(),
            settings_page: Default::default(),
            last_active_tab: None,
            location: initial_url.to_string(),
            location_dirty: false,
            load_status: LoadStatus::Complete,
            status_text: None,
            can_go_back: false,
            can_go_forward: false,
            favicon_textures: Default::default(),
            pending_accesskit_updates: vec![],
        }
    }

    pub(crate) fn has_keyboard_focus(&self) -> bool {
        self.context
            .egui_ctx
            .memory(|memory| memory.focused().is_some())
    }

    pub(crate) fn surrender_focus(&self) {
        self.context.egui_ctx.memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
    }

    pub(crate) fn on_window_event(
        &mut self,
        winit_window: &Window,
        event: &WindowEvent,
    ) -> EventResponse {
        self.context.on_window_event(winit_window, event)
    }

    /// The height of the top toolbar of this user inteface ie the distance from the top of the
    /// window to the position of the `WebView`.
    pub(crate) fn toolbar_height(&self) -> Length<f32, DeviceIndependentPixel> {
        self.toolbar_height
    }

    /// Return true iff the given position is over the egui toolbar.
    pub(crate) fn is_in_egui_toolbar_rect(
        &self,
        position: Point2D<f32, DeviceIndependentPixel>,
    ) -> bool {
        // While the settings page is open it covers the whole page area.
        if self.settings_page.open {
            return true;
        }
        if self.sidebar_mode {
            return position.x < self.sidebar.hit_width;
        }
        position.y < self.toolbar_height.get()
    }

    /// Pteron: true while the settings page is showing.
    pub(crate) fn settings_open(&self) -> bool {
        self.settings_page.open
    }

    /// Pteron: true when the Zen-style sidebar replaces the classic toolbar.
    pub(crate) fn sidebar_mode(&self) -> bool {
        self.sidebar_mode
    }

    /// Pteron: open the sidebar and put the cursor in the address bar.
    pub(crate) fn focus_location(&mut self) {
        self.sidebar.focus_location = true;
    }

    /// Create a frameless button with square sizing, as used in the toolbar.
    fn toolbar_button(text: &str) -> egui::Button<'_> {
        egui::Button::new(text)
            .frame(false)
            .min_size(Vec2 { x: 20.0, y: 20.0 })
    }

    /// Draws a browser tab, checking for clicks and queues appropriate [`UserInterfaceCommand`]s.
    /// Using a custom widget here would've been nice, but it doesn't seem as though egui
    /// supports that, so we arrange multiple Widgets in a way that they look connected.
    fn browser_tab(
        ui: &mut egui::Ui,
        window: &ServoShellWindow,
        webview: WebView,
        favicon_texture: Option<egui::load::SizedTexture>,
    ) {
        let label = match (webview.page_title(), webview.url()) {
            (Some(title), _) if !title.is_empty() => title,
            (_, Some(url)) => url.to_string(),
            _ => "New Tab".into(),
        };

        let inactive_bg_color = ui.visuals().window_fill;
        let active_bg_color = ui.visuals().widgets.active.weak_bg_fill;
        let active = window.active_webview().map(|webview| webview.id()) == Some(webview.id());

        // Setup a tab frame that will contain the favicon, title and close button
        let mut tab_frame = egui::Frame::NONE.corner_radius(4).begin(ui);
        {
            tab_frame.content_ui.add_space(5.0);

            let visuals = tab_frame.content_ui.visuals_mut();
            // Remove the stroke so we don't see the border between the close button and the label
            visuals.widgets.active.bg_stroke.width = 0.0;
            visuals.widgets.hovered.bg_stroke.width = 0.0;
            // Now we make sure the fill color is always the same, irrespective of state, that way
            // we can make sure that both the label and close button have the same background color
            visuals.widgets.noninteractive.weak_bg_fill = inactive_bg_color;
            visuals.widgets.inactive.weak_bg_fill = inactive_bg_color;
            visuals.widgets.hovered.weak_bg_fill = active_bg_color;
            visuals.widgets.active.weak_bg_fill = active_bg_color;
            visuals.selection.bg_fill = active_bg_color;
            visuals.selection.stroke.color = visuals.widgets.active.fg_stroke.color;
            visuals.widgets.hovered.fg_stroke.color = visuals.widgets.active.fg_stroke.color;

            // Expansion would also show that they are 2 separate widgets
            visuals.widgets.active.expansion = 0.0;
            visuals.widgets.hovered.expansion = 0.0;

            if let Some(favicon) = favicon_texture {
                tab_frame.content_ui.add(
                    egui::Image::from_texture(favicon)
                        .fit_to_exact_size(egui::vec2(16.0, 16.0))
                        .bg_fill(egui::Color32::TRANSPARENT),
                );
            }

            let tab = tab_frame
                .content_ui
                .add(Button::selectable(
                    active,
                    truncate_with_ellipsis(&label, 20),
                ))
                .on_hover_ui(|ui| {
                    ui.label(&label);
                });

            let close_button = tab_frame
                .content_ui
                .add(egui::Button::new("X").fill(egui::Color32::TRANSPARENT));
            close_button.widget_info(|| {
                let mut info = WidgetInfo::new(WidgetType::Button);
                info.label = Some("Close".into());
                info
            });
            if close_button.clicked() || close_button.middle_clicked() || tab.middle_clicked() {
                window
                    .queue_user_interface_command(UserInterfaceCommand::CloseWebView(webview.id()));
            } else if !active && tab.clicked() {
                window.activate_webview(webview.id());
            }
        }

        let response = tab_frame.allocate_space(ui);
        let fill_color = if active || response.hovered() {
            active_bg_color
        } else {
            inactive_bg_color
        };
        tab_frame.frame.fill = fill_color;
        tab_frame.end(ui);
    }

    /// Update the user interface, but do not paint the updated state.
    pub(crate) fn update(
        &mut self,
        state: &RunningAppState,
        window: &ServoShellWindow,
        headed_window: &headed_window::HeadedWindow,
    ) {
        self.rendering_context
            .make_current()
            .expect("Could not make RenderingContext current");
        let Self {
            rendering_context,
            context,
            toolbar_height,
            location,
            location_dirty,
            favicon_textures,
            sidebar,
            sidebar_mode,
            settings,
            settings_page,
            last_active_tab,
            ..
        } = self;

        let winit_window = headed_window.winit_window();
        context.run(winit_window, |ctx| {
            load_pending_favicons(ctx, window, favicon_textures);

            // TODO: While in fullscreen add some way to mitigate the increased phishing risk
            // when not displaying the URL bar: https://github.com/servo/servo/issues/32443
            // Show toolbar unless fullscreen is from document (web API)
            if *sidebar_mode && !headed_window.is_fullscreen_from_document() {
                // Pteron: the sidebar floats over the page, so the page keeps the full window.
                *toolbar_height = Length::default();
                sidebar.pinned = settings.sidebar_pinned;
                sidebar::set_left_inset(if sidebar.pinned { sidebar::WIDTH } else { 0.0 });
                sidebar::update_visibility(ctx, sidebar);
                let bookmarks_list = library::bookmarks();
                // The page in the active tab, if it is an ordinary web page.
                let current_page: Option<(String, String)> = window
                    .active_webview()
                    .and_then(|webview| {
                        let url = webview.url()?;
                        matches!(url.scheme(), "http" | "https")
                            .then(|| (url.to_string(), webview.page_title().unwrap_or_default()))
                    });
                let security = window
                    .active_webview()
                    .and_then(|webview| webview.url())
                    .map(|url| match url.scheme() {
                        "https" => sidebar::Security::Secure,
                        "http" => {
                            let local = matches!(
                                url.host_str(),
                                Some("localhost") | Some("127.0.0.1") | Some("[::1]")
                            );
                            if local {
                                sidebar::Security::None
                            } else {
                                sidebar::Security::NotSecure
                            }
                        },
                        _ => sidebar::Security::None,
                    })
                    .unwrap_or_default();
                let model = sidebar::Model {
                    security,
                    tabs: window
                        .webviews()
                        .into_iter()
                        .map(|(id, webview)| sidebar::TabRow {
                            label: match (webview.page_title(), webview.url()) {
                                (Some(title), _) if !title.is_empty() => title,
                                (_, Some(url)) => url.to_string(),
                                _ => "New Tab".into(),
                            },
                            active: window.active_webview().map(|active| active.id()) == Some(id),
                            pinned: window.is_pinned(id),
                            favicon: favicon_textures.get(&id).map(|(_, favicon)| *favicon),
                        })
                        .collect(),
                    sleeping: window
                        .sleeping_tabs()
                        .into_iter()
                        .map(|tab| sidebar::SleepingRow {
                            label: match tab.title.filter(|title| !title.is_empty()) {
                                Some(title) => title,
                                None => tab.url.to_string(),
                            },
                            pinned: tab.pinned,
                        })
                        .collect(),
                    can_go_back: self.can_go_back,
                    can_go_forward: self.can_go_forward,
                    loading: !matches!(self.load_status, LoadStatus::Complete),
                    experimental: state.experimental_preferences_enabled(),
                    bookmarks: bookmarks_list
                        .iter()
                        .map(|bookmark| sidebar::BookmarkRow {
                            label: if bookmark.title.is_empty() {
                                bookmark.url.clone()
                            } else {
                                bookmark.title.clone()
                            },
                            url: bookmark.url.clone(),
                            folder: bookmark.folder.clone(),
                        })
                        .collect(),
                    bookmarked: current_page
                        .as_ref()
                        .is_some_and(|(url, _)| library::is_bookmarked(url)),
                    can_bookmark: current_page.is_some(),
                    suggestions: if *location_dirty &&
                        sidebar::location_focused(ctx) &&
                        !location.trim().is_empty()
                    {
                        let mut list = library::suggest(location, 6);
                        if let Some((title, url)) = pteron::keyword_hint(location) {
                            list.insert(
                                0,
                                library::Suggestion { url, title, bookmarked: false },
                            );
                            list.truncate(6);
                        }
                        list
                    } else {
                        Vec::new()
                    },
                };
                let height = ctx.available_rect_before_wrap().height();
                let actions = sidebar::show(ctx, sidebar, &model, location, location_dirty, height);
                let ids: Vec<WebViewId> = window.webviews().into_iter().map(|(id, _)| id).collect();
                for action in actions {
                    // Any tab or navigation action leaves the settings page.
                    if !matches!(
                        action,
                        sidebar::Action::ToggleSettings | sidebar::Action::SetExperimental(_)
                    ) {
                        settings_page.open = false;
                    }
                    match action {
                        sidebar::Action::Back => {
                            window.queue_user_interface_command(UserInterfaceCommand::Back)
                        },
                        sidebar::Action::Forward => {
                            window.queue_user_interface_command(UserInterfaceCommand::Forward)
                        },
                        sidebar::Action::Reload => {
                            window.queue_user_interface_command(UserInterfaceCommand::Reload)
                        },
                        sidebar::Action::Go(text) => {
                            window.queue_user_interface_command(UserInterfaceCommand::Go(text))
                        },
                        sidebar::Action::NewTab => {
                            window.queue_user_interface_command(UserInterfaceCommand::NewWebView)
                        },
                        sidebar::Action::NewWindow => {
                            let url = Url::parse("servo:newtab")
                                .expect("Should be able to unconditionally parse 'servo:newtab' as URL");
                            window.queue_user_interface_command(UserInterfaceCommand::NewWindow(
                                TopLevelWebViewCreationRequest::WithUrl(url),
                            ));
                        },
                        sidebar::Action::Activate(index) => {
                            if let Some(id) = ids.get(index) {
                                window.activate_webview(*id);
                            }
                        },
                        sidebar::Action::Close(index) => {
                            if let Some(id) = ids.get(index) {
                                window.queue_user_interface_command(
                                    UserInterfaceCommand::CloseWebView(*id),
                                );
                            }
                        },
                        sidebar::Action::Wake(index) => {
                            window.queue_user_interface_command(UserInterfaceCommand::WakeTab(index))
                        },
                        sidebar::Action::CloseSleeping(index) => window
                            .queue_user_interface_command(UserInterfaceCommand::CloseSleepingTab(
                                index,
                            )),
                        sidebar::Action::ReaderMode => {
                            if let Some(webview) = window.active_webview() {
                                webview.evaluate_javascript(
                                    crate::reader::script(&pteron::settings()),
                                    |value| {
                                        if let Ok(servo::JSValue::String(text)) = value {
                                            if text == "none" {
                                                crate::reader::set_message(
                                                    "Reading mode: no article found on this page.",
                                                );
                                            }
                                        }
                                    },
                                );
                            }
                        },
                        sidebar::Action::ToggleBookmark => {
                            if let Some((url, title)) = &current_page {
                                library::toggle_bookmark(url, title);
                            }
                        },
                        sidebar::Action::OpenBookmark(index) => {
                            if let Some(bookmark) = bookmarks_list.get(index) {
                                window.queue_user_interface_command(UserInterfaceCommand::Go(
                                    bookmark.url.clone(),
                                ));
                            }
                        },
                        sidebar::Action::OpenBookmarkInNewTab(index) => {
                            if let Some(url) = bookmarks_list
                                .get(index)
                                .and_then(|bookmark| Url::parse(&bookmark.url).ok())
                            {
                                window.queue_user_interface_command(
                                    UserInterfaceCommand::NewTabWithUrl(url),
                                );
                            }
                        },
                        sidebar::Action::RemoveBookmark(index) => library::remove_bookmark(index),
                        sidebar::Action::MoveBookmark(index, folder) => {
                            library::set_bookmark_folder(index, &folder)
                        },
                        sidebar::Action::MoveTab { from, to } => {
                            if let (Some(from), Some(to)) = (ids.get(from), ids.get(to)) {
                                window.move_tab(*from, *to);
                            }
                        },
                        sidebar::Action::TogglePin(index) => {
                            if let Some(id) = ids.get(index) {
                                window.queue_user_interface_command(
                                    UserInterfaceCommand::TogglePin(*id),
                                );
                            }
                        },
                        sidebar::Action::TogglePinSleeping(index) => window
                            .queue_user_interface_command(
                                UserInterfaceCommand::TogglePinSleeping(index),
                            ),
                        sidebar::Action::ToggleSettings => {
                            settings_page.open = !settings_page.open;
                        },
                        sidebar::Action::SetExperimental(enabled) => {
                            state.set_experimental_preferences_enabled(enabled);
                            window.queue_user_interface_command(UserInterfaceCommand::ReloadAll);
                        },
                    }
                }
            } else if !headed_window.is_fullscreen_from_document() {
                sidebar::set_left_inset(0.0);
                let frame = egui::Frame::default()
                    .fill(ctx.style().visuals.window_fill)
                    .inner_margin(4.0);
                Panel::top("toolbar").frame(frame).show_inside(ctx, |ui| {
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            let back_button =
                                ui.add_enabled(self.can_go_back, Gui::toolbar_button("⏴"));
                            back_button.widget_info(|| {
                                let mut info = WidgetInfo::new(WidgetType::Button);
                                info.label = Some("Back".into());
                                info
                            });
                            if back_button.clicked() {
                                *location_dirty = false;
                                window.queue_user_interface_command(UserInterfaceCommand::Back);
                            }

                            let forward_button =
                                ui.add_enabled(self.can_go_forward, Gui::toolbar_button("⏵"));
                            forward_button.widget_info(|| {
                                let mut info = WidgetInfo::new(WidgetType::Button);
                                info.label = Some("Forward".into());
                                info
                            });
                            if forward_button.clicked() {
                                *location_dirty = false;
                                window.queue_user_interface_command(UserInterfaceCommand::Forward);
                            }

                            match self.load_status {
                                LoadStatus::Started | LoadStatus::HeadParsed => {
                                    let stop_button = ui.add(Gui::toolbar_button("X"));
                                    stop_button.widget_info(|| {
                                        let mut info = WidgetInfo::new(WidgetType::Button);
                                        info.label = Some("Stop".into());
                                        info
                                    });
                                    if stop_button.clicked() {
                                        warn!("Do not support stop yet.");
                                    }
                                },
                                LoadStatus::Complete => {
                                    let reload_button = ui.add(Gui::toolbar_button("↻"));
                                    reload_button.widget_info(|| {
                                        let mut info = WidgetInfo::new(WidgetType::Button);
                                        info.label = Some("Reload".into());
                                        info
                                    });
                                    if reload_button.clicked() {
                                        *location_dirty = false;
                                        window.queue_user_interface_command(
                                            UserInterfaceCommand::Reload,
                                        );
                                    }
                                },
                            }
                            ui.add_space(2.0);

                            ui.allocate_ui_with_layout(
                                ui.available_size(),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let mut experimental_preferences_enabled =
                                        state.experimental_preferences_enabled();
                                    let prefs_toggle = ui
                                        .toggle_value(&mut experimental_preferences_enabled, "☢")
                                        .on_hover_text("Enable experimental prefs");
                                    prefs_toggle.widget_info(|| {
                                        let mut info = WidgetInfo::new(WidgetType::Button);
                                        info.label = Some("Enable experimental preferences".into());
                                        info.selected = Some(experimental_preferences_enabled);
                                        info
                                    });
                                    if prefs_toggle.clicked() {
                                        state.set_experimental_preferences_enabled(
                                            experimental_preferences_enabled,
                                        );
                                        *location_dirty = false;
                                        window.queue_user_interface_command(
                                            UserInterfaceCommand::ReloadAll,
                                        );
                                    }

                                    let location_id = egui::Id::new("location_input");
                                    let location_field = ui.add_sized(
                                        ui.available_size(),
                                        egui::TextEdit::singleline(location)
                                            .id(location_id)
                                            .hint_text("Search or enter address"),
                                    );

                                    if location_field.changed() {
                                        *location_dirty = true;
                                    }
                                    // Handle adddress bar shortcut.
                                    if ui.input(|i| {
                                        if cfg!(target_os = "macos") {
                                            i.clone().consume_key(Modifiers::COMMAND, Key::L)
                                        } else {
                                            i.clone().consume_key(Modifiers::COMMAND, Key::L) ||
                                                i.clone().consume_key(Modifiers::ALT, Key::D)
                                        }
                                    }) {
                                        // The focus request immediately makes gained_focus return true.
                                        location_field.request_focus();
                                    }
                                    // Select address bar text when it's focused (click or shortcut).
                                    if location_field.gained_focus() &&
                                        let Some(mut state) =
                                            TextEditState::load(ui.ctx(), location_id)
                                    {
                                        // Select the whole input.
                                        state.cursor.set_char_range(Some(CCursorRange::two(
                                            CCursor::new(0),
                                            CCursor::new(location.len()),
                                        )));
                                        state.store(ui.ctx(), location_id);
                                    }
                                    // Navigate to address when enter is pressed in the address bar.
                                    if location_field.lost_focus() &&
                                        ui.input(|i| i.clone().key_pressed(Key::Enter))
                                    {
                                        window.queue_user_interface_command(
                                            UserInterfaceCommand::Go(location.clone()),
                                        );
                                    }
                                },
                            );
                        },
                    );
                });

                // A simple Tab header strip
                let outer = Panel::top("tabs").show_inside(ctx, |ui| {
                    // Add scroll for overflowing tabs
                    egui::ScrollArea::horizontal()
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .show(ui, |ui| {
                            ui.allocate_ui_with_layout(
                                ui.available_size(),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    for (id, webview) in window.webviews().into_iter() {
                                        let favicon = favicon_textures
                                            .get(&id)
                                            .map(|(_, favicon)| favicon)
                                            .copied();
                                        Self::browser_tab(ui, window, webview, favicon);
                                    }

                                    // Pteron: tabs that were put to sleep to save memory.
                                    for (index, tab) in
                                        window.sleeping_tabs().into_iter().enumerate()
                                    {
                                        let label = match tab.title.filter(|title| !title.is_empty()) {
                                            Some(title) => title,
                                            None => tab.url.to_string(),
                                        };
                                        let text = egui::RichText::new(format!(
                                            "zZ {}",
                                            truncate_with_ellipsis(&label, 20)
                                        ))
                                        .weak();
                                        let sleeping_tab = ui
                                            .add(egui::Button::new(text).frame(false))
                                            .on_hover_text(format!(
                                                "Sleeping tab (click to wake, middle-click to close)\n{}",
                                                tab.url
                                            ));
                                        if sleeping_tab.middle_clicked() {
                                            window.queue_user_interface_command(
                                                UserInterfaceCommand::CloseSleepingTab(index),
                                            );
                                        } else if sleeping_tab.clicked() {
                                            window.queue_user_interface_command(
                                                UserInterfaceCommand::WakeTab(index),
                                            );
                                        }
                                    }

                                    let new_tab_button = ui.add(Gui::toolbar_button("+"));
                                    new_tab_button.widget_info(|| {
                                        let mut info = WidgetInfo::new(WidgetType::Button);
                                        info.label = Some("New tab".into());
                                        info
                                    });
                                    if new_tab_button.clicked() {
                                        window.queue_user_interface_command(
                                            UserInterfaceCommand::NewWebView,
                                        );
                                    }

                                    let new_window_button = ui.add(Gui::toolbar_button("⊞"));
                                    new_window_button.widget_info(|| {
                                        let mut info = WidgetInfo::new(WidgetType::Button);
                                        info.label = Some("New window".into());
                                        info
                                    });
                                    if new_window_button.clicked() {
                                        let url = Url::parse("servo:newtab").expect("Should be able to unconditionally parse 'servo:newtab' as URL");
                                        window.queue_user_interface_command(
                                            UserInterfaceCommand::NewWindow(TopLevelWebViewCreationRequest::WithUrl(url)),
                                        );
                                    }
                                },
                            );
                        })
                });

                *toolbar_height = Length::new(outer.response.rect.max.y);
            } else {
                sidebar::set_left_inset(0.0);
                *toolbar_height = Length::default();
            }

            // Pteron: full-window settings page.
            let active_tab = window.active_webview().map(|webview| webview.id());
            if active_tab != *last_active_tab {
                settings_page.open = false;
                *last_active_tab = active_tab;
            }
            // (A pinned sidebar is a panel, so the page area already starts next to it.)
            let page_area = ctx.available_rect_before_wrap();
            if settings_page::show(ctx, settings_page, settings, page_area) {
                apply_theme(ctx, settings);
                pteron::update(settings.clone());
            }
            if let Some(result) = crate::sync::take_chosen() {
                settings_page.notice = Some(match result {
                    Ok(folder) => {
                        settings.sync_folder = folder;
                        pteron::update(settings.clone());
                        let message = match crate::sync::join() {
                            Ok(message) => message,
                            Err(message) => message,
                        };
                        // Settings may have come from the other computer.
                        *settings = pteron::settings();
                        apply_theme(ctx, settings);
                        message
                    },
                    Err(message) => message,
                });
            }
            if let Some(result) = crate::appearance::take_picked() {
                settings_page.notice = Some(match result {
                    Ok(()) => {
                        settings.newtab_bg = "picture".to_string();
                        pteron::update(settings.clone());
                        "Picture saved. Open a new tab to see it.".to_string()
                    },
                    Err(message) => message,
                });
            }
            for action in std::mem::take(&mut settings_page.pending) {
                settings_page.notice = Some(match action {
                    settings_page::PageAction::ClearCookies => {
                        let manager = state.servo().site_data_manager();
                        manager.clear_cookies(None);
                        let sites = manager.site_data(servo::StorageType::all());
                        let names: Vec<String> = sites.iter().map(|site| site.name()).collect();
                        let names: Vec<&str> = names.iter().map(String::as_str).collect();
                        manager.clear_site_data(&names, servo::StorageType::all());
                        "Cookies and site data cleared. Reload open pages to log out.".to_string()
                    },
                    settings_page::PageAction::ClearCache => {
                        state.servo().network_manager().clear_cache();
                        "Cache cleared.".to_string()
                    },
                    settings_page::PageAction::ClearHistory => {
                        library::clear_history();
                        "Browsing history cleared.".to_string()
                    },
                    settings_page::PageAction::InstallUpdate => match crate::updater::launch_installer() {
                        Ok(()) => {
                            window.schedule_close();
                            "Updating...".to_string()
                        },
                        Err(message) => message,
                    },
                    settings_page::PageAction::SaveReport(note) => match crate::report::save(&note) {
                        Ok(path) => {
                            crate::report::show_in_folder(&path);
                            settings_page.report_note.clear();
                            format!(
                                "Saved: {}. Send this file to whoever gave you Pteron.",
                                path.display()
                            )
                        },
                        Err(message) => message,
                    },
                    settings_page::PageAction::CopyReport(note) => {
                        ctx.copy_text(crate::report::text(&note));
                        "Copied the details. Paste them into a message.".to_string()
                    },
                    settings_page::PageAction::ExportBookmarks => match library::export_bookmarks() {
                        Ok(path) => {
                            crate::report::show_in_folder(&path);
                            format!("Saved: {}", path.display())
                        },
                        Err(message) => message,
                    },
                    settings_page::PageAction::ImportBookmarks => match library::import_from_downloads() {
                        Ok(0) => "Nothing new: all those bookmarks were already saved.".to_string(),
                        Ok(1) => "Imported 1 bookmark.".to_string(),
                        Ok(count) => format!("Imported {count} bookmarks."),
                        Err(message) => message,
                    },
                    settings_page::PageAction::ChooseSyncFolder => {
                        crate::sync::choose_folder_async();
                        "Choose a folder in the window that opens.".to_string()
                    },
                    settings_page::PageAction::SyncNow => {
                        let message = match crate::sync::sync_now(true) {
                            Ok(message) => message,
                            Err(message) => message,
                        };
                        *settings = pteron::settings();
                        apply_theme(ctx, settings);
                        message
                    },
                    settings_page::PageAction::StopSync => {
                        settings.sync_folder.clear();
                        pteron::update(settings.clone());
                        "Sync is off. The file in the folder was left where it is.".to_string()
                    },
                    settings_page::PageAction::OpenDownloadPage => {
                        if let Ok(url) = Url::parse(&format!("https://github.com/{}/releases/latest", crate::updater::REPO)) {
                            window.queue_user_interface_command(UserInterfaceCommand::NewTabWithUrl(url));
                        }
                        "Opened the download page in a new tab.".to_string()
                    },
                    settings_page::PageAction::ChoosePicture => {
                        crate::appearance::choose_picture_async();
                        "Choose a picture in the window that opens.".to_string()
                    },
                    settings_page::PageAction::OpenKnownIssues => {
                        if let Ok(url) = Url::parse("servo:known-issues") {
                            window.queue_user_interface_command(UserInterfaceCommand::NewTabWithUrl(url));
                        }
                        String::new()
                    },
                    settings_page::PageAction::ForgetSession => {
                        pteron::clear_session();
                        "Saved tabs forgotten (they are saved again from the tabs open now)."
                            .to_string()
                    },
                });
            }

            let scale =
                Scale::<_, DeviceIndependentPixel, DevicePixel>::new(ctx.pixels_per_point());

            headed_window.for_each_active_dialog(window, |dialog| dialog.update(ctx));

            // If the top parts of the GUI changed size, then update the size of the WebView and also
            // the size of its RenderingContext.
            let available_rect = ctx.available_rect_before_wrap();

            // Build a graft node for each WebView.
            let affine = {
                // The grafted WebView tree reports bounds in device pixels relative to the
                // WebView's own origin, so this node supplies the offset of the WebView within
                // the window, and scales it to the same scale as the rest of the nodes in egui's
                // AccessKit tree.
                let scale = (1.0 / window.platform_window().hidpi_scale_factor().get()) as f64;
                let x = available_rect.min.x as f64;
                let y = available_rect.min.y as f64;
                Affine::new([scale, 0.0, 0.0, scale, x, y])
            };
            for (webview_id, webview) in window.webviews() {
                if let Some(tree_id) = webview.accesskit_tree_id() {
                    let id = egui::Id::new(webview_id);
                    ctx.accesskit_node_builder(id, |node| {
                        node.set_tree_id(tree_id);
                        // Only the transform is set: AccessKit consumers exclude graft nodes from
                        // the presented tree, so bounds on this node would never be read.
                        node.set_transform(affine);
                    });
                }
            }
            let size = Size2D::new(available_rect.width(), available_rect.height()) * scale;
            if let Some(webview) = window.active_webview() &&
                size != webview.size()
            {
                // `rect` is sized to just the WebView viewport, which is required by
                // `OffscreenRenderingContext` See:
                // <https://github.com/servo/servo/issues/38369#issuecomment-3138378527>
                webview.resize(PhysicalSize::new(size.width as u32, size.height as u32))
            }

            if let Some(status_text) = &self.status_text {
                egui::Tooltip::always_open(
                    ctx.clone(),
                    LayerId::new(Order::Tooltip, Id::new("tooltip")),
                    "tooltip layer".into(),
                    pos2(0.0, available_rect.max.y),
                )
                .show(|ui| ui.add(Label::new(status_text.clone()).extend()));
            }

            // Pteron: find in page (Ctrl+F).
            Self::show_find_bar(ctx, window);
            Self::show_zoom_hud(ctx);
            Self::show_password_bars(ctx, window);

            // Pteron: short messages from reading mode.
            if let Some(text) = crate::reader::message() {
                egui::Area::new(Id::new("reader_notice"))
                    .order(Order::Tooltip)
                    .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -116.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(text);
                        });
                    });
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }

            // Pteron: tell the user when a pop-up was blocked.
            if let Some(count) = pteron::popup_notice() {
                egui::Area::new(Id::new("popup_blocked_notice"))
                    .order(Order::Tooltip)
                    .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(if count > 1 {
                                format!("Pop-up blocked ({count} so far). Click a link to open windows.")
                            } else {
                                "Pop-up blocked. Click a link to open windows.".to_string()
                            });
                        });
                    });
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }

            // Pteron: download messages and progress.
            if let Some(text) = crate::downloads::notice() {
                egui::Area::new(Id::new("download_notice"))
                    .order(Order::Tooltip)
                    .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -64.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(text);
                        });
                    });
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
            if crate::downloads::any_active() {
                ctx.request_repaint_after(std::time::Duration::from_millis(300));
            }

            window.repaint_webviews();

            if let Some(render_to_parent) = rendering_context.render_to_parent_callback() {
                ctx.layer_painter(LayerId::background()).add(PaintCallback {
                    rect: available_rect,
                    callback: Arc::new(CallbackFn::new(move |info, painter| {
                        let clip = info.viewport_in_pixels();
                        let rect_in_parent = Rect::new(
                            Point2D::new(clip.left_px, clip.from_bottom_px),
                            Size2D::new(clip.width_px, clip.height_px),
                        );
                        render_to_parent(painter.gl(), rect_in_parent)
                    })),
                });
            }
        });

        // If any egui widget requested a repaint, also request a repaint for our
        // containing window. This allows egui widget to animate on their own.
        if self.context.egui_ctx.has_requested_repaint() {
            window.set_needs_repaint();
        }

        let adapter = self
            .context
            .egui_winit
            .accesskit
            .as_mut()
            .expect("guaranteed by Gui::new()");
        for tree_update in self.pending_accesskit_updates.drain(..) {
            adapter.update_if_active(|| tree_update);
        }
    }

    /// Paint the GUI, as of the last update.
    /// Pteron: the find-in-page bar, top right over the page.
    /// Pteron: the "Save password?" and "Fill saved login" bars (bottom right over the page).
    fn show_password_bars(ctx: &egui::Context, window: &ServoShellWindow) {
        use std::time::Duration;
        if crate::passwords::working() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let active = window.active_webview();
        let page = active.as_ref().and_then(|webview| webview.url());
        if let (Some(webview), Some(page)) = (&active, &page) &&
            let Some(script) = crate::passwords::take_fill_script(page)
        {
            webview.evaluate_javascript(script, |_| {});
        }
        let place = |id: &'static str| {
            egui::Area::new(Id::new(id))
                .order(Order::Tooltip)
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -176.0))
        };
        if let Some((host, user, replaces)) = crate::passwords::save_prompt() {
            let (mut save, mut later, mut never) = (false, false, false);
            place("pteron_save_password").show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    let who = if user.is_empty() { host.clone() } else { format!("{user} on {host}") };
                    ui.label(if replaces {
                        format!("Update the saved password for {who}?")
                    } else {
                        format!("Save the password for {who}?")
                    });
                    ui.horizontal(|ui| {
                        save = ui.button(if replaces { "Update" } else { "Save" }).clicked();
                        later = ui.button("Not now").clicked();
                        never = ui.button("Never for this site").clicked();
                    });
                });
            });
            if save {
                crate::passwords::accept_save();
            } else if never {
                crate::passwords::dismiss_save(true);
            } else if later {
                crate::passwords::dismiss_save(false);
            }
            ctx.request_repaint_after(Duration::from_millis(500));
        } else if let Some(page) = &page &&
            let Some((host, users)) = crate::passwords::fill_offer(page)
        {
            let (mut chosen, mut close) = (None, false);
            place("pteron_fill_password").show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(format!("Saved login for {host}"));
                    for (index, user) in users.iter().enumerate().take(5) {
                        let label = if user.is_empty() { "Fill saved login".to_string() } else { format!("Fill as {user}") };
                        if ui.button(label).clicked() {
                            chosen = Some(index);
                        }
                    }
                    close = ui.button("Not now").clicked();
                });
            });
            if let Some(index) = chosen {
                crate::passwords::request_fill(index);
            } else if close {
                crate::passwords::dismiss_offer();
            }
        } else if let Some(text) = crate::passwords::message() {
            place("pteron_password_message").show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(text);
                });
            });
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Pteron: the zoom level, shown for a moment after a zoom shortcut.
    fn show_zoom_hud(ctx: &egui::Context) {
        let Some(percent) = crate::find::zoom_percent() else { return };
        egui::Area::new(Id::new("pteron_zoom_hud"))
            .order(Order::Foreground)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new(format!("{percent}%")).size(16.0).strong());
                });
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    fn show_find_bar(ctx: &egui::Context, window: &ServoShellWindow) {
        use crate::find;
        let active = window.active_webview();
        let tab_id = active.as_ref().map(|webview| format!("{:?}", webview.id()));
        // Another tab was shown: the bar closes.
        find::with(|state| {
            if state.open && state.tab.is_some() && state.tab != tab_id {
                state.open = false;
            }
        });
        let (open, mut query, result, focus) =
            find::with(|s| (s.open, s.query.clone(), s.result.clone(), std::mem::take(&mut s.focus)));
        let Some(webview) = active else { return };
        if !open {
            return;
        }
        find::with(|s| s.tab = tab_id);
        let input_id = Id::new("pteron_find_input");
        let mut direction: Option<i32> = None;
        let mut close = false;
        let mut changed = false;
        egui::Area::new(Id::new("pteron_find_bar"))
            .order(Order::Foreground)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 12.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut query)
                                .id(input_id)
                                .hint_text("Find in page")
                                .desired_width(200.0),
                        );
                        if focus {
                            edit.request_focus();
                        }
                        changed = edit.changed();
                        if edit.has_focus() || edit.lost_focus() {
                            let (enter, shift, escape) = ui.input(|i| {
                                (
                                    i.key_pressed(egui::Key::Enter),
                                    i.modifiers.shift,
                                    i.key_pressed(egui::Key::Escape),
                                )
                            });
                            if enter {
                                direction = Some(if shift { -1 } else { 1 });
                                edit.request_focus();
                            }
                            if escape {
                                close = true;
                            }
                        }
                        let label = if query.is_empty() {
                            String::new()
                        } else if result.is_empty() || result == "0/0" {
                            "No matches".to_string()
                        } else {
                            result.replace('/', " of ")
                        };
                        ui.label(label);
                        if ui.small_button("\u{25B2}").on_hover_text("Previous (Shift+Enter)").clicked() {
                            direction = Some(-1);
                        }
                        if ui.small_button("\u{25BC}").on_hover_text("Next (Enter)").clicked() {
                            direction = Some(1);
                        }
                        if ui.small_button("\u{2715}").on_hover_text("Close (Esc)").clicked() {
                            close = true;
                        }
                    });
                });
            });
        if close {
            // Remove the highlights, then give the keyboard back to the page.
            webview.evaluate_javascript(find::script("", 0), |_| {});
            ctx.memory_mut(|memory| memory.surrender_focus(input_id));
            find::with(|s| {
                s.open = false;
                s.query.clear();
                s.result.clear();
            });
            return;
        }
        let step = if changed { Some(0) } else { direction };
        find::with(|s| s.query = query.clone());
        if let Some(step) = step {
            webview.evaluate_javascript(find::script(&query, step), |value| {
                if let Ok(servo::JSValue::String(text)) = value {
                    find::with(|s| s.result = text);
                }
            });
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    pub(crate) fn paint(&mut self, window: &Window) {
        self.rendering_context
            .make_current()
            .expect("Could not make RenderingContext current");
        self.rendering_context
            .parent_context()
            .prepare_for_rendering();
        self.context.paint(window);
        self.rendering_context.parent_context().present();
    }

    /// Updates the location field from the given [`RunningAppState`], unless the user has started
    /// editing it without clicking Go, returning true iff it has changed (needing an egui update).
    fn update_location_in_toolbar(&mut self, window: &ServoShellWindow) -> bool {
        // User edited without clicking Go?
        if self.location_dirty {
            return false;
        }

        let current_url_string = window
            .active_webview()
            .and_then(|webview| Some(webview.url()?.to_string()))
            // Pteron: the new-tab page shows an empty address bar.
            .map(|url| if url == "servo:newtab" { String::new() } else { url });
        match current_url_string {
            Some(location) if location != self.location => {
                self.location = location;
                true
            },
            _ => false,
        }
    }

    fn update_load_status(&mut self, window: &ServoShellWindow) -> bool {
        let state_status = window
            .active_webview()
            .map(|webview| webview.load_status())
            .unwrap_or(LoadStatus::Complete);
        let old_status = std::mem::replace(&mut self.load_status, state_status);
        let status_changed = old_status != self.load_status;

        // When the load status changes, we want the new changes to the URL to start
        // being reflected in the location bar.
        if status_changed {
            self.location_dirty = false;
        }

        status_changed
    }

    fn update_status_text(&mut self, window: &ServoShellWindow) -> bool {
        let state_status = window
            .active_webview()
            .and_then(|webview| webview.status_text());
        let old_status = std::mem::replace(&mut self.status_text, state_status);
        old_status != self.status_text
    }

    fn update_can_go_back_and_forward(&mut self, window: &ServoShellWindow) -> bool {
        let (can_go_back, can_go_forward) = window
            .active_webview()
            .map(|webview| (webview.can_go_back(), webview.can_go_forward()))
            .unwrap_or((false, false));
        let old_can_go_back = std::mem::replace(&mut self.can_go_back, can_go_back);
        let old_can_go_forward = std::mem::replace(&mut self.can_go_forward, can_go_forward);
        old_can_go_back != self.can_go_back || old_can_go_forward != self.can_go_forward
    }

    /// Updates all fields taken from the given [`ServoShellWindow`], such as the location field.
    /// Returns true iff the egui needs an update.
    pub(crate) fn update_webview_data(&mut self, window: &ServoShellWindow) -> bool {
        // Note: We must use the "bitwise OR" (|) operator here instead of "logical OR" (||)
        //       because logical OR would short-circuit if any of the functions return true.
        //       We want to ensure that all functions are called. The "bitwise OR" operator
        //       does not short-circuit.
        self.update_load_status(window) |
            self.update_location_in_toolbar(window) |
            self.update_status_text(window) |
            self.update_can_go_back_and_forward(window)
    }

    /// Returns true if a redraw is required after handling the provided event.
    pub(crate) fn handle_accesskit_event(
        &mut self,
        event: &egui_winit::accesskit_winit::WindowEvent,
    ) -> bool {
        match event {
            egui_winit::accesskit_winit::WindowEvent::InitialTreeRequested => {
                self.context.egui_ctx.enable_accesskit();
                true
            },
            egui_winit::accesskit_winit::WindowEvent::ActionRequested(req) => {
                self.context
                    .egui_winit
                    .on_accesskit_action_request(req.clone());
                true
            },
            egui_winit::accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                self.context.egui_ctx.disable_accesskit();
                false
            },
        }
    }

    pub(crate) fn set_zoom_factor(&self, factor: f32) {
        self.context.egui_ctx.set_zoom_factor(factor);
    }

    pub(crate) fn notify_accessibility_tree_update(&mut self, tree_update: accesskit::TreeUpdate) {
        self.pending_accesskit_updates.push(tree_update);
    }
}

fn embedder_image_to_egui_image(image: &Image) -> egui::ColorImage {
    let width = image.width as usize;
    let height = image.height as usize;

    match image.format {
        PixelFormat::K8 => egui::ColorImage::from_gray([width, height], image.data()),
        PixelFormat::KA8 => {
            // Convert to rgba
            let data: Vec<u8> = image
                .data()
                .chunks_exact(2)
                .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
                .collect();
            egui::ColorImage::from_rgba_unmultiplied([width, height], &data)
        },
        PixelFormat::RGB8 => egui::ColorImage::from_rgb([width, height], image.data()),
        PixelFormat::RGBA8 => {
            egui::ColorImage::from_rgba_unmultiplied([width, height], image.data())
        },
        PixelFormat::BGRA8 => {
            // Convert from BGRA to RGBA
            let data: Vec<u8> = image
                .data()
                .chunks_exact(4)
                .flat_map(|chunk| [chunk[2], chunk[1], chunk[0], chunk[3]])
                .collect();
            egui::ColorImage::from_rgba_unmultiplied([width, height], &data)
        },
    }
}

/// Uploads all favicons that have not yet been processed to the GPU.
/// Pteron: apply the theme setting ("system", "light" or "dark").
/// egui colours for a full Pteron theme.
fn visuals_for(p: &crate::appearance::Palette) -> egui::Visuals {
    use egui::Color32;
    let c = |rgb: [u8; 3]| Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
    let mixed = |a: [u8; 3], b: [u8; 3], t: f32| {
        let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
        Color32::from_rgb(m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2]))
    };
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let (bg, surface, text, accent) = (c(p.bg), c(p.surface), c(p.text), c(p.accent));
    let line = mixed(p.bg, p.text, 0.16);
    v.panel_fill = bg;
    v.window_fill = bg;
    v.faint_bg_color = surface;
    v.extreme_bg_color = surface;
    v.code_bg_color = surface;
    v.window_stroke = egui::Stroke::new(1.0, line);
    v.selection.bg_fill = accent.gamma_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0, accent);
    v.hyperlink_color = accent;
    v.text_cursor.stroke.color = accent;
    let noninteractive = &mut v.widgets.noninteractive;
    noninteractive.bg_fill = bg;
    noninteractive.weak_bg_fill = bg;
    noninteractive.bg_stroke = egui::Stroke::new(1.0, line);
    noninteractive.fg_stroke = egui::Stroke::new(1.0, text);
    let inactive = &mut v.widgets.inactive;
    inactive.bg_fill = surface;
    inactive.weak_bg_fill = surface;
    inactive.bg_stroke = egui::Stroke::new(1.0, line);
    inactive.fg_stroke = egui::Stroke::new(1.0, text);
    let hovered = &mut v.widgets.hovered;
    hovered.bg_fill = mixed(p.surface, p.accent, 0.20);
    hovered.weak_bg_fill = mixed(p.surface, p.accent, 0.20);
    hovered.bg_stroke = egui::Stroke::new(1.0, accent.gamma_multiply(0.7));
    hovered.fg_stroke = egui::Stroke::new(1.5, text);
    let active = &mut v.widgets.active;
    active.bg_fill = mixed(p.surface, p.accent, 0.34);
    active.weak_bg_fill = mixed(p.surface, p.accent, 0.34);
    active.bg_stroke = egui::Stroke::new(1.0, accent);
    active.fg_stroke = egui::Stroke::new(2.0, text);
    let open = &mut v.widgets.open;
    open.bg_fill = mixed(p.surface, p.accent, 0.26);
    open.weak_bg_fill = mixed(p.surface, p.accent, 0.26);
    open.bg_stroke = egui::Stroke::new(1.0, line);
    open.fg_stroke = egui::Stroke::new(1.0, text);
    v
}

fn apply_theme(ctx: &egui::Context, settings: &pteron::Settings) {
    // Start from egui's plain looks every time, so switching away from a colour theme
    // leaves nothing behind.
    ctx.set_visuals_of(egui::Theme::Dark, egui::Visuals::dark());
    ctx.set_visuals_of(egui::Theme::Light, egui::Visuals::light());
    let palette = crate::appearance::palette_for(settings);
    ctx.set_theme(match (&palette, settings.theme.as_str()) {
        (Some(p), _) if p.dark => egui::ThemePreference::Dark,
        (Some(_), _) => egui::ThemePreference::Light,
        (None, "light") => egui::ThemePreference::Light,
        (None, "dark") => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
    });
    if let Some(p) = &palette {
        let theme = if p.dark { egui::Theme::Dark } else { egui::Theme::Light };
        ctx.set_visuals_of(theme, visuals_for(p));
        return;
    }
    // Plain looks keep their own colours; the chosen accent tints selections and links.
    let [r, g, b] = crate::appearance::accent_rgb(settings);
    let accent = egui::Color32::from_rgb(r, g, b);
    ctx.all_styles_mut(|style| {
        style.visuals.selection.bg_fill = accent.gamma_multiply(0.35);
        style.visuals.selection.stroke.color = accent;
        style.visuals.hyperlink_color = accent;
        style.visuals.text_cursor.stroke.color = accent;
        style.visuals.widgets.hovered.bg_stroke.color = accent.gamma_multiply(0.7);
    });
}

fn load_pending_favicons(
    ctx: &egui::Context,
    window: &ServoShellWindow,
    texture_cache: &mut HashMap<WebViewId, (egui::TextureHandle, egui::load::SizedTexture)>,
) {
    for id in window.take_pending_favicon_loads() {
        let Some(webview) = window.webview_by_id(id) else {
            continue;
        };
        let Some(favicon) = webview.favicon() else {
            continue;
        };

        let egui_image = embedder_image_to_egui_image(&favicon);
        let handle = ctx.load_texture(format!("favicon-{id:?}"), egui_image, Default::default());
        let texture = egui::load::SizedTexture::new(
            handle.id(),
            egui::vec2(favicon.width as f32, favicon.height as f32),
        );

        // We don't need the handle anymore but we can't drop it either since that would cause
        // the texture to be freed.
        texture_cache.insert(id, (handle, texture));
    }
}
