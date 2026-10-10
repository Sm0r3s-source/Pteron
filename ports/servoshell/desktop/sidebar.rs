/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: a Zen-style vertical sidebar. It floats over the page (the page is never
//! shifted or resized) and slides in when the pointer touches the left window edge.
//! This module only knows about egui; `gui.rs` turns the returned [`Action`]s into
//! servoshell commands.

use egui::text::{CCursor, CCursorRange};
use egui::text_edit::TextEditState;
use egui::{
    Align2, Area, Button, Color32, Context, FontId, Frame, Id, Key, Modifiers, Order, Pos2, Rect,
    RichText, ScrollArea, Sense, Shape, Stroke, TextEdit, pos2, vec2,
};

/// Width of the open sidebar, in egui points.
pub(crate) const WIDTH: f32 = 260.0;
/// Width of the invisible strip at the left edge that opens the sidebar.
pub(crate) const HOT_ZONE: f32 = 4.0;
const LOCATION_ID: &str = "location_input";
const ROW_HEIGHT: f32 = 28.0;

/// One live tab, as shown in the list.
pub(crate) struct TabRow {
    pub label: String,
    pub active: bool,
    pub pinned: bool,
    pub favicon: Option<egui::load::SizedTexture>,
}

/// One bookmark, as shown in the list.
pub(crate) struct BookmarkRow {
    pub label: String,
    pub url: String,
    pub folder: String,
}

/// One sleeping tab, as shown in the list.
pub(crate) struct SleepingRow {
    pub label: String,
    pub pinned: bool,
}

/// Everything the sidebar needs to draw itself.
/// What the address bar says about the connection of the current page.
#[derive(Clone, Copy, Default, PartialEq)]
pub(crate) enum Security {
    /// Internal pages, new tab, local files: nothing to say.
    #[default]
    None,
    /// https
    Secure,
    /// plain http
    NotSecure,
}

pub(crate) struct Model {
    pub security: Security,
    pub tabs: Vec<TabRow>,
    /// Sleeping tabs.
    pub sleeping: Vec<SleepingRow>,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub loading: bool,
    pub experimental: bool,
    pub bookmarks: Vec<BookmarkRow>,
    /// Whether the page in the active tab is bookmarked, and whether it can be at all.
    pub bookmarked: bool,
    pub can_bookmark: bool,
    /// Pages matching what is being typed in the address bar.
    pub suggestions: Vec<crate::library::Suggestion>,
}

/// What the user did in the sidebar this frame.
pub(crate) enum Action {
    Back,
    Forward,
    Reload,
    Go(String),
    NewTab,
    NewWindow,
    Activate(usize),
    Close(usize),
    /// Drag a tab: move tab `from` to where tab `to` is.
    MoveTab { from: usize, to: usize },
    Wake(usize),
    CloseSleeping(usize),
    TogglePin(usize),
    TogglePinSleeping(usize),
    ToggleBookmark,
    /// Show the page in reading mode.
    ReaderMode,
    OpenBookmark(usize),
    OpenBookmarkInNewTab(usize),
    RemoveBookmark(usize),
    /// Put a bookmark into a folder (empty name: no folder).
    MoveBookmark(usize, String),
    SetExperimental(bool),
    ToggleSettings,
}

#[derive(Default)]
pub(crate) struct State {
    /// Whether the sidebar is currently shown.
    pub visible: bool,
    /// Keep the sidebar open all the time (comes from the settings).
    pub pinned: bool,
    /// Ask the address bar to take keyboard focus (Ctrl+L).
    pub focus_location: bool,
    /// Width of the part of the window (from the left edge) that belongs to the sidebar
    /// right now, including the hot zone when hidden. Used to route mouse events.
    pub hit_width: f32,
}

/// Decide whether the sidebar is shown this frame.
pub(crate) fn update_visibility(ctx: &Context, state: &mut State) {
    let pointer_x = ctx.pointer_latest_pos().map(|pos| pos.x);
    let location_focused = ctx.memory(|memory| memory.has_focus(Id::new(LOCATION_ID)));
    let reach = if state.visible { WIDTH + 6.0 } else { HOT_ZONE };
    let near = pointer_x.is_some_and(|x| x <= reach);
    let visible = state.pinned || state.focus_location || location_focused || near;
    if visible != state.visible {
        ctx.request_repaint();
    }
    state.visible = visible;
    state.hit_width = if visible { WIDTH } else { HOT_ZONE };
}

/// Width (in egui points) that a pinned sidebar takes from the left of the page. Read by the
/// window to move mouse positions and pop-ups, which are measured from the page's corner.
static LEFT_INSET_BITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) fn set_left_inset(points: f32) {
    LEFT_INSET_BITS.store(points.to_bits(), std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn left_inset() -> f32 {
    f32::from_bits(LEFT_INSET_BITS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Is the address bar being edited right now?
pub(crate) fn location_focused(ctx: &Context) -> bool {
    ctx.memory(|memory| memory.has_focus(Id::new(LOCATION_ID)))
}

/// The accent colour of the chosen colour theme.
fn accent_color() -> Color32 {
    let [r, g, b] = crate::appearance::current_accent();
    Color32::from_rgb(r, g, b)
}

/// `base` with a little of `tint` mixed in (`amount` 0.0 to 1.0).
fn tinted(base: Color32, tint: Color32, amount: f32) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    Color32::from_rgb(mix(base.r(), tint.r()), mix(base.g(), tint.g()), mix(base.b(), tint.b()))
}

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Previous,
    Next,
    Play,
    Pause,
}

/// A round media button with its symbol drawn by the painter. `main` is the big filled one.
fn media_button(ui: &mut egui::Ui, glyph: Glyph, main: bool, accent: Color32) -> egui::Response {
    let size = if main { 34.0 } else { 28.0 };
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let painter = ui.painter();
    let center = rect.center();
    let radius = size / 2.0;
    let (fill, ink) = if main {
        let fill = if response.hovered() { accent } else { accent.gamma_multiply(0.9) };
        (fill, Color32::from_rgb(24, 24, 26))
    } else {
        let fill = if response.hovered() {
            accent.gamma_multiply(0.30)
        } else {
            accent.gamma_multiply(0.14)
        };
        (fill, ui.visuals().text_color())
    };
    painter.circle_filled(center, radius, fill);
    let u = size / 34.0 * 6.5;
    let triangle = |tip_x: f32, back_x: f32| {
        Shape::convex_polygon(
            vec![
                center + vec2(tip_x, 0.0),
                center + vec2(back_x, -u),
                center + vec2(back_x, u),
            ],
            ink,
            Stroke::NONE,
        )
    };
    match glyph {
        Glyph::Play => {
            painter.add(triangle(u * 1.0 + 1.0, -u * 0.8 + 1.0));
        },
        Glyph::Pause => {
            for dx in [-u * 0.55, u * 0.55] {
                painter.rect_filled(
                    Rect::from_center_size(center + vec2(dx, 0.0), vec2(u * 0.55, u * 1.8)),
                    1.5,
                    ink,
                );
            }
        },
        Glyph::Next => {
            painter.add(triangle(u * 0.8, -u * 0.7));
            painter.rect_filled(
                Rect::from_center_size(center + vec2(u * 0.95, 0.0), vec2(u * 0.35, u * 1.7)),
                1.0,
                ink,
            );
        },
        Glyph::Previous => {
            painter.add(triangle(-u * 0.8, u * 0.7));
            painter.rect_filled(
                Rect::from_center_size(center + vec2(-u * 0.95, 0.0), vec2(u * 0.35, u * 1.7)),
                1.0,
                ink,
            );
        },
    }
    response
}

/// Three little bars, like a sound level meter.
fn equalizer_icon(ui: &mut egui::Ui, accent: Color32, playing: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
    let heights: [f32; 3] = if playing { [9.0, 14.0, 7.0] } else { [5.0, 5.0, 5.0] };
    for (i, h) in heights.iter().enumerate() {
        let x = rect.min.x + 2.0 + i as f32 * 5.0;
        ui.painter().rect_filled(
            Rect::from_min_max(pos2(x, rect.max.y - h), pos2(x + 3.0, rect.max.y)),
            1.5,
            accent,
        );
    }
}

/// A star (outlined, or filled when the page is bookmarked), drawn with the painter.
fn star_button(ui: &mut egui::Ui, filled: bool, enabled: bool) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(vec2(26.0, 26.0), sense);
    if enabled && response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let gold = accent_color();
    let color = if !enabled {
        ui.visuals().weak_text_color()
    } else if filled {
        gold
    } else {
        ui.visuals().text_color()
    };
    let center = rect.center() + vec2(0.0, 0.5);
    let points: Vec<Pos2> = (0..10)
        .map(|i| {
            let angle = i as f32 * std::f32::consts::PI / 5.0 - std::f32::consts::FRAC_PI_2;
            let radius = if i % 2 == 0 { 9.0_f32 } else { 4.0_f32 };
            center + vec2(angle.cos(), angle.sin()) * radius
        })
        .collect();
    if filled && enabled {
        ui.painter()
            .add(Shape::convex_polygon(points, color, Stroke::new(1.0_f32, color)));
    } else {
        ui.painter()
            .add(Shape::closed_line(points, Stroke::new(1.4_f32, color)));
    }
    response
}

fn icon_button(text: &str) -> Button<'_> {
    Button::new(text).frame(false).min_size(vec2(26.0, 26.0))
}

