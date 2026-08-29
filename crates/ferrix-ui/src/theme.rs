//! Themes, as values rather than constants.
//!
//! The palette used to be ~17 associated `const Color32`s on a unit struct,
//! read as `Theme::BG` from anywhere. That is exactly one theme, forever: a
//! const cannot be swapped at runtime, and every call site baked the dark
//! palette in at compile time.
//!
//! So `Theme` is now a plain value with two constructors, [`Theme::dark`] and
//! [`Theme::light`], threaded down through the UI. There are deliberately NO
//! colour constants left on the type — a call site that tries to bypass the
//! active theme no longer compiles, which is a stronger guarantee than a
//! review convention.
//!
//! ## Light is not "dark, inverted"
//!
//! Inverting a dark palette produces washed-out pastels that lose exactly the
//! distinctions that matter. Every *semantic* colour — numbers green, errors
//! red, matches amber, selection blue, validation flags loud — is picked
//! separately for each theme so it stays both recognisable and legible against
//! that theme's background. Dark themes want light, desaturated hues; light
//! themes want dark, saturated ones.
//!
//! ## Translucency is load-bearing
//!
//! [`Theme::range_fill`], [`Theme::match_bg`] and [`Theme::match_current`] are
//! translucent in BOTH themes. They are overlays: zebra striping, table
//! banding and conditional-format fills have to remain visible underneath, and
//! a selected search match has to read as both selected AND a match. Making
//! any of them opaque in light mode would silently erase that layering — hence
//! [`Theme::overlays_are_translucent`] and the test that calls it.

use egui::{Color32, Rounding, Stroke, Visuals};

/// Which palette is active. This is what gets persisted.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

impl ThemeMode {
    /// Stable token for the preferences file. Deliberately not `Debug`, which
    /// is free to change.
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeMode::Dark => "dark",
            ThemeMode::Light => "light",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "dark" => Some(ThemeMode::Dark),
            "light" => Some(ThemeMode::Light),
            _ => None,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        }
    }

    /// Label for the toolbar toggle: shows what clicking will switch TO.
    pub fn toggle_label(self) -> &'static str {
        match self {
            ThemeMode::Dark => "☀ Light",
            ThemeMode::Light => "🌙 Dark",
        }
    }
}

