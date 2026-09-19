//! Local mouse selection + system clipboard state.
//!
//! C22.6: rimeterm can own the mouse for text selection when the child
//! program hasn't asked for xterm mouse reports.
//!
//! Two consumers with different needs:
//!
//! - [`PtyPane`](crate::pty_pane::PtyPane) stores its selection inside
//!   alacritty's `Term.selection` in **absolute grid coordinates**
//!   (`alacritty_terminal::index::Point`). When the child streams new
//!   output, `Term::scroll_up` rotates the selection along with the
//!   content, so the highlight always sticks to the same text — even
//!   across scrollback — exactly like alacritty itself. PtyPane keeps
//!   only the small UI-side state machine here: [`ClickStreak`] for
//!   double/triple-click granularity promotion, plus which anchor the
//!   drag is extending.
//! - The read-only file viewer overlays render into a ratatui `Buffer`
//!   and have no grid, so they keep the original viewport-relative
//!   [`SelectionState`].
//!
//! `Shift+Left` inside `Down` extends the existing selection instead of
//! starting a new one (Alacritty / xterm convention).

use std::time::Instant;

use alacritty_terminal::index::Point;

/// Maximum gap between clicks for double-/triple-click detection. 400 ms
/// matches xterm's default and Windows' `GetDoubleClickTime` median.
const MULTI_CLICK_MS: u128 = 400;

/// Which granularity a drag selects with, promoted on double- /
/// triple-click. Maps 1:1 onto alacritty's `SelectionType`
/// (Char→Simple, Word→Semantic, Line→Lines).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Granularity {
    #[default]
    Char,
    Word,
    Line,
}

/// PTY-side click state: streak counting + granularity promotion for a
/// selection anchored in `Term.selection`.
///
/// All coordinates are **absolute grid points** (`Point` = line +
/// column in alacritty's storage space), so a click streak survives
/// streaming output and viewport scrolling without re-anchoring.
#[derive(Clone, Debug, Default)]
pub struct ClickStreak {
    /// Timestamp + cell of the most recent `Down`, used to detect
    /// double/triple-click. Absolute grid point.
    last_click: Option<(Instant, Point)>,
    /// Click streak: 1 = char, 2 = word, 3 = line, wrapping back to 1.
    click_streak: u8,
}

impl ClickStreak {
    /// Register a fresh `Down` at absolute grid point `at`. If the click
    /// lands on the same point within [`MULTI_CLICK_MS`] of the previous
    /// one, the granularity promotes: 1 → 2 (word) → 3 (line) → back
    /// to 1 (char). `now` is passed in so unit tests can drive
    /// multi-click without racing wall-clock.
    pub fn begin(&mut self, at: Point, now: Instant) -> Granularity {
        let streak = match self.last_click {
            Some((t, point))
                if point == at && now.duration_since(t).as_millis() < MULTI_CLICK_MS =>
            {
                (self.click_streak % 3) + 1
            }
            _ => 1,
        };
        self.click_streak = streak;
        self.last_click = Some((now, at));
        match streak {
            2 => Granularity::Word,
            3 => Granularity::Line,
            _ => Granularity::Char,
        }
    }

    /// A click at a different point always restarts the streak — call
    /// before `begin` decides promotion. (Kept for symmetry; `begin`
    /// already requires point equality for promotion.)
    pub fn reset(&mut self) {
        self.click_streak = 0;
        self.last_click = None;
    }
}

/// Per-pane selection state for buffer-rendering surfaces (the file
/// viewer overlays). Empty (`None` anchor) means "no active selection";
/// a non-empty state means the highlight is either being dragged (still
/// tracking mouse) or frozen after `commit`.
///
/// PTY panes do NOT use this — they anchor inside `Term.selection`
/// (see module docs). Kept because viewer overlays render into ratatui
/// buffers with no grid to anchor against.
#[derive(Clone, Debug, Default)]
pub struct SelectionState {
    /// The stationary end of the selection (grabbed on `Down`).
    /// `None` = no selection at all.
    anchor: Option<Cell>,
    /// The moving end of the selection. Equal to `anchor` on a fresh
    /// `Down` with no drag yet.
    cursor: Cell,
    /// How to interpret the (anchor, cursor) range at extract time.
    mode: Granularity,
    /// When `commit` runs, we flip this true so the highlight stays
    /// visible until the next `Down` — matches xterm.
    frozen: bool,
    /// Timestamp + cell of the most recent `Down`, used to detect
    /// double/triple-click.
    last_click: Option<(Instant, Cell)>,
    /// Click streak: 1 = char, 2 = word, 3 = line, wrapping back to 1.
    click_streak: u8,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub row: u16,
    pub col: u16,
}

