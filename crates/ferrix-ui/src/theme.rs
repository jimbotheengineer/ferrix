//! Dark theme tuned for long sessions staring at dense numeric grids.

use egui::{Color32, Rounding, Stroke, Visuals};

pub struct Theme;

impl Theme {
    pub const BG: Color32 = Color32::from_rgb(0x12, 0x14, 0x18);
    pub const PANEL: Color32 = Color32::from_rgb(0x17, 0x1a, 0x1f);
    pub const HEADER_BG: Color32 = Color32::from_rgb(0x1d, 0x21, 0x27);
    pub const GRID_LINE: Color32 = Color32::from_rgb(0x26, 0x2b, 0x33);
    pub const TEXT: Color32 = Color32::from_rgb(0xd6, 0xdb, 0xe3);
    pub const TEXT_DIM: Color32 = Color32::from_rgb(0x7a, 0x84, 0x94);
    pub const ACCENT: Color32 = Color32::from_rgb(0x4d, 0x9d, 0xff);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0x1e, 0x33, 0x4d);
    pub const NUMBER: Color32 = Color32::from_rgb(0x8f, 0xd0, 0xa8);
    pub const ERROR: Color32 = Color32::from_rgb(0xff, 0x7b, 0x72);
    pub const ROW_ALT: Color32 = Color32::from_rgb(0x15, 0x18, 0x1d);
    /// Search-match fill: warm enough to read against the dark grid without
    /// competing with the blue selection.
    pub const MATCH_BG: Color32 = Color32::from_rgb(0x4a, 0x3d, 0x1a);
    pub const MATCH_CURRENT: Color32 = Color32::from_rgb(0x7a, 0x5f, 0x1e);
    pub const MATCH_EDGE: Color32 = Color32::from_rgb(0xf0, 0xc0, 0x50);
    /// Fill for cells inside a multi-cell selection. Translucent so zebra
    /// striping and search highlights stay visible underneath.
    pub const RANGE_FILL: Color32 = Color32::from_rgba_premultiplied(0x2a, 0x3f, 0x5f, 0x90);
    /// Banded-row fill inside a structured table. Distinct from `ROW_ALT` so
    /// the table's own striping is visible against the sheet's.
    pub const TABLE_BAND: Color32 = Color32::from_rgb(0x1a, 0x20, 0x2a);
    /// The corner triangle marking a cell that fails its column's validation.
    /// Deliberately loud: the whole point is that bad data is impossible to
    /// miss rather than silently dropped.
    pub const INVALID_FLAG: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4f);

    pub fn apply(ctx: &egui::Context) {
        let mut v = Visuals::dark();
        v.panel_fill = Self::PANEL;
        v.window_fill = Self::BG;
        v.extreme_bg_color = Self::BG;
        v.override_text_color = Some(Self::TEXT);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, Self::GRID_LINE);
        v.widgets.inactive.bg_fill = Self::HEADER_BG;
        v.widgets.hovered.bg_fill = Self::ACCENT_SOFT;
        v.widgets.active.bg_fill = Self::ACCENT_SOFT;
        v.selection.bg_fill = Self::ACCENT_SOFT;
        v.selection.stroke = Stroke::new(1.0_f32, Self::ACCENT);
        v.widgets.noninteractive.rounding = Rounding::same(2.0);
        ctx.set_visuals(v);

        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        ctx.set_style(style);
    }
}
