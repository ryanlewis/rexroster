//! Fitting the roster's floating layer to its rows.
//!
//! The layer's window chrome takes about a row, so a height worked out from
//! row counts alone comes out short. Instead the roster compares the rows it
//! wants with the rows its terminal really has, grows or shrinks the layer by
//! the difference, and stops when the difference is zero.

/// The tallest the layer gets, as a fraction of the window.
pub const MAX_HEIGHT: f64 = 0.6;
/// The fewest rows the layer shrinks to: one line plus about a row of window chrome.
pub const MIN_ROWS: f64 = 2.0;
/// The fewest columns the roster's terminal should have. Below this, a
/// compact row has little room left for the message.
pub const MIN_COLUMNS: u16 = 50;

use std::time::Duration;

use crate::config::Border;

/// The first wait before measuring again while the pane hasn't been drawn.
/// Each wait doubles, up to `RETRY_STEP_MAX`.
pub const RETRY_FIRST: Duration = Duration::from_millis(100);
pub const RETRY_STEP_MAX: Duration = Duration::from_secs(1);
/// The most the retries wait in all, after which the layer keeps its height
/// until something else (a resize, a new row count) asks for a fit.
pub const RETRY_TOTAL: Duration = Duration::from_secs(5);

/// The wait before retry number `n` (from 0), or `None` once the waits so far
/// plus this one would pass `RETRY_TOTAL`.
pub fn retry_delay(n: u32) -> Option<Duration> {
    let step = |i: u32| {
        RETRY_FIRST
            .checked_mul(1 << i.min(16))
            .map_or(RETRY_STEP_MAX, |d| d.min(RETRY_STEP_MAX))
    };
    let mut total = Duration::ZERO;
    for i in 0..=n {
        total += step(i);
        if total > RETRY_TOTAL {
            return None;
        }
    }
    Some(step(n))
}

/// Rows the border takes: none, or a top and bottom line.
pub fn chrome_rows(border: Border) -> u16 {
    match border {
        Border::None => 0,
        Border::Rounded => 2,
    }
}

/// Terminal rows wanted for `lines` lines of content. Without a border the
/// footer takes a line of its own; with one it sits on the bottom border.
/// Never less than one row.
pub fn wanted_rows(lines: usize, footer: bool, border: Border) -> u16 {
    let footer_line = footer && border == Border::None;
    u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(u16::from(footer_line))
        .saturating_add(chrome_rows(border))
        .max(1)
}

/// The new layer height, or `None` when it already fits (or can't change).
///
/// - `want`: rows the roster wants.
/// - `rows`: rows its terminal has now.
/// - `per_row`: one row's share of the window height, from the lead's pane.
/// - `y`, `h`: the layer's current top and height, as window fractions.
pub fn new_height(want: u16, rows: u16, per_row: f64, y: f64, h: f64) -> Option<f64> {
    let missing = f64::from(want) - f64::from(rows);
    if missing == 0.0 || per_row <= 0.0 || !per_row.is_finite() {
        return None;
    }
    // The window's bottom edge wins over the minimum height.
    let target = (h + missing * per_row)
        .min(MAX_HEIGHT)
        .max(per_row * MIN_ROWS)
        .min(1.0 - y);
    ((target - h).abs() > 1e-6).then_some(target)
}

