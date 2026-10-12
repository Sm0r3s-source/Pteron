/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pteron: a full-window settings page (categories on the left, options on the right),
//! drawn with egui on top of the page area.

use egui::{
    Align2, Area, Color32, Context, FontId, Frame, Id, Key, Order, Rect, RichText, ScrollArea, Sense,
    TextEdit, pos2, vec2,
};

use crate::{pteron, updater};

const NAV_WIDTH: f32 = 220.0;
const CONTENT_WIDTH: f32 = 680.0;
const QUERY_ID: &str = "pteron_settings_query";
const HITS_ID: &str = "pteron_settings_hits";

#[derive(Clone, Copy, Default, PartialEq)]
pub(crate) enum Category {
    #[default]
    Search,
    Appearance,
    Startup,
    Tabs,
    Video,
    Compatibility,
    Privacy,
    About,
}

impl Category {
    const ALL: [Category; 8] = [
        Category::Search,
        Category::Appearance,
        Category::Startup,
        Category::Tabs,
        Category::Video,
        Category::Compatibility,
        Category::Privacy,
        Category::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Category::Search => "Search",
            Category::Appearance => "Appearance",
            Category::Startup => "On startup",
            Category::Tabs => "Tabs & memory",
            Category::Video => "Video",
            Category::Compatibility => "Web compatibility",
            Category::Privacy => "Privacy & data",
            Category::About => "About",
        }
    }
}

/// Things the page asks the app to do (the page itself cannot reach the engine).
pub(crate) enum PageAction {
    ClearCookies,
    ClearCache,
    ForgetSession,
    ClearHistory,
    /// Close Pteron and let the downloaded update replace the files.
    InstallUpdate,
    /// Save a problem report file on the Desktop, with this note from the user.
    SaveReport(String),
    /// Open the page listing known problems in a new tab.
    OpenKnownIssues,
    /// Put the report text on the clipboard.
    CopyReport(String),
    /// Save the bookmarks as an .html file on the Desktop.
    ExportBookmarks,
    /// Import the newest bookmark file found on the Desktop or in Downloads.
    ImportBookmarks,
    /// Open the file chooser for the new tab background picture.
    ChoosePicture,
    /// Open the releases page in a new tab (the manual way to update).
    OpenDownloadPage,
    /// Open the folder chooser for syncing.
    ChooseSyncFolder,
    /// Compare with the sync folder right now.
    SyncNow,
    /// Stop syncing (the file in the folder is left alone).
    StopSync,
}

#[derive(Default)]
pub(crate) struct PageState {
    pub open: bool,
    pub category: Category,
    /// Requests for the app to carry out after this frame.
    pub pending: Vec<PageAction>,
    /// Result of the last request, shown under the buttons.
    pub notice: Option<String>,
    /// Text typed in "Find in Settings".
    pub query: String,
    /// Text typed in "Report a problem".
    pub report_note: String,
}