/// A cog with triangular teeth, drawn with the painter (no font glyph needed).
fn cog_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let color = ui.visuals().text_color();
    let center = rect.center();
    // 8 triangular teeth on a round body: base-left, tip, base-right, valley.
    let teeth = 8;
    let step = std::f32::consts::TAU / teeth as f32;
    let (body, tip, half_width) = (7.0_f32, 10.4_f32, 0.27_f32);
    let at = |angle: f32, radius: f32| -> Pos2 {
        center + vec2(angle.cos(), angle.sin()) * radius
    };
    let mut points: Vec<Pos2> = Vec::with_capacity(teeth * 4);
    for i in 0..teeth {
        let angle = i as f32 * step - std::f32::consts::FRAC_PI_2;
        points.push(at(angle - half_width, body));
        points.push(at(angle, tip));
        points.push(at(angle + half_width, body));
        points.push(at(angle + step / 2.0, body));
    }
    let stroke = Stroke::new(1.6_f32, color);
    ui.painter().add(Shape::closed_line(points, stroke));
    ui.painter().circle_stroke(center, 3.0, stroke);
    response
}

/// Draw the sidebar (if visible) and report what was clicked.
/// Draw the sidebar and report what was clicked. While pinned it is a real panel that
/// takes space from the page; otherwise it floats over the page and only shows when the
/// pointer touches the left edge.
pub(crate) fn show(
    ui: &mut egui::Ui,
    state: &mut State,
    model: &Model,
    location: &mut String,
    location_dirty: &mut bool,
    height: f32,
) -> Vec<Action> {
    let mut actions = Vec::new();
    // Forget a finished tab drag.
    if ui.input(|input| !input.pointer.any_down() && !input.pointer.any_released()) {
        ui.memory_mut(|memory| memory.data.remove::<usize>(Id::new("pteron_tab_drag")));
    }
    // A steady, clearly visible typing cursor. (The blinking one needs timed repaints, which
    // Pteron does not run while idle, so it could stay invisible.)
    if ui.ctx().global_style().visuals.text_cursor.blink {
        ui.ctx().global_style_mut(|style| {
            style.visuals.text_cursor.blink = false;
            style.visuals.text_cursor.stroke = Stroke::new(2.0, accent_color());
        });
    }
    if state.pinned {
        egui::Panel::left("pteron_sidebar_panel")
            .resizable(false)
            .exact_size(WIDTH)
            .frame(Frame::NONE)
            .show_inside(ui, |ui| {
                framed(ui, state, model, location, location_dirty, height, &mut actions);
            });
        return actions;
    }
    if !state.visible {
        return actions;
    }
    let ctx = ui.ctx().clone();
    Area::new(Id::new("pteron_sidebar"))
        .order(Order::Foreground)
        .fixed_pos(pos2(0.0, 0.0))
        .show(&ctx, |ui| {
            framed(ui, state, model, location, location_dirty, height, &mut actions);
        });
    actions
}

