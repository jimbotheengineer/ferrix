//! Page setup and pagination for printing and PDF export.
//!
//! # Why this is not a loop over rows
//!
//! The naive paginator walks every row, accumulates heights, and cuts a page
//! when the accumulator overflows. On a 200M-row sheet that is 200M iterations
//! to answer a question the user asked in order to be told "this is 400,000
//! pages, did you mean to?" — and they wait a minute to hear it. Worse, the
//! obvious way to return the answer is a `Vec<Page>`, which at ~40 bytes per
//! page is 16GB for that sheet. Both halves violate the scale invariant.
//!
//! So pagination here is **arithmetic over uniform runs**, not iteration over
//! rows. [`RowSizes`] already stores heights as spans, and the rows it does
//! *not* mention are all exactly the default height. That means the sheet is a
//! short sequence of runs, each of which is "N rows that are all `h` tall".
//! Within one run the answer is closed-form: `floor(capacity / h)` rows fit on
//! a page, so the run spans `ceil(N / rows_per_page)` pages and we can skip
//! straight to its end. Cost is O(number of runs), independent of row count.
//!
//! [`Paginator::page_count`] is therefore cheap enough to call before showing
//! the export dialog, which is what makes the "more than 1000 pages" warning
//! possible at all. And [`Paginator::pages`] is a lazy iterator that holds one
//! [`Page`] at a time, so rendering streams a band at a time rather than
//! materialising the document.
//!
//! # Rows do not straddle a page
//!
//! Matching Excel, a row that does not fit in the remaining space moves whole
//! to the next page rather than being split across the boundary. A row taller
//! than an entire page is the one exception — it gets a page to itself and is
//! clipped, because the alternative is an infinite loop.

use crate::sizing::{ColSizes, RowSizes};

/// A page is 1/72 inch — the PostScript/PDF unit. Every length in this module
/// is in points, so nothing has to guess what a bare `f32` means.
pub type Points = f32;

/// Excel's default row height and column width, in points.
pub const DEFAULT_ROW_HEIGHT: Points = 15.0;
/// Excel's default column width (8.43 chars) in points.
pub const DEFAULT_COL_WIDTH: Points = 48.0;

/// Above this many pages, callers should confirm before rendering.
///
/// Exists because "print" on a 200M-row sheet is a 400,000-page job that no
/// one meant to start, and the only cheap moment to say so is before the
/// first byte is written.
pub const LARGE_JOB_PAGES: u64 = 1000;

/// Standard paper sizes, as portrait width x height in points.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PaperSize {
    #[default]
    Letter,
    Legal,
    Tabloid,
    A3,
    A4,
    A5,
}

impl PaperSize {
    /// Portrait dimensions `(width, height)` in points.
    pub fn dimensions(self) -> (Points, Points) {
        match self {
            // 8.5 x 11 in
            PaperSize::Letter => (612.0, 792.0),
            // 8.5 x 14 in
            PaperSize::Legal => (612.0, 1008.0),
            // 11 x 17 in
            PaperSize::Tabloid => (792.0, 1224.0),
            // 297 x 420 mm
            PaperSize::A3 => (841.89, 1190.55),
            // 210 x 297 mm
            PaperSize::A4 => (595.28, 841.89),
            // 148 x 210 mm
            PaperSize::A5 => (419.53, 595.28),
        }
    }

    /// Human-readable name, for the page-setup dropdown.
    pub fn label(self) -> &'static str {
        match self {
            PaperSize::Letter => "Letter",
            PaperSize::Legal => "Legal",
            PaperSize::Tabloid => "Tabloid",
            PaperSize::A3 => "A3",
            PaperSize::A4 => "A4",
            PaperSize::A5 => "A5",
        }
    }

    /// Every size, in dropdown order.
    pub fn all() -> &'static [PaperSize] {
        &[
            PaperSize::Letter,
            PaperSize::Legal,
            PaperSize::Tabloid,
            PaperSize::A3,
            PaperSize::A4,
            PaperSize::A5,
        ]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