/// Draw the settings page over `area` when it is open. Returns true if a setting changed.
pub(crate) fn show(
    ctx: &Context,
    page: &mut PageState,
    settings: &mut pteron::Settings,
    area: Rect,
) -> bool {
    if !page.open {
        return false;
    }
    let mut changed = false;
    let mut close = ctx.input(|input| input.key_pressed(Key::Escape));
    let accent = {
        let [r, g, b] = crate::appearance::current_accent();
        Color32::from_rgb(r, g, b)
    };

    Area::new(Id::new("pteron_settings_page"))
        .order(Order::Middle)
        .fixed_pos(area.min)
        .show(ctx, |ui| {
            Frame::NONE
                .fill(ui.visuals().panel_fill)
                .show(ui, |ui| {
                    ui.set_min_size(area.size());
                    ui.set_max_size(area.size());
                    ui.horizontal_top(|ui| {
                        // Left: categories, Firefox-style list.
                        // Note: `allocate_ui` would inherit the horizontal layout of the
                        // parent and put the entries side by side, so ask for top-down.
                        ui.allocate_ui_with_layout(
                            vec2(NAV_WIDTH, area.height()),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                            ui.set_min_height(area.height());
                            ui.add_space(22.0);
                            ui.horizontal(|ui| {
                                ui.add_space(20.0);
                                ui.label(RichText::new("Settings").size(22.0).strong());
                            });
                            ui.add_space(18.0);
                            for category in Category::ALL {
                                let selected = page.query.is_empty() && page.category == category;
                                if nav_row(ui, category.label(), selected, accent).clicked() {
                                    page.category = category;
                                    page.query.clear();
                                }
                            }
                        },
                        );

                        // Right: find box on top, then the options.
                        ui.vertical(|ui| {
                            let width = area.width() - NAV_WIDTH;
                            ui.set_width(width);
                            ui.add_space(18.0);
                            ui.horizontal(|ui| {
                                ui.add_space(16.0);
                                ui.add(
                                    TextEdit::singleline(&mut page.query)
                                        .hint_text("Find in Settings")
                                        .desired_width(300.0)
                                        .margin(vec2(10.0, 6.0)),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.add_space(16.0);
                                        if ui.button("Close  ✕").clicked() {
                                            close = true;
                                        }
                                    },
                                );
                            });
                            ui.add_space(6.0);

                            ctx.data_mut(|data| {
                                data.insert_temp(Id::new(QUERY_ID), page.query.trim().to_lowercase());
                                data.insert_temp(Id::new(HITS_ID), 0usize);
                            });
                            let searching = !page.query.trim().is_empty();

                            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                                let margin = ((width - CONTENT_WIDTH) / 2.0).max(16.0);
                                ui.horizontal_top(|ui| {
                                    ui.add_space(margin);
                                    ui.vertical(|ui| {
                                        ui.set_width((width - 2.0 * margin - 8.0).min(CONTENT_WIDTH));
                                        ui.add_space(8.0);
                                        let categories: Vec<Category> = if searching {
                                            Category::ALL.to_vec()
                                        } else {
                                            vec![page.category]
                                        };
                                        for category in categories {
                                            if !searching {
                                                ui.label(
                                                    RichText::new(category.label())
                                                        .size(26.0)
                                                        .strong(),
                                                );
                                                ui.add_space(14.0);
                                            }
                                            match category {
                                                Category::Search => {
                                                    changed |= search_section(ui, settings)
                                                },
                                                Category::Appearance => {
                                                    changed |= appearance_section(ui, settings, &mut page.pending, &page.notice)
                                                },
                                                Category::Startup => {
                                                    changed |= startup_section(ui, settings)
                                                },
                                                Category::Tabs => {
                                                    changed |= tabs_section(ui, settings)
                                                },
                                                Category::Video => {
                                                    changed |= video_section(ui, settings)
                                                },
                                                Category::Compatibility => {
                                                    changed |= compat_section(ui, settings)
                                                },
                                                Category::Privacy => privacy_section(
                                                    ui,
                                                    &mut page.pending,
                                                    &page.notice,
                                                    settings,
                                                    &mut changed,
                                                ),
                                                Category::About => {
                                                    if about_section(ui, &mut page.pending, &mut page.report_note, &page.notice) {
                                                        *settings = pteron::Settings::default();
                                                        changed = true;
                                                    }
                                                },
                                            }
                                        }
                                        if searching &&
                                            ui.data(|data| {
                                                data.get_temp::<usize>(Id::new(HITS_ID))
                                                    .unwrap_or(0)
                                            }) == 0
                                        {
                                            ui.label(
                                                RichText::new("Sorry! No results in Settings for your search.")
                                                    .weak(),
                                            );
                                        }
                                        ui.add_space(30.0);
                                    });
                                });
                            });
                        });
                    });
                });
        });

    if close {
        page.open = false;
        page.query.clear();
    }
    changed
}

/// One entry of the category list: rounded highlight, accent-colored when selected.
fn nav_row(ui: &mut egui::Ui, label: &str, selected: bool, accent: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(NAV_WIDTH - 16.0, 38.0), Sense::click());
    let rect = rect.translate(vec2(8.0, 0.0));
    let visuals = ui.visuals();
    let fill = if selected {
        Some(accent.gamma_multiply(0.18))
    } else if response.hovered() {
        Some(visuals.widgets.hovered.weak_bg_fill)
    } else {
        None
    };
    let text_color = if selected { accent } else { visuals.text_color() };
    let painter = ui.painter();
    if let Some(fill) = fill {
        painter.rect_filled(rect, 8.0, fill);
    }
    painter.text(
        pos2(rect.min.x + 16.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(15.0),
        text_color,
    );
    response
}

/// True when the card should be drawn for the current "Find in Settings" text.
fn matches_query(ui: &egui::Ui, text: &str) -> bool {
    let query = ui
        .data(|data| data.get_temp::<String>(Id::new(QUERY_ID)))
        .unwrap_or_default();
    let show = query.is_empty() || text.to_lowercase().contains(&query);
    if show {
        ui.data_mut(|data| {
            *data.get_temp_mut_or_default::<usize>(Id::new(HITS_ID)) += 1;
        });
    }
    show
}

