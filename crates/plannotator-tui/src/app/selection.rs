//! A mouse selection in document coordinates, and its conversion to a source range.

use std::ops::Range;

use crate::wrap::Row;

/// Anchor and head are (row, column) in document coordinates.
#[derive(Debug, Clone, Copy)]
pub(super) struct Selection {
    anchor: (usize, usize),
    head: (usize, usize),
    pub(super) dragging: bool,
    /// Covers whole rows, whichever columns the anchor and head are on.
    whole_rows: bool,
}

impl Selection {
    #[cfg(test)]
    pub(crate) fn anchor(&self) -> (usize, usize) {
        self.anchor
    }

    pub(super) fn start(at: (usize, usize)) -> Self {
        Self { anchor: at, head: at, dragging: true, whole_rows: false }
    }

    pub(super) fn start_rows(at: (usize, usize)) -> Self {
        Self { anchor: at, head: at, dragging: true, whole_rows: true }
    }

    pub(super) fn finished(anchor: (usize, usize), head: (usize, usize)) -> Self {
        Self { anchor, head, dragging: false, whole_rows: false }
    }

    pub(super) fn set_head(&mut self, head: (usize, usize)) {
        self.head = head;
    }

    /// Start and end in reading order. A whole-row selection reads from column 0 of its first
    /// row; only `columns_on` knows how wide its last row is.
    pub(super) fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let (a, b) =
            if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) };
        if self.whole_rows { ((a.0, 0), b) } else { (a, b) }
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.whole_rows && self.anchor == self.head
    }

    /// Columns of `line` covered by the selection, if any. The end reaches the far edge of
    /// the character under the head, so a wide character is never covered by half.
    pub(super) fn columns_on(&self, row: usize, line: &Row) -> Option<Range<usize>> {
        let (a, b) = self.ordered();
        if row < a.0 || row > b.0 {
            return None;
        }
        if self.whole_rows {
            return (!line.cells.is_empty()).then_some(0..line.cells.len());
        }
        let start = if row == a.0 { line.char_start(a.1) } else { 0 };
        let end = if row == b.0 { line.char_end(b.1) } else { line.cells.len().max(1) };
        (start < end).then_some(start..end)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::*;
    use crate::wrap::clip_line;

    fn line(text: &str) -> Row {
        let offsets = (0..text.chars().count()).map(Some).collect::<Vec<_>>();
        clip_line(&Line::from(text), &offsets, 80)
    }

    #[test]
    fn columns_span_rows_in_either_drag_direction() {
        let line = line("abcdefghij");
        let sel = Selection::finished((5, 2), (3, 5));
        assert_eq!(sel.columns_on(2, &line), None);
        assert_eq!(sel.columns_on(3, &line), Some(5..10));
        assert_eq!(sel.columns_on(4, &line), Some(0..10));
        assert_eq!(sel.columns_on(5, &line), Some(0..3));
    }

    #[test]
    fn a_wide_character_under_the_head_is_covered_whole() {
        let line = line("你好");
        let sel = Selection::finished((0, 0), (0, 2));
        assert_eq!(sel.columns_on(0, &line), Some(0..4), "the second cell of 好 is part of it");
        let half = Selection::finished((0, 3), (0, 3));
        assert_eq!(half.columns_on(0, &line), Some(2..4), "a head inside a wide char snaps out");
    }
}