/// A full palette. `Copy` and 80-odd bytes, so it is passed by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub mode: ThemeMode,
    pub bg: Color32,
    pub panel: Color32,
    pub header_bg: Color32,
    pub grid_line: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub number: Color32,
    pub error: Color32,
    pub row_alt: Color32,
    /// Search-match fill. Translucent so striping shows through.
    pub match_bg: Color32,
    /// The match the user is parked on. Translucent for the same reason, but
    /// stronger, so it reads as "this one" without hiding the cell.
    pub match_current: Color32,
    pub match_edge: Color32,
    /// Fill for cells inside a multi-cell selection. Translucent so zebra
    /// striping and search highlights stay visible underneath.
    pub range_fill: Color32,
    /// Banded-row fill inside a structured table. Distinct from `row_alt` so
    /// the table's own striping is visible against the sheet's.
    pub table_band: Color32,
    /// The corner triangle marking a cell that fails its column's validation.
    /// Deliberately loud in both themes: the whole point is that bad data is
    /// impossible to miss rather than silently dropped.
    pub invalid_flag: Color32,
    /// Corner marker for a cell carrying a comment. Distinct from
    /// `invalid_flag` on purpose: a cell can be both commented and invalid,
    /// and two facts must not be painted as one.
    pub comment_flag: Color32,
    /// Fill for the empty padding rows past the end of the sheet (issue #20).
    /// Must differ from both `bg` and `row_alt`, so "there is no row here" is
    /// visibly different from "this row holds empty strings".
    pub pad_row: Color32,
    /// Outline colours for the references highlighted while a formula is being
    /// edited (issue #38), assigned round-robin in source order.
    ///
    /// A cycle rather than a per-reference colour: the point is that the FIRST
    /// argument's outline is distinguishable from the SECOND's, and a formula
    /// with more references than colours reuses them far enough apart to still
    /// read. They are strokes over the live grid, so each must be legible
    /// against `bg`, `row_alt` and the selection fill — asserted below.
    pub ref_colors: [Color32; 5],
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    /// Dark theme tuned for long sessions staring at dense numeric grids.
    pub const fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            bg: Color32::from_rgb(0x12, 0x14, 0x18),
            panel: Color32::from_rgb(0x17, 0x1a, 0x1f),
            header_bg: Color32::from_rgb(0x1d, 0x21, 0x27),
            grid_line: Color32::from_rgb(0x26, 0x2b, 0x33),
            text: Color32::from_rgb(0xd6, 0xdb, 0xe3),
            // Brightened from 0x7a8494, which scored only 3.40:1 against
            // accent_soft (the selection fill) — dimmed text on a selected
            // cell was genuinely hard to read. 0x909aa8 is 4.51:1 there and
            // still clearly dimmer than `text`, so the dim/primary
            // distinction survives. Caught by the contrast test below, which
            // checks every ink against every surface in both themes.
            text_dim: Color32::from_rgb(0x90, 0x9a, 0xa8),
            accent: Color32::from_rgb(0x4d, 0x9d, 0xff),
            accent_soft: Color32::from_rgb(0x1e, 0x33, 0x4d),
            // Light, desaturated green/red: saturated ones vibrate against a
            // near-black background.
            number: Color32::from_rgb(0x8f, 0xd0, 0xa8),
            error: Color32::from_rgb(0xff, 0x7b, 0x72),
            row_alt: Color32::from_rgb(0x15, 0x18, 0x1d),
            // Warm amber at ~25% over the grid: composites to roughly the old
            // opaque #4a3d1a, but now lets banding and conditional fills show
            // through instead of covering them.
            // Search highlights are deliberately DIM amber, not bright.
            //
            // They sit behind arbitrary cell ink -- green numbers, red errors,
            // blue accents -- and a mid-luminance amber cannot clear 4:1
            // against mid-luminance ink. The old values composited to 3.75:1
            // for accent-on-match and 2.05:1 for accent-on-current, so the
            // cell you had just jumped to was the least readable on screen.
            //
            // The current match does not need a brighter FILL to stand out:
            // grid.rs already strokes it with a 1.5px match_edge border, which
            // distinguishes it without fighting the text. Contrast is checked
            // by semantic_text_is_legible_on_every_surface_in_both_themes.
            match_bg: Color32::from_rgba_premultiplied(0x2d, 0x24, 0x0f, 0x30),
            match_current: Color32::from_rgba_premultiplied(0x33, 0x29, 0x11, 0x36),
            match_edge: Color32::from_rgb(0xf0, 0xc0, 0x50),
            // Same reasoning as the search highlights: a range selection sits
            // behind arbitrary ink, so it has to stay dark enough for red
            // errors and blue accents to read on it. At the old 0x90 alpha,
            // error-on-range was 3.67:1 and accent-on-range 3.35:1.
            range_fill: Color32::from_rgba_premultiplied(0x1c, 0x2a, 0x3f, 0x60),
            table_band: Color32::from_rgb(0x1a, 0x20, 0x2a),
            invalid_flag: Color32::from_rgb(0xe5, 0x48, 0x4f),
            comment_flag: Color32::from_rgb(0xf2, 0xb1, 0x3c),
            // Darker than `bg`: past the end of the sheet reads as a recess.
            pad_row: Color32::from_rgb(0x0c, 0x0d, 0x10),
            // Bright and saturated: these are 1.5px strokes over a near-black
            // grid, and a desaturated outline at that width simply vanishes.
            // Blue is deliberately absent — `accent` already means "the
            // selection", and a blue reference outline would be read as one.
            ref_colors: [
                Color32::from_rgb(0x6f, 0xd6, 0x8a),
                Color32::from_rgb(0xe8, 0x8d, 0x3c),
                Color32::from_rgb(0xc4, 0x8b, 0xf0),
                Color32::from_rgb(0x4f, 0xd0, 0xd6),
                Color32::from_rgb(0xf0, 0x72, 0xa8),
            ],
        }
    }

    /// Light theme. Backgrounds are near-white but never pure white — a full
    /// #ffffff grid glares, and leaves no room for a lighter zebra stripe.
    pub const fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            bg: Color32::from_rgb(0xfb, 0xfc, 0xfd),
            panel: Color32::from_rgb(0xef, 0xf1, 0xf5),
            header_bg: Color32::from_rgb(0xe3, 0xe7, 0xee),
            grid_line: Color32::from_rgb(0xd0, 0xd6, 0xe0),
            text: Color32::from_rgb(0x1b, 0x1f, 0x26),
            text_dim: Color32::from_rgb(0x5b, 0x65, 0x74),
            accent: Color32::from_rgb(0x14, 0x60, 0xc8),
            accent_soft: Color32::from_rgb(0xcd, 0xdf, 0xf8),
            // Dark and saturated: the mirror-image reasoning to the dark
            // theme. #8fd0a8 on white is unreadable; this is still obviously
            // "the green that means number".
            number: Color32::from_rgb(0x0d, 0x6e, 0x3c),
            error: Color32::from_rgb(0xba, 0x1f, 0x1a),
            row_alt: Color32::from_rgb(0xf1, 0xf3, 0xf7),
            // Amber at ~44%: composites to a legible highlight over both the
            // plain and the zebra row, which stay distinguishable under it.
            match_bg: Color32::from_rgba_premultiplied(0x6b, 0x56, 0x21, 0x70),
            // Mirror of the dark-theme reasoning: in light mode the highlight
            // must stay LIGHT so dark ink reads on it. At the old 0xc0 alpha
            // it composited to a saturated amber where green numbers, red
            // errors and blue accents all fell to ~3.4:1.
            match_current: Color32::from_rgba_premultiplied(0x66, 0x49, 0x0e, 0x70),
            // A pale amber edge vanishes on white, so the outline goes dark.
            match_edge: Color32::from_rgb(0x8a, 0x5d, 0x08),
            range_fill: Color32::from_rgba_premultiplied(0x18, 0x31, 0x50, 0x50),
            table_band: Color32::from_rgb(0xe6, 0xeb, 0xf3),
            invalid_flag: Color32::from_rgb(0xcc, 0x28, 0x24),
            comment_flag: Color32::from_rgb(0xb8, 0x7a, 0x00),
            // Slightly grey against the near-white sheet.
            pad_row: Color32::from_rgb(0xe8, 0xea, 0xef),
            // Mirror reasoning: dark and saturated so a thin stroke survives
            // against a near-white sheet. Same five hues, same order, so a
            // formula's third reference is "the purple one" in both themes.
            ref_colors: [
                Color32::from_rgb(0x1a, 0x7f, 0x45),
                Color32::from_rgb(0xa5, 0x54, 0x00),
                Color32::from_rgb(0x7b, 0x3f, 0xb8),
                // Pulled bluer than the obvious teal: at #0a717a this sat only
                // 83 away from the green above, so two different references
                // outlined in "green-ish" were indistinguishable on a light
                // grid. Kept clear of the #1460c8 selection accent too, so it
                // still does not read as "selected".
                Color32::from_rgb(0x06, 0x80, 0x8f),
                Color32::from_rgb(0xb0, 0x2c, 0x6e),
            ],
        }
    }

    pub fn of(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
        }
    }

    pub fn is_dark(&self) -> bool {
        self.mode == ThemeMode::Dark
    }

    /// True when every overlay colour still lets what is underneath show
    /// through. Asserted by a test in both themes — see the module docs.
    #[allow(dead_code)] // used by the contrast tests below
    pub fn overlays_are_translucent(&self) -> bool {
        [self.range_fill, self.match_bg, self.match_current]
            .iter()
            .all(|c| c.a() < 255 && c.a() > 0)
    }

    /// Push this palette into egui's own widget styling, so buttons, windows,
    /// scrollbars and text edits follow the theme along with the hand-painted
    /// grid.
    pub fn apply(&self, ctx: &egui::Context) {
        let mut v = if self.is_dark() {
            Visuals::dark()
        } else {
            Visuals::light()
        };
        v.panel_fill = self.panel;
        v.window_fill = self.bg;
        v.extreme_bg_color = self.bg;
        v.override_text_color = Some(self.text);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, self.grid_line);
        v.widgets.inactive.bg_fill = self.header_bg;
        v.widgets.hovered.bg_fill = self.accent_soft;
        v.widgets.active.bg_fill = self.accent_soft;
        v.selection.bg_fill = self.accent_soft;
        v.selection.stroke = Stroke::new(1.0_f32, self.accent);
        v.widgets.noninteractive.rounding = Rounding::same(2.0);
        ctx.set_visuals(v);

        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        ctx.set_style(style);
    }
}