/// A Firefox-style group: rounded box, bold title, optional description, then the options.
fn card(
    ui: &mut egui::Ui,
    title: &str,
    description: Option<&str>,
    keywords: &str,
    add: impl FnOnce(&mut egui::Ui),
) {
    let haystack = format!("{title} {} {keywords}", description.unwrap_or_default());
    if !matches_query(ui, &haystack) {
        return;
    }
    Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(18.0)
        .corner_radius(10.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(16.0));
            if let Some(description) = description {
                ui.add_space(2.0);
                ui.label(RichText::new(description).weak());
            }
            ui.add_space(10.0);
            add(ui);
        });
    ui.add_space(14.0);
}

/// A radio button with a grey explanation indented under it.
fn radio(
    ui: &mut egui::Ui,
    current: &mut String,
    value: &str,
    label: &str,
    description: Option<&str>,
) -> bool {
    let changed = ui
        .radio_value(current, value.to_string(), RichText::new(label).size(14.5))
        .changed();
    if let Some(description) = description {
        ui.horizontal(|ui| {
            ui.add_space(26.0);
            ui.label(RichText::new(description).small().weak());
        });
    }
    ui.add_space(4.0);
    changed
}

fn search_section(ui: &mut egui::Ui, settings: &mut pteron::Settings) -> bool {
    let mut changed = false;
    card(
        ui,
        "Default search engine",
        Some("Choose the search engine used from the address bar and the new tab page."),
        "search engine duckduckgo google bing brave startpage searxng",
        |ui| {
            for (id, label, _) in pteron::ENGINES {
                changed |= radio(ui, &mut settings.engine, id, label, None);
            }
            changed |= radio(
                ui,
                &mut settings.engine,
                pteron::SEARXNG_ID,
                "SearXNG (my own instance)",
                Some("Use a search server you run yourself."),
            );
        },
    );
    if settings.engine == pteron::SEARXNG_ID {
        card(
            ui,
            "Your SearXNG address",
            Some("Use %s where the search words go. If you leave it out, /search?q=%s is added."),
            "searxng address url instance",
            |ui| {
                changed |= ui
                    .add(
                        TextEdit::singleline(&mut settings.searxng_url)
                            .hint_text("https://search.example.org")
                            .desired_width(f32::INFINITY)
                            .margin(vec2(10.0, 6.0)),
                    )
                    .changed();
            },
        );
    }
    let target = pteron::search_url_for(settings);
    card(
        ui,
        "Where searches go",
        Some("Searches are sent to this address."),
        "search address url",
        |ui| {
            ui.label(RichText::new(target).small());
        },
    );
    changed
}

/// A small preview of a theme: its background, a card, a button and its accent colour.
fn theme_tile(ui: &mut egui::Ui, name: &str, palette: &crate::appearance::Palette, selected: bool) -> egui::Response {
    let c = |rgb: [u8; 3]| Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(104.0, 78.0), egui::Sense::click());
    let painter = ui.painter();
    painter.rect_filled(rect, 10.0, c(palette.bg));
    let ring = if selected {
        c(palette.accent)
    } else if response.hovered() {
        ui.visuals().text_color().gamma_multiply(0.5)
    } else {
        ui.visuals().text_color().gamma_multiply(0.18)
    };
    painter.rect_stroke(rect, 10.0, egui::Stroke::new(if selected { 2.5 } else { 1.0 }, ring), egui::StrokeKind::Inside);
    let card = egui::Rect::from_min_size(rect.min + egui::vec2(10.0, 10.0), egui::vec2(rect.width() - 20.0, 24.0));
    painter.rect_filled(card, 6.0, c(palette.surface));
    painter.circle_filled(card.left_center() + egui::vec2(9.0, 0.0), 4.5, c(palette.accent));
    painter.rect_filled(
        egui::Rect::from_min_size(card.min + egui::vec2(22.0, 9.0), egui::vec2(card.width() - 34.0, 6.0)),
        3.0,
        c(palette.muted).gamma_multiply(0.55),
    );
    painter.text(
        rect.left_bottom() + egui::vec2(10.0, -10.0),
        egui::Align2::LEFT_BOTTOM,
        name,
        egui::FontId::proportional(13.0),
        c(palette.text),
    );
    response
}

