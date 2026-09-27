//! Word-wrapping of styled lines to a column width, carrying a source offset per cell.
//!
//! `tui-markdown` emits one `Line` per logical line and never wraps; the layout needs
//! exact row counts and the selection needs to know which source byte sits under each
//! screen column, so both live here. Generic text wrapping — no markdown knowledge.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// One screen row: styled text plus, per column, the source byte it came from.
#[derive(Debug)]
pub(crate) struct Row {
    pub(crate) line: Line<'static>,
    /// `cells[col]` is the source offset shown at that column (wide chars repeat it).
    pub(crate) cells: Vec<Option<usize>>,
}

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    width: usize,
    offset: Option<usize>,
    style: Style,
}

/// Flatten a line into cells, pairing each char with its source offset.
fn cells_of(line: &Line<'_>, offsets: &[Option<usize>]) -> Vec<Cell> {
    let mut offsets = offsets.iter();
    line.spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .map(|(ch, style)| Cell {
            ch,
            width: ch.width().unwrap_or(0),
            offset: offsets.next().copied().flatten(),
            style,
        })
        .collect()
}

impl Row {
    /// Every character of the row with the column it starts at.
    fn chars_with_columns(&self) -> impl Iterator<Item = (usize, char)> + '_ {
        let mut column = 0usize;
        self.line.spans.iter().flat_map(|span| span.content.chars()).map(move |ch| {
            let start = column;
            column += ch.width().unwrap_or(0);
            (start, ch)
        })
    }

    /// The column of the character at or left of `column`. A cursor never sits inside a wide
    /// character: its second cell would make one key press move nothing.
    pub(crate) fn char_start(&self, column: usize) -> usize {
        self.chars_with_columns()
            .map(|(start, _)| start)
            .take_while(|&start| start <= column)
            .last()
            .unwrap_or(0)
    }

    /// The column one character right of `column`, stopping at the row's last character.
    pub(crate) fn next_char(&self, column: usize) -> usize {
        self.chars_with_columns().map(|(start, _)| start).find(|&start| start > column).unwrap_or(column)
    }

    /// The column one character left of `column`, stopping at the row's first character.
    pub(crate) fn prev_char(&self, column: usize) -> usize {
        self.chars_with_columns()
            .map(|(start, _)| start)
            .take_while(|&start| start < column)
            .last()
            .unwrap_or(0)
    }

    /// How many cells the character starting at `column` occupies; 1 if none starts there.
    pub(crate) fn char_width(&self, column: usize) -> usize {
        self.chars_with_columns()
            .find(|&(start, _)| start == column)
            .map_or(1, |(_, ch)| ch.width().unwrap_or(1).max(1))
    }

    /// `column`, clamped to the row and snapped to the start of the character there.
    pub(crate) fn clamp_column(&self, column: usize) -> usize {
        self.char_start(column.min(self.cells.len().saturating_sub(1)))
    }

    /// The column just past the character at or left of `column`: the other cell of a wide
    /// character belongs to it, so a range ending here covers a whole character.
    pub(crate) fn char_end(&self, column: usize) -> usize {
        let start = self.char_start(column);
        start + self.char_width(start)
    }

    /// The column a word motion lands on: the next (+1) or previous (-1) word start on this
    /// row, a word being a run of non-whitespace. Stops at the row's ends.
    pub(crate) fn word_start(&self, column: usize, direction: i64) -> usize {
        let chars: Vec<(usize, char)> = self.chars_with_columns().collect();
        let from = chars.iter().rposition(|&(start, _)| start <= column).unwrap_or(0);
        let is_start = |index: usize| {
            let word = chars.get(index).is_some_and(|&(_, ch)| !ch.is_whitespace());
            let after_space = index == 0 || chars.get(index - 1).is_some_and(|&(_, ch)| ch.is_whitespace());
            word && after_space
        };
        let index = if direction > 0 {
            (from + 1..chars.len()).find(|&i| is_start(i)).unwrap_or(chars.len().saturating_sub(1))
        } else {
            (0..from).rev().find(|&i| is_start(i)).unwrap_or(0)
        };
        chars.get(index).map_or(column, |&(start, _)| start)
    }
}