fn framed(
    ui: &mut egui::Ui,
    state: &mut State,
    model: &Model,
    location: &mut String,
    location_dirty: &mut bool,
    height: f32,
    actions: &mut Vec<Action>,
) {
    let margin = 8.0;
    {
        {
            Frame::default()
                .fill(tinted(ui.visuals().window_fill, accent_color(), 0.06))
                .stroke(ui.visuals().window_stroke)
                .inner_margin(margin)
                .show(ui, |ui| {
                    ui.set_width(WIDTH - 2.0 * margin);
                    ui.set_min_height(height - 2.0 * margin);
                    let inner_width = ui.available_width();

                    // Navigation buttons.
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(model.can_go_back, icon_button("⏴"))
                            .on_hover_text("Back")
                            .clicked()
                        {
                            *location_dirty = false;
                            actions.push(Action::Back);
                        }
                        if ui
                            .add_enabled(model.can_go_forward, icon_button("⏵"))
                            .on_hover_text("Forward")
                            .clicked()
                        {
                            *location_dirty = false;
                            actions.push(Action::Forward);
                        }
                        if !model.loading &&
                            ui.add(icon_button("↻")).on_hover_text("Reload").clicked()
                        {
                            *location_dirty = false;
                            actions.push(Action::Reload);
                        }
                        if model.loading {
                            ui.add_enabled(false, icon_button("…"));
                        }
                        if star_button(ui, model.bookmarked, model.can_bookmark)
                            .on_hover_text(if model.bookmarked {
                                "Remove bookmark"
                            } else {
                                "Bookmark this page"
                            })
                            .clicked()
                        {
                            actions.push(Action::ToggleBookmark);
                        }
                        if ui
                            .add_enabled(model.can_bookmark, icon_button("Aa"))
                            .on_hover_text("Reading mode: clean text and pictures only")
                            .clicked()
                        {
                            actions.push(Action::ReaderMode);
                        }
                    });
                    ui.add_space(4.0);

                    // Address bar.
                    let location_id = Id::new(LOCATION_ID);
                    let field = ui.add_sized(
                        [inner_width, 28.0],
                        TextEdit::singleline(location)
                            .id(location_id)
                            .hint_text("Search or enter address"),
                    );
                    if field.changed() {
                        *location_dirty = true;
                    }
                    match model.security {
                        Security::Secure => {
                            ui.label(
                                RichText::new("\u{1F512} Secure connection (https)")
                                    .small()
                                    .color(Color32::from_rgb(70, 160, 90)),
                            );
                        },
                        Security::NotSecure => {
                            ui.label(
                                RichText::new("\u{26A0} Not secure (http) - do not enter passwords")
                                    .small()
                                    .color(Color32::from_rgb(220, 150, 40)),
                            );
                        },
                        Security::None => {},
                    }
                    if field.has_focus() && *location_dirty && !model.suggestions.is_empty() {
                        suggestion_popup(
                            ui.ctx(),
                            field.rect,
                            &model.suggestions,
                            location_dirty,
                            actions,
                        );
                    }
                    let shortcut = ui.input_mut(|input| {
                        input.consume_key(Modifiers::COMMAND, Key::L) ||
                            (!cfg!(target_os = "macos") &&
                                input.consume_key(Modifiers::ALT, Key::D))
                    });
                    if shortcut || state.focus_location {
                        field.request_focus();
                        state.focus_location = false;
                    }
                    if field.gained_focus() &&
                        let Some(mut edit) = TextEditState::load(ui.ctx(), location_id)
                    {
                        edit.cursor.set_char_range(Some(CCursorRange::two(
                            CCursor::new(0),
                            CCursor::new(location.len()),
                        )));
                        edit.store(ui.ctx(), location_id);
                    }
                    if field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                        actions.push(Action::Go(location.clone()));
                    }
                    ui.add_space(6.0);

                    if ui
                        .add_sized([inner_width, ROW_HEIGHT], Button::new("+  New tab"))
                        .clicked()
                    {
                        actions.push(Action::NewTab);
                    }
                    // Pteron: what Plume Music is playing, with simple controls.
                    if let Some(now) = crate::music::now_playing() {
                        let accent = accent_color();
                        ui.add_space(8.0);
                        Frame::new()
                            .fill(tinted(ui.visuals().window_fill, accent, 0.12))
                            .stroke(Stroke::new(1.0, accent.gamma_multiply(0.35)))
                            .corner_radius(12.0)
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.set_width(inner_width - 22.0);
                                ui.horizontal(|ui| {
                                    equalizer_icon(ui, accent, now.playing);
                                    ui.add(
                                        egui::Label::new(RichText::new(&now.title).strong().size(13.0))
                                            .truncate(),
                                    );
                                });
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    // Centre the three buttons.
                                    let used = 28.0 + 34.0 + 28.0 + 2.0 * ui.spacing().item_spacing.x * 1.0;
                                    ui.add_space(((ui.available_width() - used) / 2.0).max(0.0));
                                    if media_button(ui, Glyph::Previous, false, accent)
                                        .on_hover_text("Previous song")
                                        .clicked()
                                    {
                                        crate::music::send_command("prev");
                                    }
                                    let glyph = if now.playing { Glyph::Pause } else { Glyph::Play };
                                    if media_button(ui, glyph, true, accent)
                                        .on_hover_text(if now.playing { "Pause" } else { "Play" })
                                        .clicked()
                                    {
                                        crate::music::send_command("toggle");
                                    }
                                    if media_button(ui, Glyph::Next, false, accent)
                                        .on_hover_text("Next song")
                                        .clicked()
                                    {
                                        crate::music::send_command("next");
                                    }
                                });
                            });
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(1000));
                    }
                    ui.add_space(6.0);
                    ui.separator();

                    // Vertical tab list.
                    let list_height = (height - 190.0).max(60.0);
                    ScrollArea::vertical()
                        .max_height(list_height)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            // Pinned tabs first, then the rest.
                            let any_pinned = model.tabs.iter().any(|tab| tab.pinned) ||
                                model.sleeping.iter().any(|tab| tab.pinned);
                            for section_pinned in [true, false] {
                                if section_pinned {
                                    if !any_pinned {
                                        continue;
                                    }
                                } else if any_pinned {
                                    ui.add_space(2.0);
                                    ui.separator();
                                    ui.add_space(2.0);
                                }
                                for (index, tab) in model.tabs.iter().enumerate() {
                                    if tab.pinned == section_pinned {
                                        tab_row(ui, index, tab, actions);
                                    }
                                }
                                for (index, tab) in model.sleeping.iter().enumerate() {
                                    if tab.pinned == section_pinned {
                                        sleeping_row(ui, index, tab, actions);
                                    }
                                }
                            }
                            downloads_section(ui);
                            if !model.bookmarks.is_empty() {
                                ui.add_space(6.0);
                                ui.separator();
                                ui.label(RichText::new("Bookmarks").small().weak());
                                let mut folders: Vec<&str> = model
                                    .bookmarks
                                    .iter()
                                    .filter(|b| !b.folder.is_empty())
                                    .map(|b| b.folder.as_str())
                                    .collect();
                                folders.sort_by_key(|name| name.to_lowercase());
                                folders.dedup();
                                for (index, bookmark) in model.bookmarks.iter().enumerate() {
                                    if bookmark.folder.is_empty() {
                                        bookmark_row(ui, index, bookmark, &folders, actions);
                                    }
                                }
                                for name in &folders {
                                    egui::CollapsingHeader::new(RichText::new(*name).size(13.0))
                                        .id_salt(("pteron_bookmark_folder", *name))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            for (index, bookmark) in model.bookmarks.iter().enumerate() {
                                                if bookmark.folder == *name {
                                                    bookmark_row(ui, index, bookmark, &folders, actions);
                                                }
                                            }
                                        });
                                }
                            }
                        });

                    ui.add_space(6.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        let mut experimental = model.experimental;
                        if ui
                            .add_sized(
                                [26.0, 26.0],
                                Button::new(RichText::new("☢").size(16.0))
                                    .frame(false)
                                    .selected(experimental),
                            )
                            .on_hover_text("Enable experimental prefs")
                            .clicked()
                        {
                            experimental = !experimental;
                            *location_dirty = false;
                            actions.push(Action::SetExperimental(experimental));
                        }
                        if ui
                            .add_sized([26.0, 26.0], Button::new(RichText::new("⊞").size(16.0)).frame(false))
                            .on_hover_text("New window")
                            .clicked()
                        {
                            actions.push(Action::NewWindow);
                        }
                        if cog_button(ui).on_hover_text("Settings").clicked() {
                            actions.push(Action::ToggleSettings);
                        }
                    });
                });
        }
    }
}