/// A round colour sample; `selected` draws a ring around it.
fn swatch(ui: &mut egui::Ui, rgb: [u8; 3], selected: bool, name: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::click());
    let color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
    ui.painter().circle_filled(rect.center(), 11.0, color);
    if selected {
        ui.painter()
            .circle_stroke(rect.center(), 14.0, egui::Stroke::new(2.0, ui.visuals().text_color()));
    } else if response.hovered() {
        ui.painter()
            .circle_stroke(rect.center(), 13.0, egui::Stroke::new(1.5, color.gamma_multiply(0.6)));
    }
    response.on_hover_text(name)
}

fn appearance_section(
    ui: &mut egui::Ui,
    settings: &mut pteron::Settings,
    pending: &mut Vec<PageAction>,
    notice: &Option<String>,
) -> bool {
    let mut changed = false;
    card(
        ui,
        "Theme",
        Some("Colours for the sidebar, settings, buttons and the new tab page."),
        "theme light dark appearance color colour palette midnight forest sunset custom personalize",
        |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                let plain: [(&str, &str, Option<crate::appearance::Palette>); 3] = [
                    ("system", "System", None),
                    ("light", "Light", Some(crate::appearance::LIGHT_PREVIEW)),
                    ("dark", "Dark", Some(crate::appearance::DARK_PREVIEW)),
                ];
                for (id, name, preview) in plain {
                    let preview = preview.unwrap_or_else(|| {
                        if ui.visuals().dark_mode {
                            crate::appearance::DARK_PREVIEW
                        } else {
                            crate::appearance::LIGHT_PREVIEW
                        }
                    });
                    if theme_tile(ui, name, &preview, settings.theme == id).clicked() {
                        settings.theme = id.to_string();
                        changed = true;
                    }
                }
                for (id, name, palette) in crate::appearance::THEMES {
                    if theme_tile(ui, name, &palette, settings.theme == id).clicked() {
                        settings.theme = id.to_string();
                        changed = true;
                    }
                }
                let custom = crate::appearance::parse_hex(&settings.custom_bg).unwrap_or([0x1b, 0x1c, 0x1e]);
                let custom_palette = crate::appearance::palette_for(&pteron::Settings {
                    theme: "custom".to_string(),
                    custom_bg: crate::appearance::to_hex(custom),
                    accent: "auto".to_string(),
                    ..settings.clone()
                })
                .unwrap_or(crate::appearance::DARK_PREVIEW);
                if theme_tile(ui, "Custom", &custom_palette, settings.theme == "custom").clicked() {
                    settings.theme = "custom".to_string();
                    changed = true;
                }
            });
            if settings.theme == "custom" {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let mut rgb = crate::appearance::parse_hex(&settings.custom_bg).unwrap_or([0x1b, 0x1c, 0x1e]);
                    if ui.color_edit_button_srgb(&mut rgb).changed() {
                        settings.custom_bg = crate::appearance::to_hex(rgb);
                        changed = true;
                    }
                    ui.label(RichText::new("Background colour. Text and buttons adjust to it.").weak());
                });
            }
        },
    );
    card(
        ui,
        "Accent colour",
        Some("Highlights, selections, the star and the new tab page glow."),
        "color colour accent palette custom personalize highlight",
        |ui| {
            ui.horizontal_wrapped(|ui| {
                let auto = crate::appearance::palette_for(&pteron::Settings { accent: "auto".to_string(), ..settings.clone() })
                    .map(|p| p.accent)
                    .unwrap_or(crate::appearance::PRESETS[0].2);
                if swatch(ui, auto, settings.accent == "auto", "Theme default").clicked() {
                    settings.accent = "auto".to_string();
                    changed = true;
                }
                for (id, name, rgb) in crate::appearance::PRESETS {
                    if swatch(ui, rgb, settings.accent == id, name).clicked() {
                        settings.accent = id.to_string();
                        changed = true;
                    }
                }
                let custom = crate::appearance::parse_hex(&settings.accent_custom).unwrap_or([0xd6, 0xaa, 0x48]);
                if swatch(ui, custom, settings.accent == "custom", "Your own colour").clicked() {
                    settings.accent = "custom".to_string();
                    changed = true;
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let mut rgb = crate::appearance::parse_hex(&settings.accent_custom).unwrap_or([0xd6, 0xaa, 0x48]);
                if ui.color_edit_button_srgb(&mut rgb).changed() {
                    settings.accent_custom = crate::appearance::to_hex(rgb);
                    settings.accent = "custom".to_string();
                    changed = true;
                }
                ui.label(RichText::new("Pick your own colour").weak());
            });
        },
    );
    card(
        ui,
        "New tab background",
        Some("What the page behind the search box looks like."),
        "background wallpaper picture image homepage new tab color gradient",
        |ui| {
            changed |= radio(ui, &mut settings.newtab_bg, "theme", "Soft glow in the theme colour", None);
            changed |= radio(ui, &mut settings.newtab_bg, "gradient", "Gradient in the theme colour", None);
            changed |= radio(ui, &mut settings.newtab_bg, "color", "One colour", None);
            if settings.newtab_bg == "color" {
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    let mut rgb = crate::appearance::parse_hex(&settings.newtab_color).unwrap_or([0x1b, 0x1c, 0x1e]);
                    if ui.color_edit_button_srgb(&mut rgb).changed() {
                        settings.newtab_color = crate::appearance::to_hex(rgb);
                        changed = true;
                    }
                });
                ui.add_space(4.0);
            }
            changed |= radio(
                ui,
                &mut settings.newtab_bg,
                "picture",
                "My picture",
                Some("JPG, PNG, WebP or GIF, up to 4 MB."),
            );
            if settings.newtab_bg == "picture" {
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    if crate::appearance::picking() {
                        ui.label("Choosing...");
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
                    } else if ui
                        .button(if crate::appearance::has_picture() { "Change picture" } else { "Choose picture" })
                        .clicked()
                    {
                        pending.push(PageAction::ChoosePicture);
                    }
                });
                if !crate::appearance::has_picture() && !crate::appearance::picking() {
                    ui.label(RichText::new("No picture chosen yet, so the soft glow is shown.").small().weak());
                }
            }
            if let Some(notice) = notice {
                ui.add_space(6.0);
                ui.label(notice);
            }
        },
    );
    card(
        ui,
        "Sidebar",
        Some("Off: the sidebar slides in when you move the mouse to the left edge."),
        "sidebar tabs always show pin",
        |ui| {
            changed |= ui
                .checkbox(&mut settings.sidebar_pinned, "Always show the sidebar")
                .changed();
        },
    );
    changed
}