/// The new layer `x` and width when the roster has fewer than `MIN_COLUMNS`
/// columns, or `None` when it has enough (or can't grow).
///
/// - `cols`: columns its terminal has now.
/// - `per_col`: one column's share of the window width, from the lead's pane.
/// - `x`, `w`: the layer's current left edge and width, as window fractions.
///
/// The layer grows to the left, keeping its right edge, and stops at the
/// window's left edge. It never shrinks, so a layer widened by hand keeps its
/// width.
pub fn new_width(cols: u16, per_col: f64, x: f64, w: f64) -> Option<(f64, f64)> {
    let missing = f64::from(MIN_COLUMNS.saturating_sub(cols));
    if missing == 0.0 || per_col <= 0.0 || !per_col.is_finite() {
        return None;
    }
    let right = x + w;
    let target = (w + missing * per_col).min(right);
    ((target - w).abs() > 1e-6).then_some((right - target, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PER: f64 = 0.02; // 50 rows in the window

    #[test]
    fn widens_a_narrow_layer_to_the_left() {
        // 100 columns in the window: 20 columns at 0.20 wide.
        let (x, w) = new_width(20, 0.01, 0.72, 0.20).unwrap();
        assert!((w - 0.50).abs() < 1e-9, "{w}");
        assert!((x + w - 0.92).abs() < 1e-9, "right edge moved: {x}");
    }

    #[test]
    fn leaves_a_wide_enough_layer_alone() {
        assert_eq!(new_width(MIN_COLUMNS, 0.01, 0.5, 0.5), None);
        assert_eq!(new_width(80, 0.01, 0.2, 0.8), None);
    }

    #[test]
    fn widens_no_further_than_the_left_edge() {
        // 40 columns in the whole window.
        assert_eq!(new_width(10, 0.025, 0.7, 0.25), Some((0.0, 0.95)));
        assert_eq!(new_width(38, 0.025, 0.0, 0.95), None);
    }

    #[test]
    fn ignores_a_bad_column_width() {
        assert_eq!(new_width(10, 0.0, 0.7, 0.25), None);
        assert_eq!(new_width(10, f64::NAN, 0.7, 0.25), None);
    }

    #[test]
    fn stops_when_the_difference_is_zero() {
        assert_eq!(new_height(5, 5, PER, 0.02, 0.12), None);
    }

    #[test]
    fn grows_by_the_missing_rows() {
        let h = new_height(5, 3, PER, 0.02, 0.08).unwrap();
        assert!((h - 0.12).abs() < 1e-9, "{h}");
    }

    #[test]
    fn shrinks_by_the_extra_rows() {
        let h = new_height(4, 6, PER, 0.02, 0.16).unwrap();
        assert!((h - 0.12).abs() < 1e-9, "{h}");
    }

    #[test]
    fn caps_and_floors() {
        // Never taller than MAX_HEIGHT or past the bottom of the window.
        assert_eq!(new_height(200, 5, PER, 0.02, 0.12), Some(MAX_HEIGHT));
        assert_eq!(new_height(200, 5, PER, 0.7, 0.12), Some(1.0 - 0.7));
        // Never shorter than MIN_ROWS rows.
        let h = new_height(2, 40, PER, 0.02, 0.8).unwrap();
        assert!((h - PER * MIN_ROWS).abs() < 1e-9, "{h}");
    }

    #[test]
    fn no_change_when_already_at_a_limit() {
        // Wants more, but is already at the cap: don't resize again.
        assert_eq!(new_height(200, 30, PER, 0.02, MAX_HEIGHT), None);
    }

    #[test]
    fn ignores_a_bad_row_height() {
        assert_eq!(new_height(5, 3, 0.0, 0.02, 0.12), None);
        assert_eq!(new_height(5, 3, f64::NAN, 0.02, 0.12), None);
    }

    #[test]
    fn wanted_rows_counts_workers_and_the_footer() {
        let n = Border::None;
        assert_eq!(wanted_rows(3, false, n), 3);
        assert_eq!(wanted_rows(3, true, n), 4);
        assert_eq!(wanted_rows(0, false, n), 1);
        assert_eq!(wanted_rows(0, true, n), 1);
        assert_eq!(wanted_rows(usize::MAX, true, n), u16::MAX);
    }

    #[test]
    fn wanted_rows_with_a_border() {
        let r = Border::Rounded;
        // Two border rows; the footer sits on the bottom one.
        assert_eq!(wanted_rows(3, false, r), 5);
        assert_eq!(wanted_rows(3, true, r), 5);
        assert_eq!(wanted_rows(0, false, r), 2);
        assert_eq!(wanted_rows(usize::MAX, false, r), u16::MAX);
    }

    #[test]
    fn converges_without_bouncing() {
        // Simulate a layer with one row of chrome: rows = h / PER - 1.
        #[allow(clippy::cast_possible_truncation)]
        let rows_of = |h: f64| ((h / PER).round() as u16).saturating_sub(1);
        let want = wanted_rows(3, true, Border::None);
        let mut h = 0.12;
        let mut resizes = 0;
        while let Some(next) = new_height(want, rows_of(h), PER, 0.02, h) {
            h = next;
            resizes += 1;
            assert!(resizes < 3, "bouncing");
        }
        assert_eq!(rows_of(h), want);
    }

    #[test]
    fn retries_back_off_and_stop_within_the_total() {
        let ms = Duration::from_millis;
        assert_eq!(retry_delay(0), Some(ms(100)));
        assert_eq!(retry_delay(1), Some(ms(200)));
        assert_eq!(retry_delay(3), Some(ms(800)));
        // Capped at a second a step.
        assert_eq!(retry_delay(4), Some(RETRY_STEP_MAX));
        let waits: Vec<_> = (0..).map_while(retry_delay).collect();
        let total: Duration = waits.iter().sum();
        assert!(total <= RETRY_TOTAL, "{total:?}");
        assert!(total > RETRY_TOTAL - RETRY_STEP_MAX, "{total:?}");
        assert_eq!(retry_delay(u32::MAX), None);
    }

    #[test]
    fn never_runs_past_the_bottom_of_the_window() {
        // A layer moved low, y = 0.97, wants to shrink.
        let h = new_height(2, 10, PER, 0.97, 0.03);
        assert!(h.is_none_or(|h| 0.97 + h <= 1.0 + 1e-9), "{h:?}");
    }
}
