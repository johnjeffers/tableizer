//! The app's whole visual system in one place: a light/dark theme that can follow the OS, a
//! user-selectable accent color and layout density. [`build`] turns [`Settings`] into an egui
//! [`Style`] (spacing, rounding, widget colors) plus a [`Palette`] of the extra colors the custom
//! grid/toolbar painting needs. Retheme by editing this module.

use eframe::egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Theme, Vec2,
    style::Spacing,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// App-wide named text styles. Their sizes are registered in `text_styles` by [`build`], so call
/// sites select a style by name and no font size is hard-coded outside this module.
pub const MENU_SECTION: &str = "menu_section";
pub const MENU_ITEM: &str = "menu_item";
pub const SETTINGS_HEADING: &str = "settings_heading";
pub const COLUMN_HEADER: &str = "column_header";
pub const STEPPER: &str = "stepper";

/// The [`TextStyle`] for one of the named styles above (e.g. [`MENU_SECTION`]).
pub fn text_style(name: &str) -> TextStyle {
    TextStyle::Name(name.into())
}

/// Light/dark selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// Follow the OS light/dark setting.
    #[default]
    Auto,
    Light,
    Dark,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Auto, Mode::Light, Mode::Dark];
    pub fn label(self) -> &'static str {
        match self {
            Mode::Auto => "Auto (OS)",
            Mode::Light => "Light",
            Mode::Dark => "Dark",
        }
    }
}

/// Accent color for selection, active controls, links and focus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Accent {
    #[default]
    Blue,
    Teal,
    Violet,
    Amber,
}

impl Accent {
    pub const ALL: [Accent; 4] = [Accent::Blue, Accent::Teal, Accent::Violet, Accent::Amber];
    pub fn label(self) -> &'static str {
        match self {
            Accent::Blue => "Blue",
            Accent::Teal => "Teal",
            Accent::Violet => "Violet",
            Accent::Amber => "Amber",
        }
    }
    pub fn color(self) -> Color32 {
        match self {
            Accent::Blue => Color32::from_rgb(56, 124, 246),
            Accent::Teal => Color32::from_rgb(22, 160, 145),
            Accent::Violet => Color32::from_rgb(138, 104, 244),
            Accent::Amber => Color32::from_rgb(228, 150, 42),
        }
    }
}

/// Layout density.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

impl Density {
    pub const ALL: [Density; 2] = [Density::Compact, Density::Comfortable];
    pub fn label(self) -> &'static str {
        match self {
            Density::Comfortable => "Comfortable",
            Density::Compact => "Compact",
        }
    }
}

/// Persisted, user-editable theme settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub accent: Accent,
    #[serde(default)]
    pub density: Density,
    /// Data-cell font family (a system font name); `None` = use the app font.
    #[serde(default)]
    pub table_font: Option<String>,
    /// Data-cell font size, in points.
    #[serde(default = "default_table_font_size")]
    pub table_font_size: f32,
}

fn default_table_font_size() -> f32 {
    13.5
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            accent: Accent::default(),
            density: Density::default(),
            table_font: None,
            table_font_size: default_table_font_size(),
        }
    }
}

/// Extra colors + metrics the custom grid/toolbar painting needs (beyond the egui [`Style`]).
#[derive(Clone)]
pub struct Palette {
    pub accent: Color32,
    pub header_bg: Color32,
    pub header_text: Color32,
    pub row_selected: Color32,
    pub row_hover: Color32,
    pub search_match: Color32,
    pub stripe: Color32,
    pub warning: Color32,
    pub error: Color32,
    pub border: Color32,
    pub row_height: f32,
    pub header_height: f32,
    /// Font used for data cells (the user-chosen family + size).
    pub table_font: FontId,
}

/// Whether `settings` resolves to dark, given the OS preference.
pub fn is_dark(settings: &Settings, system_dark: bool) -> bool {
    match settings.mode {
        Mode::Auto => system_dark,
        Mode::Light => false,
        Mode::Dark => true,
    }
}