fn startup_section(ui: &mut egui::Ui, settings: &mut pteron::Settings) -> bool {
    let mut changed = false;
    card(
        ui,
        "When Pteron starts",
        Some("Pinned tabs always come back."),
        "startup start launch homepage restore session new tab",
        |ui| {
            changed |= radio(ui, &mut settings.startup, "newtab", "Open a new tab", None);
            changed |= radio(
                ui,
                &mut settings.startup,
                "restore",
                "Continue where I left off",
                Some(
                    "Tabs come back asleep to save memory: the tab you were on loads right away, \
                     the others load when you click them.",
                ),
            );
            changed |= radio(
                ui,
                &mut settings.startup,
                "custom",
                "Open a page I choose",
                None,
            );
            if settings.startup == "custom" {
                ui.add_space(4.0);
                changed |= ui
                    .add(
                        TextEdit::singleline(&mut settings.homepage_url)
                            .hint_text("https://example.org")
                            .desired_width(f32::INFINITY)
                            .margin(vec2(10.0, 6.0)),
                    )
                    .changed();
            }
        },
    );
    card(
        ui,
        "Tiles on the new tab page",
        Some("The row of site shortcuts under the search box."),
        "new tab tiles shortcuts bookmarks most visited sites frequent",
        |ui| {
            changed |= radio(ui, &mut settings.newtab_tiles, "off", "Show nothing", None);
            changed |= radio(
                ui,
                &mut settings.newtab_tiles,
                "bookmarks",
                "Only my bookmarks",
                Some("Star a page in the sidebar to put it here."),
            );
            changed |= radio(
                ui,
                &mut settings.newtab_tiles,
                "frequent",
                "My bookmarks and the sites I visit most",
                None,
            );
        },
    );
    changed
}

fn tabs_section(ui: &mut egui::Ui, settings: &mut pteron::Settings) -> bool {
    let mut changed = false;
    card(
        ui,
        "Sleeping tabs",
        Some(
            "A sleeping tab frees its memory and shows as \"zZ\" in the sidebar. Click it to wake \
             it; the page reloads. Tabs that are playing audio or video never sleep.",
        ),
        "sleep memory ram tabs idle",
        |ui| {
            let mut enabled = settings.tab_sleep_minutes > 0;
            if ui
                .checkbox(&mut enabled, "Put unused background tabs to sleep")
                .changed()
            {
                settings.tab_sleep_minutes = if enabled { 5 } else { 0 };
                changed = true;
            }
            if enabled {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut settings.tab_sleep_minutes)
                                .range(1..=240)
                                .prefix("Sleep after ")
                                .suffix(" min unused"),
                        )
                        .changed();
                });
            }
        },
    );
    changed
}

