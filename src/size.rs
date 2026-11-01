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

/// Terminal rows wanted: the lines the workers take, plus the footer line
/// when it shows. There is no frame. Never less than one row.
pub fn wanted_rows(lines: usize, footer: bool) -> u16 {
    u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(u16::from(footer))
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

#[cfg(test)]
mod tests {
    use super::*;

    const PER: f64 = 0.02; // 50 rows in the window

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
        assert_eq!(wanted_rows(3, false), 3);
        assert_eq!(wanted_rows(3, true), 4);
        assert_eq!(wanted_rows(0, false), 1);
        assert_eq!(wanted_rows(0, true), 1);
        assert_eq!(wanted_rows(usize::MAX, true), u16::MAX);
    }

    #[test]
    fn converges_without_bouncing() {
        // Simulate a layer with one row of chrome: rows = h / PER - 1.
        #[allow(clippy::cast_possible_truncation)]
        let rows_of = |h: f64| ((h / PER).round() as u16).saturating_sub(1);
        let want = wanted_rows(3, true);
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
    fn never_runs_past_the_bottom_of_the_window() {
        // Review finding: a layer moved low, y = 0.97, wants to shrink.
        let h = new_height(2, 10, PER, 0.97, 0.03);
        assert!(h.is_none_or(|h| 0.97 + h <= 1.0 + 1e-9), "{h:?}");
    }
}