fn with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// Build the egui [`Style`] + [`Palette`] for `settings` under the given OS theme.
pub fn build(settings: &Settings, system_dark: bool) -> (Style, Palette) {
    let dark = is_dark(settings, system_dark);
    let accent = settings.accent.color();
    let comfortable = settings.density == Density::Comfortable;

    let mut visuals = if dark { Theme::Dark } else { Theme::Light }.default_visuals();
    // Accent wherever egui draws selection / focus / links.
    visuals.selection.bg_fill = with_alpha(accent, if dark { 110 } else { 90 });
    visuals.selection.stroke = Stroke::new(1.0, accent);
    visuals.hyperlink_color = accent;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, with_alpha(accent, 160));
    // Consistent, subtle rounding everywhere.
    let radius = CornerRadius::same(5);
    visuals.widgets.noninteractive.corner_radius = radius;
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.active.corner_radius = radius;
    visuals.widgets.open.corner_radius = radius;
    visuals.window_corner_radius = CornerRadius::same(8);
    visuals.menu_corner_radius = CornerRadius::same(6);

    // Keep all metrics on the integer pixel grid — fractional spacing renders blurry text and
    // trips egui's "unaligned" debug overlay.
    let scale = if comfortable { 1.0 } else { 0.65 };
    let round = |v: f32| (v * scale).round();
    let spacing = Spacing {
        item_spacing: Vec2::new(round(8.0), round(6.0)),
        button_padding: Vec2::new(round(8.0), round(4.0)),
        menu_margin: Margin::same(round(6.0) as i8),
        window_margin: Margin::same(round(8.0) as i8),
        interact_size: Vec2::new(
            Spacing::default().interact_size.x,
            if comfortable { 24.0 } else { 20.0 },
        ),
        ..Spacing::default()
    };

    let text_styles = BTreeMap::from([
        (
            TextStyle::Small,
            FontId::new(11.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
        // Named chrome styles — the single source of truth for these font sizes.
        (
            text_style(MENU_SECTION),
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            text_style(MENU_ITEM),
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            text_style(SETTINGS_HEADING),
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            text_style(COLUMN_HEADER),
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            text_style(STEPPER),
            FontId::new(12.0, FontFamily::Proportional),
        ),
    ]);
    let style = Style {
        visuals,
        spacing,
        text_styles,
        ..Style::default()
    };

    // Row height tracks the data-cell font size + density padding, so large fonts don't clip.
    let row_height = (settings.table_font_size + if comfortable { 12.0 } else { 7.0 }).round();

    let palette = Palette {
        accent,
        header_bg: if dark {
            Color32::from_gray(38)
        } else {
            Color32::from_gray(236)
        },
        header_text: if dark {
            Color32::from_gray(165)
        } else {
            Color32::from_gray(95)
        },
        row_selected: with_alpha(accent, if dark { 70 } else { 55 }),
        row_hover: with_alpha(accent, if dark { 28 } else { 20 }),
        search_match: Color32::from_rgba_unmultiplied(250, 205, 70, if dark { 50 } else { 95 }),
        stripe: if dark {
            Color32::from_white_alpha(8)
        } else {
            Color32::from_black_alpha(10)
        },
        warning: Color32::from_rgb(235, 165, 50),
        error: if dark {
            Color32::from_rgb(240, 110, 110)
        } else {
            Color32::from_rgb(200, 50, 50)
        },
        border: if dark {
            Color32::from_white_alpha(24)
        } else {
            Color32::from_black_alpha(28)
        },
        row_height,
        // Keep the header at least as tall as a row (so big fonts don't make rows overshoot it).
        header_height: row_height.max(if comfortable { 30.0 } else { 24.0 }),
        table_font: FontId::new(
            settings.table_font_size,
            FontFamily::Name(crate::fonts::TABLE_FONT.into()),
        ),
    };
    (style, palette)
}