/// The list of downloads (hidden when there are none).
fn downloads_section(ui: &mut egui::Ui) {
    use crate::downloads::{self, State};
    let items = downloads::snapshot();
    if items.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.separator();
    ui.label(RichText::new("Downloads").small().weak());
    for item in items.iter().rev().take(8) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut name = item.name.clone();
            if name.chars().count() > 26 {
                name = format!("{}...", name.chars().take(25).collect::<String>());
            }
            ui.label(RichText::new(name).strong());
            match &item.state {
                State::Confirm => {
                    ui.label(
                        RichText::new("\u{26A0} This is a program. It can harm your computer if you do not trust the site.")
                            .small()
                            .color(Color32::from_rgb(220, 150, 40)),
                    );
                    ui.horizontal(|ui| {
                        if ui.small_button("Download anyway").clicked() {
                            downloads::confirm(item.id);
                        }
                        if ui.small_button("Cancel").clicked() {
                            downloads::cancel(item.id);
                        }
                    });
                },
                State::Running => {
                    let fraction = item
                        .total
                        .filter(|total| *total > 0)
                        .map(|total| (item.received as f32 / total as f32).clamp(0.0, 1.0));
                    match fraction {
                        Some(fraction) => {
                            ui.add(egui::ProgressBar::new(fraction).desired_height(8.0));
                            ui.label(
                                RichText::new(format!(
                                    "{} of {}",
                                    downloads::format_bytes(item.received),
                                    downloads::format_bytes(item.total.unwrap_or(0))
                                ))
                                .small()
                                .weak(),
                            );
                        },
                        None => {
                            ui.label(
                                RichText::new(format!(
                                    "{} downloaded",
                                    downloads::format_bytes(item.received)
                                ))
                                .small()
                                .weak(),
                            );
                        },
                    }
                    if ui.small_button("Cancel").clicked() {
                        downloads::cancel(item.id);
                    }
                },
                State::Done => {
                    ui.label(
                        RichText::new(format!("Done, {}", downloads::format_bytes(item.received)))
                            .small()
                            .color(Color32::from_rgb(70, 160, 90)),
                    );
                    ui.horizontal(|ui| {
                        if !item.dangerous && ui.small_button("Open").clicked() {
                            downloads::open_file(item);
                        }
                        if ui.small_button("Show in folder").clicked() {
                            downloads::show_in_folder(&item.path);
                        }
                        if ui.small_button("\u{2212}").on_hover_text("Remove from list").clicked() {
                            downloads::remove(item.id);
                        }
                    });
                },
                State::Failed(reason) => {
                    ui.label(RichText::new(reason.clone()).small().color(Color32::from_rgb(220, 90, 80)));
                    if ui.small_button("Remove").clicked() {
                        downloads::remove(item.id);
                    }
                },
                State::Cancelled => {
                    ui.label(RichText::new("Cancelled").small().weak());
                    if ui.small_button("Remove").clicked() {
                        downloads::remove(item.id);
                    }
                },
            }
        });
    }
}