/// Page margins in points.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Margins {
    pub left: Points,
    pub right: Points,
    pub top: Points,
    pub bottom: Points,
    /// Space reserved for the header, inside `top`.
    pub header: Points,
    /// Space reserved for the footer, inside `bottom`.
    pub footer: Points,
}

impl Default for Margins {
    /// Excel's "Normal" preset: 0.7in sides, 0.75in top/bottom, 0.3in
    /// header/footer.
    fn default() -> Self {
        Margins {
            left: 50.4,
            right: 50.4,
            top: 54.0,
            bottom: 54.0,
            header: 21.6,
            footer: 21.6,
        }
    }
}

impl Margins {
    /// Excel's "Narrow" preset.
    pub fn narrow() -> Self {
        Margins {
            left: 18.0,
            right: 18.0,
            top: 36.0,
            bottom: 36.0,
            header: 14.4,
            footer: 14.4,
        }
    }

    /// Excel's "Wide" preset.
    pub fn wide() -> Self {
        Margins {
            left: 72.0,
            right: 72.0,
            top: 72.0,
            bottom: 72.0,
            header: 36.0,
            footer: 36.0,
        }
    }
}

/// How content is scaled to the page.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scaling {
    /// A fixed percentage. 100 means no scaling.
    Percent(u16),
    /// Shrink until the content fits `wide` pages across and `tall` pages
    /// down. Either may be `None` to leave that axis unconstrained.
    FitTo {
        wide: Option<u16>,
        tall: Option<u16>,
    },
}

impl Default for Scaling {
    fn default() -> Self {
        Scaling::Percent(100)
    }
}

impl Scaling {
    /// The scale factor to apply, given the unscaled content extent and the
    /// printable area of one page.
    ///
    /// `FitTo` never scales *up* — matching Excel, fitting a half-page report
    /// to one page leaves it half a page rather than magnifying it.
    pub fn factor(self, content: (Points, Points), printable: (Points, Points)) -> f32 {
        match self {
            Scaling::Percent(p) => (p as f32 / 100.0).max(0.01),
            Scaling::FitTo { wide, tall } => {
                let mut f = 1.0f32;
                if let Some(w) = wide.filter(|w| *w > 0) {
                    if content.0 > 0.0 {
                        f = f.min(printable.0 * w as f32 / content.0);
                    }
                }
                if let Some(t) = tall.filter(|t| *t > 0) {
                    if content.1 > 0.0 {
                        f = f.min(printable.1 * t as f32 / content.1);
                    }
                }
                // Never magnify, and never collapse to zero.
                f.clamp(0.01, 1.0)
            }
        }
    }
}

/// Which direction pages are numbered when the sheet is wider *and* taller
/// than one page.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PageOrder {
    /// All the pages of the first column band, then the next band.
    #[default]
    DownThenOver,
    /// All the pages of the first row band, then the next band.
    OverThenDown,
}

/// A header or footer, in Excel's three-part left/centre/right form.
///
/// The strings may contain field codes; see [`HeaderFooter::render`].
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HeaderFooter {
    pub left: String,
    pub center: String,
    pub right: String,
}

impl HeaderFooter {
    pub fn is_empty(&self) -> bool {
        self.left.is_empty() && self.center.is_empty() && self.right.is_empty()
    }

    /// The three parts with field codes substituted.
    pub fn render(&self, ctx: &FieldContext) -> [String; 3] {
        [
            substitute_fields(&self.left, ctx),
            substitute_fields(&self.center, ctx),
            substitute_fields(&self.right, ctx),
        ]
    }
}

/// The values header/footer field codes resolve against.
#[derive(Clone, Debug, Default)]
pub struct FieldContext {
    /// 1-based page number.
    pub page: u64,
    /// Total pages in the job.
    pub pages: u64,
    /// Date, already formatted for display.
    pub date: String,
    /// Time, already formatted for display.
    pub time: String,
    /// Workbook file name.
    pub file: String,
    /// Sheet name.
    pub sheet: String,
}