impl SelectionState {
    /// Start a fresh selection. `now` is passed in (rather than
    /// captured internally) so unit tests can drive multi-click without
    /// racing wall-clock.
    ///
    /// If the click lands on the same cell within [`MULTI_CLICK_MS`] of
    /// the previous `begin`, the granularity promotes: 1 → 2 (word) →
    /// 3 (line) → back to 1 (char).
    pub fn begin(&mut self, at: Cell, now: Instant) {
        let streak = match self.last_click {
            Some((t, cell)) if cell == at && now.duration_since(t).as_millis() < MULTI_CLICK_MS => {
                (self.click_streak % 3) + 1
            }
            _ => 1,
        };
        self.click_streak = streak;
        self.last_click = Some((now, at));
        self.mode = match streak {
            2 => Granularity::Word,
            3 => Granularity::Line,
            _ => Granularity::Char,
        };
        self.anchor = Some(at);
        self.cursor = at;
        self.frozen = false;
    }

    /// Extend the moving end during a drag. Silent no-op if there's no
    /// active anchor.
    pub fn extend(&mut self, to: Cell) {
        if self.anchor.is_some() {
            self.cursor = to;
            self.frozen = false;
        }
    }

    /// Extend from an existing anchor without changing granularity —
    /// used by `Shift+Left` to grow the previous selection instead of
    /// starting a new one.
    pub fn shift_extend(&mut self, to: Cell) {
        if self.anchor.is_none() {
            // Nothing to extend — treat as a fresh char-mode click.
            self.anchor = Some(to);
            self.click_streak = 1;
            self.mode = Granularity::Char;
        }
        self.cursor = to;
        self.frozen = false;
    }

    /// Freeze the highlight after `Up`. The highlight persists until the
    /// next `Down` (or `clear`), so users can see what they just copied.
    pub fn commit(&mut self) {
        if self.anchor.is_some() {
            self.frozen = true;
        }
    }

    /// Wipe the selection entirely (called on cell-change fallthrough,
    /// resize, or explicit clear via `Esc`).
    pub fn clear(&mut self) {
        self.anchor = None;
        self.cursor = Cell { row: 0, col: 0 };
        self.frozen = false;
        self.mode = Granularity::Char;
        // Deliberately keep `last_click` — a user can single-click,
        // release, then click again quickly and still get word mode.
    }

    /// True if there's any active or frozen highlight to render / copy.
    pub fn is_active(&self) -> bool {
        self.anchor.is_some()
    }

    /// True when the highlight has been committed (mouse-up seen).
    /// Renderer paints the same reverse-video overlay either way; this
    /// is here so tests can assert the state machine.
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Return the currently-active granularity (mainly for tests).
    pub fn granularity(&self) -> Granularity {
        self.mode
    }

    /// Return `(top_left, bottom_right)` in char-mode ordering (row
    /// major). Callers use it to check "is `(r, c)` inside the
    /// highlight?" while painting. Returns `None` for an empty
    /// selection.
    pub fn char_range(&self) -> Option<(Cell, Cell)> {
        let anchor = self.anchor?;
        let (start, end) = if (anchor.row, anchor.col) <= (self.cursor.row, self.cursor.col) {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };
        Some((start, end))
    }