fn tab_row(ui: &mut egui::Ui, index: usize, tab: &TabRow, actions: &mut Vec<Action>) {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click_and_drag());
    // Drag to reorder: remember which row was picked up, show a line where it would land.
    let drag_key = Id::new("pteron_tab_drag");
    if response.drag_started() {
        ui.memory_mut(|memory| memory.data.insert_temp(drag_key, index));
    }
    let dragged: Option<usize> = ui.memory(|memory| memory.data.get_temp(drag_key));
    if let Some(from) = dragged &&
        from != index &&
        ui.rect_contains_pointer(rect) &&
        ui.input(|input| input.pointer.is_decidedly_dragging() || input.pointer.any_released())
    {
        let y = if from > index { rect.min.y } else { rect.max.y };
        ui.painter().line_segment(
            [pos2(rect.min.x + 4.0, y), pos2(rect.max.x - 4.0, y)],
            Stroke::new(2.0, accent_color()),
        );
        if ui.input(|input| input.pointer.any_released()) {
            actions.push(Action::MoveTab { from, to: index });
        }
    }
    let close_rect = Rect::from_min_size(
        pos2(rect.max.x - 26.0, rect.min.y + 2.0),
        vec2(24.0, ROW_HEIGHT - 4.0),
    );
    let close = ui.interact(close_rect, response.id.with("close"), Sense::click());
    let row_hovered = ui.rect_contains_pointer(rect);

    let visuals = ui.visuals();
    let fill = if tab.active {
        Some(visuals.widgets.active.weak_bg_fill)
    } else if row_hovered {
        Some(visuals.widgets.hovered.weak_bg_fill)
    } else {
        None
    };
    let text_color = visuals.text_color();
    let painter = ui.painter();
    if let Some(fill) = fill {
        painter.rect_filled(rect, 6.0, fill);
    }
    if let Some(favicon) = tab.favicon {
        let icon_rect = Rect::from_min_size(pos2(rect.min.x + 6.0, rect.center().y - 8.0), vec2(16.0, 16.0));
        painter.image(
            favicon.id,
            icon_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    let text_clip = Rect::from_min_max(rect.min, pos2(rect.max.x - 28.0, rect.max.y));
    painter.with_clip_rect(text_clip).text(
        pos2(rect.min.x + 30.0, rect.center().y),
        Align2::LEFT_CENTER,
        &tab.label,
        FontId::proportional(13.0),
        text_color,
    );
    if tab.pinned {
        // A pinned tab shows a pin instead of a close button; clicking it unpins.
        pin_glyph(painter, close_rect.center(), text_color, row_hovered);
    } else if row_hovered {
        painter.text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::proportional(16.0),
            text_color,
        );
    }

    if tab.pinned {
        if close.clicked() {
            actions.push(Action::TogglePin(index));
        } else if response.clicked() && !tab.active {
            actions.push(Action::Activate(index));
        }
    } else if close.clicked() || close.middle_clicked() || response.middle_clicked() {
        actions.push(Action::Close(index));
    } else if response.clicked() && !tab.active {
        actions.push(Action::Activate(index));
    }
    response.context_menu(|ui| {
        let pin_label = if tab.pinned { "Unpin tab" } else { "Pin tab" };
        if ui.button(pin_label).clicked() {
            actions.push(Action::TogglePin(index));
            ui.close();
        }
        if ui.button("Close tab").clicked() {
            if tab.pinned {
                actions.push(Action::TogglePin(index));
            }
            actions.push(Action::Close(index));
            ui.close();
        }
    });
}