/// Replace Excel's `&`-prefixed field codes with their values.
///
/// Supported: `&P` page, `&N` total pages, `&D` date, `&T` time, `&F` file,
/// `&A` sheet, `&&` a literal ampersand. An unrecognised code is left as
/// written rather than silently deleted — a user who typed `&Q` should see
/// `&Q` and notice, not lose two characters and wonder.
pub fn substitute_fields(src: &str, ctx: &FieldContext) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('P') => {
                chars.next();
                out.push_str(&ctx.page.to_string());
            }
            Some('N') => {
                chars.next();
                out.push_str(&ctx.pages.to_string());
            }
            Some('D') => {
                chars.next();
                out.push_str(&ctx.date);
            }
            Some('T') => {
                chars.next();
                out.push_str(&ctx.time);
            }
            Some('F') => {
                chars.next();
                out.push_str(&ctx.file);
            }
            Some('A') => {
                chars.next();
                out.push_str(&ctx.sheet);
            }
            Some('&') => {
                chars.next();
                out.push('&');
            }
            // Unknown code, or a trailing '&' at end of string.
            _ => out.push('&'),
        }
    }
    out
}

/// Everything that decides how a sheet lands on paper.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct PageSetup {
    pub paper: PaperSize,
    pub orientation: Orientation,
    pub margins: Margins,
    pub scaling: Scaling,
    /// Rows repeated at the top of every page, inclusive, 0-based.
    pub repeat_rows: Option<(u32, u32)>,
    /// Columns repeated at the left of every page, inclusive, 0-based.
    pub repeat_cols: Option<(u32, u32)>,
    pub gridlines: bool,
    /// Print the A/B/C and 1/2/3 headings.
    pub headings: bool,
    pub header: HeaderFooter,
    pub footer: HeaderFooter,
    pub order: PageOrder,
    /// Manual page breaks before these rows, 0-based, ascending.
    pub row_breaks: Vec<u32>,
    /// Manual page breaks before these columns, 0-based, ascending.
    pub col_breaks: Vec<u32>,
}

impl PageSetup {
    /// The full sheet of paper, honouring orientation.
    pub fn paper_size(&self) -> (Points, Points) {
        let (w, h) = self.paper.dimensions();
        match self.orientation {
            Orientation::Portrait => (w, h),
            Orientation::Landscape => (h, w),
        }
    }

    /// The area content may occupy, after margins and header/footer bands.
    ///
    /// Clamped at zero so absurd margins produce an empty page rather than a
    /// negative capacity that would make the paginator loop forever.
    pub fn printable(&self) -> (Points, Points) {
        let (pw, ph) = self.paper_size();
        let w = pw - self.margins.left - self.margins.right;
        let h = ph - self.margins.top - self.margins.bottom;
        (w.max(0.0), h.max(0.0))
    }

    /// Add a manual page break before `row`, keeping the list sorted and
    /// duplicate-free.
    pub fn add_row_break(&mut self, row: u32) {
        if let Err(pos) = self.row_breaks.binary_search(&row) {
            self.row_breaks.insert(pos, row);
        }
    }

    /// Add a manual page break before `col`.
    pub fn add_col_break(&mut self, col: u32) {
        if let Err(pos) = self.col_breaks.binary_search(&col) {
            self.col_breaks.insert(pos, col);
        }
    }

    pub fn remove_row_break(&mut self, row: u32) -> bool {
        match self.row_breaks.binary_search(&row) {
            Ok(pos) => {
                self.row_breaks.remove(pos);
                true
            }
            Err(_) => false,
        }
    }

    pub fn remove_col_break(&mut self, col: u32) -> bool {
        match self.col_breaks.binary_search(&col) {
            Ok(pos) => {
                self.col_breaks.remove(pos);
                true
            }
            Err(_) => false,
        }
    }
}