/// Turn accumulated cells into a row, merging same-style runs into spans.
fn finish_row(mut cells: Vec<Cell>, line_style: Style) -> Row {
    while cells.last().is_some_and(|c| c.ch.is_whitespace()) {
        cells.pop();
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut columns: Vec<Option<usize>> = Vec::new();
    for cell in &cells {
        match spans.last_mut() {
            Some(last) if last.style == cell.style => last.content.to_mut().push(cell.ch),
            _ => spans.push(Span::styled(cell.ch.to_string(), cell.style)),
        }
        columns.extend(std::iter::repeat_n(cell.offset, cell.width));
    }
    Row { line: Line::from(spans).style(line_style), cells: columns }
}

/// Wrap one logical line into as many rows as needed for `width` columns.
/// `offsets` has one entry per char of the line. An empty line yields one empty row.
pub(crate) fn wrap_line(line: &Line<'_>, offsets: &[Option<usize>], width: usize) -> Vec<Row> {
    let width = width.max(1);
    let cells = cells_of(line, offsets);
    let mut rows: Vec<Row> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut current_width = 0usize;

    // Tokens are unbreakable runs: a word, or a run of whitespace.
    let mut rest = cells.as_slice();
    while let Some(first) = rest.first() {
        let is_space = first.ch.is_whitespace();
        let len = rest.iter().take_while(|c| c.ch.is_whitespace() == is_space).count();
        let (token, tail) = rest.split_at(len);
        rest = tail;
        let token_width: usize = token.iter().map(|c| c.width).sum();

        // Whitespace at a row start is dropped, except leading indentation on the first row.
        if is_space && current.is_empty() && !rows.is_empty() {
            continue;
        }
        if current_width + token_width <= width {
            current.extend_from_slice(token);
            current_width += token_width;
            continue;
        }
        if !current.is_empty() {
            rows.push(finish_row(std::mem::take(&mut current), line.style));
            current_width = 0;
            if is_space {
                continue;
            }
        }
        if token_width <= width {
            current.extend_from_slice(token);
            current_width = token_width;
        } else {
            // Token wider than a row: hard-split by cells.
            for cell in token {
                if current_width + cell.width > width && !current.is_empty() {
                    rows.push(finish_row(std::mem::take(&mut current), line.style));
                    current_width = 0;
                }
                current.push(*cell);
                current_width += cell.width;
            }
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(finish_row(current, line.style));
    }
    rows
}

/// Keep one row; clip anything past `width` (code and tables keep their columns).
pub(crate) fn clip_line(line: &Line<'_>, offsets: &[Option<usize>], width: usize) -> Row {
    let mut used = 0usize;
    let kept = cells_of(line, offsets)
        .into_iter()
        .take_while(|cell| {
            let fits = used + cell.width <= width;
            if fits {
                used += cell.width;
            }
            fits
        })
        .collect();
    finish_row(kept, line.style)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing, reason = "tests assert by panicking")]
mod tests {
    use super::*;
    use ratatui::style::{Modifier, Stylize};

    fn plain(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.line.to_string()).collect()
    }

    fn identity(n: usize) -> Vec<Option<usize>> {
        (0..n).map(Some).collect()
    }

    #[test]
    fn wraps_on_word_boundaries_and_keeps_offsets() {
        let line = Line::from("the quick brown fox jumps");
        let rows = wrap_line(&line, &identity(25), 10);
        assert_eq!(plain(&rows), ["the quick", "brown fox", "jumps"]);
        assert_eq!(rows.first().map(|r| r.cells.clone()), Some(identity(9)));
        assert_eq!(rows.get(1).and_then(|r| r.cells.first().copied()), Some(Some(10)));
        assert_eq!(rows.get(2).and_then(|r| r.cells.first().copied()), Some(Some(20)));
    }

    #[test]
    fn styles_survive_wrapping_at_span_and_line_level() {
        let line = Line::from(vec!["plain ".into(), "bold text here".bold()]).style(Style::new().italic());
        let rows = wrap_line(&line, &identity(20), 11);
        assert_eq!(plain(&rows), ["plain bold", "text here"]);
        let second = rows.get(1).expect("two rows");
        assert!(second.line.style.add_modifier.contains(Modifier::ITALIC));
        assert!(second.line.spans.first().is_some_and(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn hard_splits_long_tokens_and_wide_chars_take_two_cells() {
        assert_eq!(
            plain(&wrap_line(&Line::from("abcdefghijkl"), &identity(12), 5)),
            ["abcde", "fghij", "kl"]
        );
        let row = clip_line(&Line::from("日本"), &identity(2), 10);
        assert_eq!(row.cells, [Some(0), Some(0), Some(1), Some(1)]);
    }

    #[test]
    fn empty_line_is_one_row() {
        assert_eq!(wrap_line(&Line::from(""), &[], 20).len(), 1);
    }

    #[test]
    fn a_wide_character_is_one_step_not_two_cells() {
        // 日 at column 0 (two cells), x at 2, 本 at 3 (two cells).
        let row = clip_line(&Line::from("日x本"), &identity(3), 20);
        assert_eq!(row.cells, [Some(0), Some(0), Some(1), Some(2), Some(2)]);
        assert_eq!((row.next_char(0), row.next_char(2)), (2, 3), "one press per character");
        assert_eq!(row.next_char(3), 3, "right stops on the last character");
        assert_eq!((row.prev_char(4), row.prev_char(3), row.prev_char(0)), (3, 2, 0));
        assert_eq!((row.char_start(1), row.char_start(4)), (0, 3), "a cell inside a wide char snaps left");
        assert_eq!((row.char_width(0), row.char_width(2)), (2, 1));
        assert_eq!(row.clamp_column(99), 3, "the cursor sits on the last character, not its tail");
    }

    #[test]
    fn word_motion_lands_on_word_starts_in_columns() {
        // Columns: 日 0, 本 2, space 4, g 5, o 6, space 7, n 8, o 9, w 10.
        let row = clip_line(&Line::from("日本 go now"), &identity(9), 20);
        assert_eq!((row.word_start(0, 1), row.word_start(5, 1)), (5, 8));
        assert_eq!((row.word_start(8, -1), row.word_start(5, -1)), (5, 0));
        assert_eq!(row.word_start(11, 1), 10, "forward stops on the last character");
    }
}