/// The list of matching pages under the address bar. Rows react to the mouse *press*, because
/// the address bar loses focus (and this list disappears) before the button is released.
fn suggestion_popup(
    ctx: &Context,
    field: Rect,
    suggestions: &[crate::library::Suggestion],
    location_dirty: &mut bool,
    actions: &mut Vec<Action>,
) {
    Area::new(Id::new("pteron_suggestions"))
        .order(Order::Tooltip)
        .fixed_pos(field.left_bottom() + vec2(0.0, 2.0))
        .show(ctx, |ui| {
            Frame::popup(ui.style()).inner_margin(4.0).show(ui, |ui| {
                ui.set_width(field.width() - 8.0);
                for suggestion in suggestions {
                    let (rect, response) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
                    let visuals = ui.visuals();
                    if response.hovered() {
                        ui.painter()
                            .rect_filled(rect, 5.0, visuals.widgets.hovered.weak_bg_fill);
                    }
                    let text_color = visuals.text_color();
                    let weak = visuals.weak_text_color();
                    let title = if suggestion.title.is_empty() {
                        suggestion.url.as_str()
                    } else {
                        suggestion.title.as_str()
                    };
                    let clip = ui.painter().with_clip_rect(rect.shrink2(vec2(4.0, 0.0)));
                    clip.text(
                        pos2(rect.min.x + 8.0, rect.min.y + 11.0),
                        Align2::LEFT_CENTER,
                        title,
                        FontId::proportional(13.0),
                        text_color,
                    );
                    clip.text(
                        pos2(rect.min.x + 8.0, rect.min.y + 27.0),
                        Align2::LEFT_CENTER,
                        &suggestion.url,
                        FontId::proportional(11.0),
                        weak,
                    );
                    let pressed = ui.input(|input| input.pointer.primary_pressed());
                    if pressed && response.hovered() {
                        *location_dirty = false;
                        actions.push(Action::Go(suggestion.url.clone()));
                    }
                }
            });
        });
}