/// One page's worth of the sheet: a row band crossed with a column band.
///
/// Rows and columns are inclusive 0-based bounds. This is ~24 bytes and is
/// produced lazily, so a 400,000-page job still only ever holds one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Page {
    /// 1-based page number in the job.
    pub number: u64,
    pub first_row: u32,
    pub last_row: u32,
    pub first_col: u32,
    pub last_col: u32,
}

impl Page {
    pub fn rows(&self) -> u64 {
        (self.last_row as u64) - (self.first_row as u64) + 1
    }
    pub fn cols(&self) -> u64 {
        (self.last_col as u64) - (self.first_col as u64) + 1
    }
}

/// A maximal run of consecutive rows that all have the same height.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Run {
    first: u32,
    last: u32,
    height: Points,
}

impl Run {
    fn len(&self) -> u64 {
        (self.last as u64) - (self.first as u64) + 1
    }
}

/// Walk `[first..=last]` as uniform-height runs.
///
/// `RowSizes` stores explicitly-sized rows as spans and says nothing about the
/// rest, which are all `default`. Stitching those together gives a run list
/// whose length is O(number of sized spans), *not* O(rows) — which is the
/// whole reason pagination can be cheap on a 200M-row sheet.
///
/// Hidden rows (height 0) are skipped: they contribute nothing to the page and
/// must not be emitted, or a page would "contain" rows that print nothing.
fn runs_in(sizes: &RowSizes, first: u32, last: u32, default: Points) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let mut push = |first: u32, last: u32, height: Points| {
        if first > last || height <= 0.0 {
            return;
        }
        // Coalesce with the previous run when the height matches, so a
        // sequence of same-height spans stays one run.
        if let Some(prev) = runs.last_mut() {
            if prev.height == height && prev.last + 1 == first {
                prev.last = last;
                return;
            }
        }
        runs.push(Run {
            first,
            last,
            height,
        });
    };

    let mut cursor = first;
    for (sfirst, slast, h) in sizes.spans() {
        if slast < first {
            continue;
        }
        if sfirst > last {
            break;
        }
        let sfirst = sfirst.max(first);
        let slast = slast.min(last);
        if cursor < sfirst {
            push(cursor, sfirst - 1, default);
        }
        push(sfirst, slast, h);
        cursor = slast.saturating_add(1);
        if slast == u32::MAX {
            return runs;
        }
    }
    if cursor <= last {
        push(cursor, last, default);
    }
    runs
}

/// Splits a sheet into pages without materialising them.
pub struct Paginator {
    setup: PageSetup,
    /// Row bands, as (first, last) inclusive.
    row_bands: Vec<(u32, u32)>,
    /// Column bands, as (first, last) inclusive.
    col_bands: Vec<(u32, u32)>,
}

impl Paginator {
    /// Compute the band structure for a sheet.
    ///
    /// `rows` and `cols` are the inclusive content extents (the print area, if
    /// one is set). Cost is O(sized spans + columns + manual breaks).
    ///
    /// Row bands are computed by closed-form arithmetic per uniform run, so
    /// the row count does not enter the cost. Columns are walked individually
    /// because column counts are small by construction (thousands at most) and
    /// widths are stored per column, not as runs.
    pub fn new(
        setup: PageSetup,
        rows: (u32, u32),
        cols: (u32, u32),
        row_sizes: &RowSizes,
        col_sizes: &ColSizes,
    ) -> Self {
        let printable = setup.printable();

        // Repeated header rows and columns eat into every page's capacity.
        let repeat_h = setup
            .repeat_rows
            .map(|(a, b)| measure_rows(row_sizes, a, b))
            .unwrap_or(0.0);
        let repeat_w = setup
            .repeat_cols
            .map(|(a, b)| measure_cols(col_sizes, a, b))
            .unwrap_or(0.0);

        let scale = setup.scaling.factor(
            (
                measure_cols(col_sizes, cols.0, cols.1),
                measure_rows(row_sizes, rows.0, rows.1),
            ),
            printable,
        );

        // Work in unscaled content units: dividing the capacity by the scale
        // once is equivalent to scaling every row, and avoids rounding drift
        // accumulating across millions of rows.
        let cap_h = ((printable.1 / scale) - repeat_h).max(1.0);
        let cap_w = ((printable.0 / scale) - repeat_w).max(1.0);

        let row_bands = band_rows(row_sizes, rows, cap_h, &setup.row_breaks);
        let col_bands = band_cols(col_sizes, cols, cap_w, &setup.col_breaks);

        Paginator {
            setup,
            row_bands,
            col_bands,
        }
    }