fn video_section(ui: &mut egui::Ui, settings: &mut pteron::Settings) -> bool {
    let mut changed = false;
    card(
        ui,
        "YouTube",
        Some(
            "YouTube's own website needs web features Pteron does not have yet. With this on, \
             YouTube search, channels and videos open in simple Pteron pages instead. Videos \
             play up to 1080p (they start at 720p to save memory). Nothing needs to be installed.",
        ),
        "youtube video player watch quality",
        |ui| {
            changed |= ui
                .checkbox(&mut settings.youtube_player, "Browse and play YouTube in Pteron's own pages")
                .changed();
            changed |= ui
                .checkbox(&mut settings.fast_start, "Start long videos sooner (first two minutes at once, then the whole video)")
                .changed();
        },
    );
    card(
        ui,
        "Video decoding",
        Some(
            "Lets your graphics card decode video instead of the processor. Smoother playback, \
             less CPU use and longer battery life. Turn it off if videos show green or broken \
             pictures. Takes effect after you restart Pteron.",
        ),
        "hardware acceleration gpu video decode graphics card battery",
        |ui| {
            changed |= ui
                .checkbox(&mut settings.hw_video_decode, "Use hardware video decoding")
                .changed();
        },
    );
    changed
}

fn compat_section(ui: &mut egui::Ui, settings: &mut pteron::Settings) -> bool {
    let mut changed = false;
    card(
        ui,
        "Newer web features",
        Some(
            "Servo, the engine inside Pteron, has these features built in but switched off \
             because they are still being finished. Turning one on can make a site work that \
             did not, but it can also make a page slower or crash. Pages reload when you change \
             a switch. If Pteron does not close properly, these stay off for the next session.",
        ),
        "compatibility experimental features indexeddb intersection observer fontface canvas storage container queries web apps",
        |ui| {
            if crate::compat::safe_mode() && !settings.compat.is_empty() {
                ui.label(
                    RichText::new("Pteron did not close properly last time, so these are off for this session. Restart Pteron to use them again.")
                        .color(ui.visuals().warn_fg_color),
                );
                ui.add_space(6.0);
            }
            for feature in crate::compat::FEATURES {
                let mut on = settings.compat.iter().any(|id| id == feature.id);
                let response = ui.checkbox(&mut on, feature.title).on_hover_text(feature.note);
                ui.label(RichText::new(feature.note).small().weak());
                ui.add_space(4.0);
                if response.changed() {
                    settings.compat.retain(|id| id != feature.id);
                    if on {
                        settings.compat.push(feature.id.to_string());
                    }
                    changed = true;
                }
            }
        },
    );
    changed
}