/// A bookmark: click opens it here, middle-click opens it in a new tab.
fn bookmark_row(
    ui: &mut egui::Ui,
    index: usize,
    bookmark: &BookmarkRow,
    folders: &[&str],
    actions: &mut Vec<Action>,
) {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    let visuals = ui.visuals();
    let fill = response
        .hovered()
        .then_some(visuals.widgets.hovered.weak_bg_fill);
    let text_color = visuals.text_color();
    let painter = ui.painter();
    if let Some(fill) = fill {
        painter.rect_filled(rect, 6.0, fill);
    }
    painter.with_clip_rect(rect).text(
        pos2(rect.min.x + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        &bookmark.label,
        FontId::proportional(13.0),
        text_color,
    );
    if response.middle_clicked() {
        actions.push(Action::OpenBookmarkInNewTab(index));
    } else if response.clicked() {
        actions.push(Action::OpenBookmark(index));
    }
    let response = response.on_hover_text(&bookmark.url);
    response.context_menu(|ui| {
        if ui.button("Open in new tab").clicked() {
            actions.push(Action::OpenBookmarkInNewTab(index));
            ui.close();
        }
        ui.menu_button("Move to folder", |ui| {
            if !bookmark.folder.is_empty() && ui.button("No folder").clicked() {
                actions.push(Action::MoveBookmark(index, String::new()));
                ui.close();
            }
            for name in folders {
                if *name != bookmark.folder && ui.button(*name).clicked() {
                    actions.push(Action::MoveBookmark(index, name.to_string()));
                    ui.close();
                }
            }
            ui.separator();
            ui.label(RichText::new("New folder").small().weak());
            let id = Id::new(("pteron_new_folder", index));
            let mut text = ui.memory_mut(|m| m.data.get_temp::<String>(id).unwrap_or_default());
            let edit = ui.add(egui::TextEdit::singleline(&mut text).desired_width(140.0));
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter && !text.trim().is_empty() {
                actions.push(Action::MoveBookmark(index, text.trim().to_string()));
                text.clear();
                ui.close();
            }
            ui.memory_mut(|m| m.data.insert_temp(id, text));
        });
        if ui.button("Remove bookmark").clicked() {
            actions.push(Action::RemoveBookmark(index));
            ui.close();
        }
    });
}

/// A small pin, drawn with the painter. Filled when `strong`.
fn pin_glyph(painter: &egui::Painter, center: Pos2, color: Color32, strong: bool) {
    let color = if strong { color } else { color.gamma_multiply(0.6) };
    painter.circle_filled(center + vec2(0.0, -3.0), 3.2, color);
    painter.line_segment(
        [center + vec2(0.0, -1.0), center + vec2(0.0, 6.0)],
        Stroke::new(1.4_f32, color),
    );
}

fn sleeping_row(ui: &mut egui::Ui, index: usize, tab: &SleepingRow, actions: &mut Vec<Action>) {
    let label = tab.label.as_str();
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    let visuals = ui.visuals();
    let fill = response
        .hovered()
        .then_some(visuals.widgets.hovered.weak_bg_fill);
    let text_color = visuals.weak_text_color();
    let painter = ui.painter();
    if let Some(fill) = fill {
        painter.rect_filled(rect, 6.0, fill);
    }
    painter.with_clip_rect(rect).text(
        pos2(rect.min.x + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!("zZ  {label}"),
        FontId::proportional(13.0),
        text_color,
    );
    if response.middle_clicked() {
        actions.push(Action::CloseSleeping(index));
    } else if response.clicked() {
        actions.push(Action::Wake(index));
    }
    if tab.pinned {
        pin_glyph(
            painter,
            pos2(rect.max.x - 14.0, rect.center().y),
            text_color,
            false,
        );
    }
    response.context_menu(|ui| {
        let pin_label = if tab.pinned { "Unpin tab" } else { "Pin tab" };
        if ui.button(pin_label).clicked() {
            actions.push(Action::TogglePinSleeping(index));
            ui.close();
        }
        if ui.button("Close tab").clicked() {
            actions.push(Action::CloseSleeping(index));
            ui.close();
        }
    });
}