    /// Total pages, without building any.
    pub fn page_count(&self) -> u64 {
        self.row_bands.len() as u64 * self.col_bands.len() as u64
    }

    /// Whether a caller should confirm before rendering this job.
    pub fn is_large(&self) -> bool {
        self.page_count() > LARGE_JOB_PAGES
    }

    pub fn row_band_count(&self) -> usize {
        self.row_bands.len()
    }

    pub fn col_band_count(&self) -> usize {
        self.col_bands.len()
    }

    pub fn setup(&self) -> &PageSetup {
        &self.setup
    }

    /// The rows at which a page break falls, for Page Break Preview.
    pub fn row_break_rows(&self) -> impl Iterator<Item = u32> + '_ {
        self.row_bands.iter().skip(1).map(|(first, _)| *first)
    }

    /// The columns at which a page break falls, for Page Break Preview.
    pub fn col_break_cols(&self) -> impl Iterator<Item = u32> + '_ {
        self.col_bands.iter().skip(1).map(|(first, _)| *first)
    }

    /// Every page, lazily, in the configured [`PageOrder`].
    ///
    /// This is an iterator rather than a `Vec` on purpose: a caller renders
    /// one page and drops it, so peak memory is one [`Page`] regardless of
    /// job size.
    pub fn pages(&self) -> impl Iterator<Item = Page> + '_ {
        let order = self.setup.order;
        let rows = &self.row_bands;
        let cols = &self.col_bands;
        let total = self.page_count();
        (0..total).map(move |i| {
            let (ri, ci) = match order {
                PageOrder::DownThenOver => (i % rows.len() as u64, i / rows.len() as u64),
                PageOrder::OverThenDown => (i / cols.len() as u64, i % cols.len() as u64),
            };
            let (first_row, last_row) = rows[ri as usize];
            let (first_col, last_col) = cols[ci as usize];
            Page {
                number: i + 1,
                first_row,
                last_row,
                first_col,
                last_col,
            }
        })
    }

    /// The single page containing `row`/`col`, if any.
    pub fn page_at(&self, row: u32, col: u32) -> Option<Page> {
        let ri = self
            .row_bands
            .iter()
            .position(|(a, b)| row >= *a && row <= *b)?;
        let ci = self
            .col_bands
            .iter()
            .position(|(a, b)| col >= *a && col <= *b)?;
        let i = match self.setup.order {
            PageOrder::DownThenOver => ci as u64 * self.row_bands.len() as u64 + ri as u64,
            PageOrder::OverThenDown => ri as u64 * self.col_bands.len() as u64 + ci as u64,
        };
        let (first_row, last_row) = self.row_bands[ri];
        let (first_col, last_col) = self.col_bands[ci];
        Some(Page {
            number: i + 1,
            first_row,
            last_row,
            first_col,
            last_col,
        })
    }
}

/// Total height of `[first..=last]`, in unscaled points.
pub fn measure_rows(sizes: &RowSizes, first: u32, last: u32) -> Points {
    if first > last {
        return 0.0;
    }
    runs_in(sizes, first, last, DEFAULT_ROW_HEIGHT)
        .iter()
        .map(|r| r.height * r.len() as f32)
        .sum()
}

/// Total width of `[first..=last]`, in unscaled points.
pub fn measure_cols(sizes: &ColSizes, first: u32, last: u32) -> Points {
    if first > last {
        return 0.0;
    }
    (first..=last)
        .filter(|c| !sizes.is_hidden(*c))
        .map(|c| sizes.width_of(c).unwrap_or(DEFAULT_COL_WIDTH))
        .sum()
}