fn privacy_section(
    ui: &mut egui::Ui,
    pending: &mut Vec<PageAction>,
    notice: &Option<String>,
    settings: &mut pteron::Settings,
    changed: &mut bool,
) {
    card(
        ui,
        "History",
        Some(
            "Remembered pages power the suggestions under the address bar and the shortcuts \
             on the new tab page. Everything stays on this computer.",
        ),
        "history privacy suggestions remember visited pages",
        |ui| {
            *changed |= ui
                .checkbox(&mut settings.history_enabled, "Remember the pages I visit")
                .changed();
            ui.add_space(6.0);
            if ui.button("Clear browsing history").clicked() {
                pending.push(PageAction::ClearHistory);
            }
        },
    );
    card(
        ui,
        "Passwords",
        Some(
            "Pteron asks before saving a login, and fills it only when you click. Saved passwords \
             are locked with your Windows account, kept only on this computer, and never synced.",
        ),
        "passwords logins saved autofill sign in remember",
        |ui| {
            let mut on = crate::passwords::enabled();
            if ui.checkbox(&mut on, "Offer to save passwords").changed() {
                crate::passwords::set_enabled(on);
            }
            let logins = crate::passwords::list();
            ui.add_space(6.0);
            if logins.is_empty() {
                ui.label("No saved logins yet.");
            }
            let mut remove = None;
            for (index, (site, user)) in logins.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(if user.is_empty() { site.clone() } else { format!("{site} - {user}") });
                    if ui.small_button("Delete").clicked() {
                        remove = Some(index);
                    }
                });
            }
            if let Some(index) = remove {
                crate::passwords::delete(index);
            }
            if logins.len() > 1 && ui.button("Delete all saved logins").clicked() {
                crate::passwords::delete_all();
            }
            let never = crate::passwords::never_list();
            if !never.is_empty() {
                ui.add_space(8.0);
                ui.label("Never ask on:");
                let mut allow = None;
                for (index, site) in never.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(site);
                        if ui.small_button("Ask again").clicked() {
                            allow = Some(index);
                        }
                    });
                }
                if let Some(index) = allow {
                    crate::passwords::allow_again(index);
                }
            }
        },
    );
    card(
        ui,
        "Sync between computers",
        Some(
            "Keeps your bookmarks and settings the same on two computers through a folder that \
             both have, such as one inside OneDrive, Google Drive or Dropbox. Pteron only writes \
             one small file there. History, passwords and cookies are never synced.",
        ),
        "sync synchronize devices computers phone laptop bookmarks settings onedrive drive dropbox backup",
        |ui| {
            if settings.sync_folder.trim().is_empty() {
                if crate::sync::choosing() {
                    ui.label("Choosing...");
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
                } else if ui.button("Choose sync folder").clicked() {
                    pending.push(PageAction::ChooseSyncFolder);
                }
            } else {
                ui.label(RichText::new(format!("Folder: {}", settings.sync_folder)).weak());
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Sync now").clicked() {
                        pending.push(PageAction::SyncNow);
                    }
                    if ui.button("Change folder").clicked() {
                        pending.push(PageAction::ChooseSyncFolder);
                    }
                    if ui.button("Stop syncing").clicked() {
                        pending.push(PageAction::StopSync);
                    }
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Changes are saved to the folder as you make them. Another computer's changes are picked up when Pteron starts, or when you press Sync now.")
                        .small()
                        .weak(),
                );
            }
        },
    );
    card(
        ui,
        "Secure connections",
        Some("Sites that use plain http can be read and changed by others on the same network."),
        "https http secure security warning connection",
        |ui| {
            *changed |= ui
                .checkbox(&mut settings.https_warning, "Warn me before opening http sites")
                .changed();
        },
    );
    card(
        ui,
        "Clear on exit",
        Some("Forget what you looked at each time you close Pteron."),
        "clear exit close history cache privacy forget",
        |ui| {
            *changed |= ui
                .checkbox(&mut settings.clear_on_exit, "Clear history and cache when I close Pteron")
                .changed();
            ui.add_space(2.0);
            ui.label(RichText::new("Bookmarks, settings and sign-ins (cookies) are kept.").small().weak());
        },
    );
    card(
        ui,
        "Ads and trackers",
        Some("Cancels requests to well-known advertising and tracking networks. Pages load faster and use less memory."),
        "ads adblock ad blocker tracker tracking privacy block",
        |ui| {
            *changed |= ui
                .checkbox(&mut settings.adblock, "Block ads and trackers")
                .changed();
            let count = crate::adblock::blocked_count();
            if count > 0 {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Blocked {count} requests since Pteron started.")).weak());
            }
            ui.add_space(2.0);
            ui.label(RichText::new("If a page looks broken, turn this off and reload.").small().weak());
        },
    );
    card(
        ui,
        "Bookmarks",
        Some(
            "Move your bookmarks between browsers or computers. Export saves Pteron-bookmarks.html \
             on the Desktop. To import, export a bookmark file from Chrome, Edge or Firefox into \
             Downloads or the Desktop, then press Import.",
        ),
        "bookmarks import export chrome firefox edge transfer move",
        |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Export bookmarks").clicked() {
                    pending.push(PageAction::ExportBookmarks);
                }
                if ui.button("Import bookmarks").clicked() {
                    pending.push(PageAction::ImportBookmarks);
                }
            });
        },
    );
    card(
        ui,
        "Cookies, cache and saved tabs",
        None,
        "privacy clear cookies cache data forget session",
        |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Clear cookies and site data").clicked() {
                    pending.push(PageAction::ClearCookies);
                }
                if ui.button("Clear cache").clicked() {
                    pending.push(PageAction::ClearCache);
                }
                if ui.button("Forget saved tabs").clicked() {
                    pending.push(PageAction::ForgetSession);
                }
            });
            if let Some(notice) = notice {
                ui.add_space(8.0);
                ui.label(notice);
            }
        },
    );
}