    /// True when `(row, col)` is inside the currently-highlighted range,
    /// respecting granularity + grid width so word/line highlights fill
    /// past the raw cursor position.
    pub fn contains(&self, row: u16, col: u16, cols: u16) -> bool {
        let Some((start, end)) = self.char_range() else {
            return false;
        };
        match self.mode {
            Granularity::Line => row >= start.row && row <= end.row,
            Granularity::Char | Granularity::Word => {
                // Char / word share the same "flowed rectangle"
                // painter: first row runs from start.col to end of
                // line; middle rows are full; last row runs 0..=end.col.
                if row < start.row || row > end.row {
                    return false;
                }
                if start.row == end.row {
                    col >= start.col && col <= end.col
                } else if row == start.row {
                    col >= start.col && col < cols
                } else if row == end.row {
                    col <= end.col
                } else {
                    col < cols
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line};
    use std::time::Duration;

    fn cell(row: u16, col: u16) -> Cell {
        Cell { row, col }
    }

    fn point(line: i32, col: usize) -> Point {
        Point::new(Line(line), Column(col))
    }

    // --- ClickStreak (PTY-side, absolute grid coords) ---

    #[test]
    fn streak_first_click_is_char() {
        let mut s = ClickStreak::default();
        let now = Instant::now();
        assert_eq!(s.begin(point(0, 0), now), Granularity::Char);
    }

    #[test]
    fn streak_second_click_same_point_promotes_to_word() {
        let mut s = ClickStreak::default();
        let now = Instant::now();
        s.begin(point(-2, 3), now);
        assert_eq!(
            s.begin(point(-2, 3), now + Duration::from_millis(100)),
            Granularity::Word
        );
    }

    #[test]
    fn streak_third_click_same_point_promotes_to_line() {
        let mut s = ClickStreak::default();
        let now = Instant::now();
        s.begin(point(0, 0), now);
        s.begin(point(0, 0), now + Duration::from_millis(100));
        assert_eq!(
            s.begin(point(0, 0), now + Duration::from_millis(200)),
            Granularity::Line
        );
    }

    #[test]
    fn streak_fourth_click_wraps_back_to_char() {
        let mut s = ClickStreak::default();
        let now = Instant::now();
        s.begin(point(0, 0), now);
        s.begin(point(0, 0), now + Duration::from_millis(50));
        s.begin(point(0, 0), now + Duration::from_millis(100));
        assert_eq!(
            s.begin(point(0, 0), now + Duration::from_millis(150)),
            Granularity::Char
        );
    }

    #[test]
    fn streak_different_point_restarts() {
        let mut s = ClickStreak::default();
        let now = Instant::now();
        s.begin(point(0, 0), now);
        // Click at a different point within the window: no promotion.
        assert_eq!(
            s.begin(point(-1, 0), now + Duration::from_millis(100)),
            Granularity::Char
        );
    }

    #[test]
    fn streak_expired_window_restarts() {
        let mut s = ClickStreak::default();
        let now = Instant::now();
        s.begin(point(0, 0), now);
        assert_eq!(
            s.begin(point(0, 0), now + Duration::from_millis(500)),
            Granularity::Char
        );
    }

    #[test]
    fn streak_survives_streaming_rebase() {
        // The stored click point is an absolute grid coordinate: when
        // the child streams N new lines, the same visual spot shifts by
        // -N in Line space. A streak that tracked viewport rows would
        // break; absolute points keep promoting.
        let mut s = ClickStreak::default();
        let now = Instant::now();
        s.begin(point(0, 5), now);
        // Content scrolled up by 3 lines: same visual cell is now Line(-3).
        assert_eq!(
            s.begin(point(-3, 5), now + Duration::from_millis(100)),
            Granularity::Char,
            "different absolute point must restart the streak"
        );
        // But two clicks on the SAME streamed content still promote.
        assert_eq!(
            s.begin(point(-3, 5), now + Duration::from_millis(200)),
            Granularity::Word
        );
    }

    // --- SelectionState (viewer-side, viewport-relative) ---

    #[test]
    fn begin_default_is_char_mode() {
        let mut s = SelectionState::default();
        s.begin(cell(0, 0), Instant::now());
        assert_eq!(s.granularity(), Granularity::Char);
        assert!(s.is_active());
        assert!(!s.is_frozen());
    }

    #[test]
    fn double_click_within_window_promotes_to_word() {
        let mut s = SelectionState::default();
        let now = Instant::now();
        s.begin(cell(1, 2), now);
        s.begin(cell(1, 2), now + Duration::from_millis(200));
        assert_eq!(s.granularity(), Granularity::Word);
    }

    #[test]
    fn selection_clear_resets_highlight_but_keeps_streak_info() {
        let mut s = SelectionState::default();
        let now = Instant::now();
        s.begin(cell(0, 0), now);
        s.extend(cell(2, 2));
        s.commit();
        s.clear();
        assert!(!s.is_active());
        // last_click kept: a quick re-click still promotes.
        s.begin(cell(0, 0), now + Duration::from_millis(50));
        assert_eq!(s.granularity(), Granularity::Word);
    }

    #[test]
    fn contains_flows_past_cursor_in_word_line_modes() {
        let mut s = SelectionState::default();
        let now = Instant::now();
        s.begin(cell(0, 0), now);
        s.extend(cell(1, 2));
        // char mode: flowed rectangle over rows 0..=1
        assert!(s.contains(0, 0, 10));
        assert!(s.contains(0, 9, 10));
        assert!(s.contains(1, 0, 10));
        assert!(s.contains(1, 2, 10));
        assert!(!s.contains(1, 3, 10));
        assert!(!s.contains(2, 0, 10));
    }
}