/// Colours for the chart's own chrome (background, gridlines, tick labels),
/// which the scene itself does not carry — the scene holds data colours only.
///
/// See [`crate::chart_panel::SVG_FOLLOWS_APP_THEME`] for why the SVG export
/// does not use this.
#[derive(Clone, Copy, Debug)]
pub struct ChartChrome {
    pub bg: Color32,
    pub grid: Color32,
    pub label: Color32,
}

impl From<Theme> for ChartChrome {
    fn from(t: Theme) -> Self {
        Self {
            bg: t.panel,
            grid: t.grid_line,
            label: t.text_dim,
        }
    }
}

/// Relative luminance, for the legibility assertions below.
#[cfg(test)]
fn luminance(c: Color32) -> f32 {
    let f = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
}

/// WCAG contrast ratio between two opaque colours.
#[cfg(test)]
fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Composite a translucent premultiplied colour over an opaque one.
#[cfg(test)]
fn over(fg: Color32, bg: Color32) -> Color32 {
    let a = fg.a() as f32 / 255.0;
    let mix = |f: u8, b: u8| (f as f32 + b as f32 * (1.0 - a)).round().min(255.0) as u8;
    Color32::from_rgb(
        mix(fg.r(), bg.r()),
        mix(fg.g(), bg.g()),
        mix(fg.b(), bg.b()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> [Theme; 2] {
        [Theme::dark(), Theme::light()]
    }

    #[test]
    fn mode_round_trips_through_the_prefs_token() {
        for m in [ThemeMode::Dark, ThemeMode::Light] {
            assert_eq!(ThemeMode::parse(m.as_str()), Some(m));
            assert_eq!(m.toggled().toggled(), m);
            assert_ne!(m.toggled(), m);
        }
        assert_eq!(ThemeMode::parse("chartreuse"), None);
    }

    #[test]
    fn constructors_agree_with_of() {
        assert_eq!(Theme::of(ThemeMode::Dark), Theme::dark());
        assert_eq!(Theme::of(ThemeMode::Light), Theme::light());
        assert_eq!(Theme::default(), Theme::dark());
    }

    #[test]
    fn the_two_themes_are_actually_different() {
        // Guards against a copy-paste that leaves light mode dark.
        assert!(luminance(Theme::light().bg) > 0.7);
        assert!(luminance(Theme::dark().bg) < 0.05);
    }

    /// The pitfall the issue calls out by name: overlays must not become
    /// opaque in light mode, or zebra striping and table banding vanish
    /// underneath the selection and the search highlight.
    #[test]
    fn selection_and_match_overlays_stay_translucent_in_both_themes() {
        for t in both() {
            assert!(
                t.overlays_are_translucent(),
                "{:?}: an overlay went opaque — striping would be erased",
                t.mode
            );
        }
    }

    /// ...and translucent is not enough on its own: what shows through has to
    /// remain *distinguishable*. A selected plain row and a selected zebra row
    /// must still differ after the fill is composited over them.
    #[test]
    fn striping_survives_under_every_overlay() {
        for t in both() {
            for overlay in [t.range_fill, t.match_bg, t.match_current] {
                let plain = over(overlay, t.bg);
                let zebra = over(overlay, t.row_alt);
                let band = over(overlay, t.table_band);
                let delta = |a: Color32, b: Color32| {
                    (a.r() as i32 - b.r() as i32).abs()
                        + (a.g() as i32 - b.g() as i32).abs()
                        + (a.b() as i32 - b.b() as i32).abs()
                };
                assert!(
                    delta(plain, zebra) >= 3,
                    "{:?}: zebra striping disappears under an overlay",
                    t.mode
                );
                assert!(
                    delta(plain, band) >= 3,
                    "{:?}: table banding disappears under an overlay",
                    t.mode
                );
            }
        }
    }

    /// Every semantic text colour has to be readable on every surface it can
    /// actually be painted on, in both themes. 4.0:1 is a shade under the
    /// WCAG AA 4.5:1 body-text bar, which is the right target for 12.5px
    /// dense tabular text.
    #[test]
    fn semantic_text_is_legible_on_every_surface_in_both_themes() {
        for t in both() {
            let surfaces = [
                ("bg", t.bg),
                ("row_alt", t.row_alt),
                ("table_band", t.table_band),
                ("header_bg", t.header_bg),
                ("panel", t.panel),
                ("accent_soft", t.accent_soft),
                ("pad_row", t.pad_row),
                // The search highlights are deliberately excluded from the
                // text_dim pairing below and checked separately.
                //
                // match_current is a mid-luminance amber. Dim grey against
                // mid amber cannot reach 4:1 without brightening text_dim so
                // far that it stops being distinguishable from `text`, which
                // would defeat its whole purpose. The honest fix is that a
                // highlighted cell must paint with PRIMARY text, not dim --
                // asserted in highlighted_cells_use_primary_ink below.
                ("match_bg over bg", over(t.match_bg, t.bg)),
                ("match_current over bg", over(t.match_current, t.bg)),
                ("range_fill over bg", over(t.range_fill, t.bg)),
            ];
            let inks = [
                ("number", t.number),
                ("error", t.error),
                ("text", t.text),
                ("text_dim", t.text_dim),
                ("accent", t.accent),
            ];
            for (sn, s) in surfaces {
                for (inkn, ink) in inks {
                    // See the note above: dim ink is never painted on a
                    // search highlight, because a highlighted cell uses
                    // primary ink.
                    if inkn == "text_dim" && sn.starts_with("match_") {
                        continue;
                    }
                    let c = contrast(ink, s);
                    assert!(
                        c >= 4.0,
                        "{:?}: {inkn} on {sn} is {c:.2}:1 — not legible",
                        t.mode
                    );
                }
            }
        }
    }

    /// NUMBER must stay green-ish and ERROR red-ish in both themes. Picking a
    /// legible colour is not enough if it stops carrying its meaning.
    #[test]
    fn semantic_hues_survive_both_themes() {
        for t in both() {
            assert!(
                t.number.g() > t.number.r() && t.number.g() > t.number.b(),
                "{:?}: NUMBER is no longer green",
                t.mode
            );
            assert!(
                t.error.r() > t.error.g() && t.error.r() > t.error.b(),
                "{:?}: ERROR is no longer red",
                t.mode
            );
            assert!(
                t.invalid_flag.r() > t.invalid_flag.g() + 60,
                "{:?}: the validation flag is no longer loud red",
                t.mode
            );
            assert!(
                t.accent.b() > t.accent.r(),
                "{:?}: ACCENT is no longer blue",
                t.mode
            );
            // Match highlights are amber: warm, and not the selection blue.
            let m = over(t.match_current, t.bg);
            assert!(m.r() > m.b(), "{:?}: the search match is not warm", t.mode);
        }
    }

    /// Structural surfaces have to be told apart from each other, or zebra
    /// striping, table banding and the empty-row padding all read as one flat
    /// field.
    #[test]
    fn structural_surfaces_are_distinguishable() {
        for t in both() {
            let d = |a: Color32, b: Color32| {
                (a.r() as i32 - b.r() as i32).abs()
                    + (a.g() as i32 - b.g() as i32).abs()
                    + (a.b() as i32 - b.b() as i32).abs()
            };
            assert!(d(t.bg, t.row_alt) >= 6, "{:?}: no zebra striping", t.mode);
            assert!(
                d(t.bg, t.table_band) >= 6,
                "{:?}: table banding invisible",
                t.mode
            );
            assert!(
                d(t.row_alt, t.table_band) >= 6,
                "{:?}: table banding is the same as the sheet's stripe",
                t.mode
            );
            // Issue #20: padding must not look like a real but empty row.
            assert!(
                d(t.bg, t.pad_row) >= 6 && d(t.row_alt, t.pad_row) >= 6,
                "{:?}: empty padding rows look like real rows",
                t.mode
            );
            assert!(
                d(t.bg, t.grid_line) >= 20,
                "{:?}: grid lines invisible",
                t.mode
            );
        }
    }

    /// Issue #38. The reference outlines are the whole point of the coloured
    /// highlighting: if two of them look the same, or one vanishes against the
    /// grid, the feature has failed even though it "works".
    #[test]
    fn reference_outline_colours_are_distinct_and_visible_in_both_themes() {
        for t in both() {
            let d = |a: Color32, b: Color32| {
                (a.r() as i32 - b.r() as i32).abs()
                    + (a.g() as i32 - b.g() as i32).abs()
                    + (a.b() as i32 - b.b() as i32).abs()
            };
            for (i, c) in t.ref_colors.iter().enumerate() {
                for surface in [t.bg, t.row_alt, over(t.range_fill, t.bg)] {
                    let ratio = contrast(*c, surface);
                    assert!(
                        ratio >= 3.0,
                        "{:?}: reference outline {i} is {ratio:.2}:1 against the grid — invisible",
                        t.mode
                    );
                }
                // Not the selection colour: a blue outline reads as "selected".
                assert!(
                    d(*c, t.accent) >= 90,
                    "{:?}: reference outline {i} is too close to the selection accent",
                    t.mode
                );
                for (j, other) in t.ref_colors.iter().enumerate().skip(i + 1) {
                    assert!(
                        d(*c, *other) >= 90,
                        "{:?}: reference outlines {i} and {j} are the same colour",
                        t.mode
                    );
                }
            }
        }
    }

    #[test]
    fn chart_chrome_follows_the_theme() {
        let d = ChartChrome::from(Theme::dark());
        let l = ChartChrome::from(Theme::light());
        assert!(luminance(d.bg) < luminance(l.bg));
        assert!(contrast(d.label, d.bg) >= 4.0);
        assert!(contrast(l.label, l.bg) >= 4.0);
    }
}