/// Cut `[rows.0..=rows.1]` into bands that each fit `cap` points.
///
/// The heart of the scale story. For each uniform run we do not iterate rows:
/// we fill whatever is left of the open page, then compute how many whole
/// pages the remainder needs with one division, and jump to the end of the
/// run.
fn band_rows(sizes: &RowSizes, rows: (u32, u32), cap: Points, breaks: &[u32]) -> Vec<(u32, u32)> {
    let (first, last) = rows;
    if first > last {
        return vec![(first, first)];
    }
    let runs = runs_in(sizes, first, last, DEFAULT_ROW_HEIGHT);
    if runs.is_empty() {
        // Everything in range is hidden; still one page so the header and
        // footer have somewhere to print.
        return vec![(first, last)];
    }

    let mut bands: Vec<(u32, u32)> = Vec::new();
    let mut band_start = runs[0].first;
    let mut used: Points = 0.0;

    for run in &runs {
        let mut row = run.first;
        // A manual break anywhere inside this run forces a cut there.
        loop {
            // Where the current run segment ends: either the run's end, or
            // just before the next manual break inside it.
            let seg_end = match breaks.iter().copied().find(|b| *b > row && *b <= run.last) {
                Some(b) => b - 1,
                None => run.last,
            };

            let mut cursor = row;
            while cursor <= seg_end {
                let remaining = cap - used;
                let fits = if run.height > remaining {
                    0u64
                } else {
                    (remaining / run.height).floor() as u64
                };
                let left = (seg_end as u64) - (cursor as u64) + 1;

                if fits == 0 {
                    if used == 0.0 {
                        // A single row taller than a whole page. Give it its
                        // own page rather than looping forever.
                        bands.push((band_start, cursor));
                        cursor += 1;
                        band_start = cursor;
                        continue;
                    }
                    // Close the page and retry against a fresh one.
                    bands.push((band_start, cursor - 1));
                    band_start = cursor;
                    used = 0.0;
                    continue;
                }

                let take = fits.min(left);
                let end = cursor as u64 + take - 1;
                used += run.height * take as f32;
                cursor = (end + 1) as u32;

                if take == fits && cursor <= seg_end {
                    // The page filled exactly; close it.
                    bands.push((band_start, end as u32));
                    band_start = cursor;
                    used = 0.0;
                }
            }

            row = cursor;
            if seg_end >= run.last {
                break;
            }
            // We stopped at a manual break: close the page here.
            if band_start < row {
                bands.push((band_start, row - 1));
                band_start = row;
                used = 0.0;
            }
        }
    }

    if band_start <= runs[runs.len() - 1].last {
        bands.push((band_start, runs[runs.len() - 1].last));
    }
    if bands.is_empty() {
        bands.push((first, last));
    }
    bands
}

/// Cut `[cols.0..=cols.1]` into bands that each fit `cap` points.
fn band_cols(sizes: &ColSizes, cols: (u32, u32), cap: Points, breaks: &[u32]) -> Vec<(u32, u32)> {
    let (first, last) = cols;
    if first > last {
        return vec![(first, first)];
    }
    let mut bands = Vec::new();
    let mut band_start = first;
    let mut used: Points = 0.0;
    let mut any = false;

    for c in first..=last {
        if sizes.is_hidden(c) {
            continue;
        }
        let w = sizes.width_of(c).unwrap_or(DEFAULT_COL_WIDTH);
        let forced = breaks.binary_search(&c).is_ok() && c > band_start;
        if forced || (any && used + w > cap) {
            bands.push((band_start, c - 1));
            band_start = c;
            used = 0.0;
        }
        used += w;
        any = true;
    }
    if any {
        bands.push((band_start, last));
    } else {
        bands.push((first, last));
    }
    bands
}

#[cfg(test)]
#[path = "page/tests.rs"]
mod tests;