/// Returns true when the user asked to reset all settings.
/// "Updates": look for a newer release on GitHub, download it, restart into it.
fn updates_card(ui: &mut egui::Ui, pending: &mut Vec<PageAction>) {
    static LAST_ERROR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    if !updater::supported() {
        card(
            ui,
            "Updates",
            Some("Pteron updates itself on Windows. On this system, update it the way you installed it."),
            "update updates upgrade version check new",
            |_| {},
        );
        return;
    }
    updater::check_once();
    let status = updater::status();
    if updater::busy() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(400));
    }
    card(
        ui,
        "Updates",
        Some("Pteron looks for new versions on GitHub when you open this page. Nothing is installed without your say-so."),
        "update updates upgrade version check new download install",
        |ui| {
            if let Some(error) = LAST_ERROR.get_or_init(updater::take_last_error) {
                ui.label(RichText::new(format!("The last update did not finish: {error}")).color(Color32::from_rgb(214, 120, 90)));
                ui.add_space(6.0);
            }
            if let Some(error) = updater::install_error() {
                ui.label(RichText::new(error).color(Color32::from_rgb(214, 120, 90)));
                ui.add_space(6.0);
            }
            match status {
                updater::Status::Idle | updater::Status::Checking => {
                    ui.label("Looking for updates...");
                },
                updater::Status::UpToDate => {
                    ui.label(format!("You have the newest version (build {}).", updater::BUILD));
                    ui.add_space(8.0);
                    if ui.button("Check again").clicked() {
                        updater::check_async();
                    }
                },
                updater::Status::Available(release) => {
                    ui.label(RichText::new(format!("{} is available (you have build {}).", release.name, updater::BUILD)).strong());
                    if !release.notes.trim().is_empty() {
                        ui.add_space(4.0);
                        ui.label(RichText::new(release.notes.trim()).weak());
                    }
                    ui.add_space(8.0);
                    if ui.button("Download update").clicked() {
                        updater::download_async(release);
                    }
                },
                updater::Status::Downloading { done, total } => {
                    let fraction = if total > 0 { (done as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
                    ui.label(format!("Downloading... {} of {} MB", done / 1_000_000, total / 1_000_000));
                    ui.add_space(6.0);
                    ui.add(egui::ProgressBar::new(fraction).desired_width(320.0));
                },
                updater::Status::Ready(_) => {
                    ui.label(RichText::new("The update is downloaded and checked.").strong());
                    ui.label("Pteron will close, install it and open again. Your tabs come back.");
                    ui.add_space(8.0);
                    if ui.button("Restart and update").clicked() {
                        pending.push(PageAction::InstallUpdate);
                    }
                },
                updater::Status::Failed(message) => {
                    ui.label(RichText::new(message).color(Color32::from_rgb(214, 120, 90)));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Try again").clicked() {
                            updater::check_async();
                        }
                        if ui
                            .button("Open the download page")
                            .on_hover_text("Download the installer yourself and run it")
                            .clicked()
                        {
                            pending.push(PageAction::OpenDownloadPage);
                        }
                    });
                },
            }
        },
    );
}

fn about_section(
    ui: &mut egui::Ui,
    pending: &mut Vec<PageAction>,
    report_note: &mut String,
    notice: &Option<String>,
) -> bool {
    let mut reset = false;
    card(
        ui,
        pteron::APP_NAME,
        Some("Rendering engine: Servo (MPL-2.0)"),
        "about version servo",
        |ui| {
            ui.label(format!("Version 0.{}", updater::BUILD));
        },
    );
    updates_card(ui, pending);
    card(
        ui,
        "Report a problem",
        Some("Saves a small file on your Desktop (versions, your computer, your settings and the newest log). Pteron sends nothing by itself: you send the file to whoever gave you Pteron."),
        "report problem bug feedback issue help known",
        |ui| {
            ui.label("What went wrong? (optional)");
            ui.add(
                egui::TextEdit::multiline(report_note)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save report to Desktop").clicked() {
                    pending.push(PageAction::SaveReport(report_note.clone()));
                }
                if ui.button("Copy details").clicked() {
                    pending.push(PageAction::CopyReport(report_note.clone()));
                }
                if ui.button("Known problems").clicked() {
                    pending.push(PageAction::OpenKnownIssues);
                }
            });
            if let Some(text) = notice &&
                (text.starts_with("Saved:") || text.starts_with("Could not save") || text.starts_with("Copied"))
            {
                ui.add_space(6.0);
                ui.label(RichText::new(text).small());
            }
        },
    );
    card(
        ui,
        "Settings file",
        Some("Your settings are stored here."),
        "reset settings file default",
        |ui| {
            ui.label(RichText::new(pteron::settings_file_path()).small());
            ui.add_space(8.0);
            reset = ui.button("Reset all settings to default").clicked();
        },
    );
    reset
}
