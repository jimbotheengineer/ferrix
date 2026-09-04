//! The Selection side panel (DESIGN.md mock: right dock with Active Range,
//! aggregates, a sparkline and recent activity).
//!
//! Aggregation is BOUNDED: a selection larger than [`MAX_AGG_CELLS`] is
//! aggregated over its first rows only, and the panel says so out loud —
//! "Never trade truth for polish". The scan cost therefore has a hard ceiling
//! no matter how large the sheet or the selection (200M rows must cost the
//! same as 200).
//!
//! The activity log is fed by the app's status line: every time the status
//! message changes, the previous distinct message is recorded with a
//! wall-clock-free elapsed stamp. That reuses the existing "status messages
//! identify object and outcome" discipline instead of inventing a second
//! event system.

use std::collections::VecDeque;
use std::time::Instant;

use ferrix_core::{CellRef, Selection, Value};

use crate::sheet_view::SheetView;

/// Hard ceiling on cells scanned for the aggregates, chosen so the worst case
/// stays well under a millisecond. Beyond it the panel reports a truncated
/// aggregate and names the cap.
pub const MAX_AGG_CELLS: u64 = 100_000;

/// How many activity entries are kept (and at most drawn).
const MAX_ACTIVITY: usize = 8;

/// How many sample points the sparkline keeps from the scanned values.
const SPARK_POINTS: usize = 48;

/// Aggregates over the (possibly capped) numeric cells of a selection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SelectionStats {
    /// Non-empty cells seen (numeric or not).
    pub count: u64,
    /// Numeric cells seen.
    pub numbers: u64,
    pub sum: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Cells actually scanned (<= the selection's cell count).
    pub scanned: u64,
    /// True when the selection was larger than [`MAX_AGG_CELLS`] and the
    /// aggregates therefore cover only its head.
    pub truncated: bool,
    /// An evenly-spaced sample of the numeric values in scan order, for the
    /// sparkline. Empty when there are no numbers.
    pub spark: Vec<f64>,
}

impl SelectionStats {
    pub fn average(&self) -> Option<f64> {
        (self.numbers > 0).then(|| self.sum / self.numbers as f64)
    }

    /// Scan `sel` over `view`, bounded by [`MAX_AGG_CELLS`].
    ///
    /// Row-major from the top-left corner, so the truncated case reads "the
    /// first N cells" — a comprehensible cap rather than an arbitrary subset.
    pub fn compute(view: &SheetView<'_>, sel: Selection) -> Self {
        let (tl, br) = sel.bounds();
        let mut out = Self::default();
        // Sample stride so the sparkline covers the whole scanned range
        // rather than only its first points.
        let total = sel.cell_count().min(MAX_AGG_CELLS);
        let stride = (total / SPARK_POINTS as u64).max(1);
        let mut numeric_seen = 0u64;
        'scan: for r in tl.row..=br.row {
            for c in tl.col..=br.col {
                if out.scanned >= MAX_AGG_CELLS {
                    out.truncated = true;
                    break 'scan;
                }
                out.scanned += 1;
                let v = view.get(CellRef::new(r, c));
                let n = match v {
                    Value::Number(n) if n.is_finite() => Some(n),
                    Value::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
                    Value::Empty => None,
                    _ => {
                        out.count += 1;
                        None
                    }
                };
                if let Some(n) = n {
                    out.count += 1;
                    out.numbers += 1;
                    out.sum += n;
                    out.min = Some(out.min.map_or(n, |m: f64| m.min(n)));
                    out.max = Some(out.max.map_or(n, |m: f64| m.max(n)));
                    if numeric_seen % stride == 0 && out.spark.len() < SPARK_POINTS {
                        out.spark.push(n);
                    }
                    numeric_seen += 1;
                }
            }
        }
        out
    }
}

/// One recorded status message.
pub struct ActivityEntry {
    pub at: Instant,
    pub message: String,
}

/// The panel's own state: open/closed, the activity log, and the last status
/// message seen (so a change is recorded exactly once).
#[derive(Default)]
pub struct SelectionPanel {
    pub open: bool,
    activity: VecDeque<ActivityEntry>,
    last_status: String,
}

impl SelectionPanel {
    /// A panel restored from preferences: open or closed, empty log.
    pub fn with_open(open: bool) -> Self {
        Self {
            open,
            ..Default::default()
        }
    }

    /// Record the current status message if it changed since the last frame.
    /// Called once per frame; a no-op the rest of the time.
    pub fn observe_status(&mut self, status: &str) {
        if status.is_empty() || status == self.last_status {
            return;
        }
        self.last_status = status.to_string();
        self.activity.push_front(ActivityEntry {
            at: Instant::now(),
            message: status.to_string(),
        });
        while self.activity.len() > MAX_ACTIVITY {
            self.activity.pop_back();
        }
    }

    pub fn activity(&self) -> impl Iterator<Item = &ActivityEntry> {
        self.activity.iter()
    }

    #[cfg(test)]
    pub fn activity_len(&self) -> usize {
        self.activity.len()
    }
}

/// "just now", "3 min ago" — coarse on purpose; the log is orientation, not
/// an audit trail.
pub fn age_label(elapsed_secs: u64) -> String {
    match elapsed_secs {
        0..=5 => "just now".to_string(),
        6..=59 => format!("{elapsed_secs}s ago"),
        60..=3599 => format!("{} min ago", elapsed_secs / 60),
        _ => format!("{} h ago", elapsed_secs / 3600),
    }
}

/// Spreadsheet-style compact number for the stat rows: integers plain,
/// fractions to two decimals.
pub fn fmt_stat(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        let mut s = format!("{}", n as i64);
        // Thousands separators, matching the status bar's fmt_int.
        let neg = s.starts_with('-');
        let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
        let mut grouped = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(ch);
        }
        s = if neg { format!("-{grouped}") } else { grouped };
        s
    } else {
        format!("{n:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_labels_are_coarse_and_human() {
        assert_eq!(age_label(0), "just now");
        assert_eq!(age_label(30), "30s ago");
        assert_eq!(age_label(120), "2 min ago");
        assert_eq!(age_label(7200), "2 h ago");
    }

    #[test]
    fn stat_formatting_groups_thousands_and_trims_integers() {
        assert_eq!(fmt_stat(1234567.0), "1,234,567");
        assert_eq!(fmt_stat(-1234.0), "-1,234");
        assert_eq!(fmt_stat(12.345), "12.35");
        assert_eq!(fmt_stat(0.0), "0");
    }

    #[test]
    fn the_activity_log_records_changes_once_and_is_bounded() {
        let mut p = SelectionPanel::default();
        p.observe_status("Loaded 200 rows");
        p.observe_status("Loaded 200 rows"); // same message: not re-recorded
        assert_eq!(p.activity_len(), 1);
        for i in 0..20 {
            p.observe_status(&format!("edit {i}"));
        }
        assert!(p.activity_len() <= MAX_ACTIVITY);
        // Newest first.
        assert_eq!(p.activity().next().unwrap().message, "edit 19");
    }
}
