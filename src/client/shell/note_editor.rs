//! Multi-line markdown note editor. Pure state: the Notes view owns drawing,
//! autosave (driven by `revision`) and clipboard writes (`EditorOutcome::Copy`).

// The Notes view wires this editor in next; until then only the tests call it.
#![allow(dead_code)]

use super::text_editor::{word_boundary, word_class};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const UNDO_LIMIT: usize = 200;
const INDENT: &str = "  ";

/// (line index, byte offset on a grapheme boundary of that line).
type Pos = (usize, usize);

/// One soft-wrapped screen row: the byte range `start..end` of `line`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VisualRow {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditorOutcome {
    Changed,
    Moved,
    /// Text for the clipboard. Ctrl+X also deleted it, so `revision` moved.
    Copy(String),
    Unhandled,
}

#[derive(Clone, Debug)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Pos,
}

#[derive(Clone, Debug)]
pub(crate) struct NoteEditor {
    /// Never empty; an empty note is one empty line.
    lines: Vec<String>,
    cursor: Pos,
    anchor: Option<Pos>,
    /// Sticky display column for Up/Down through shorter rows.
    preferred_x: Option<usize>,
    /// First visible visual row.
    scroll: usize,
    revision: u64,
    // ponytail: whole-note snapshots, fine for notes; switch to edit diffs if
    // multi-megabyte notes ever appear.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The last edit was word-character typing that the next one may join.
    typing: bool,
}

/// Display cells for one grapheme. Tabs count as `INDENT`, so the view must
/// draw them as that many spaces.
pub(crate) fn grapheme_cells(grapheme: &str) -> usize {
    if grapheme == "\t" {
        INDENT.len()
    } else {
        grapheme.width()
    }
}

fn cells(text: &str) -> usize {
    text.graphemes(true).map(grapheme_cells).sum()
}

fn split_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split(['\n', '\r'])
        .map(str::to_owned)
        .collect()
}

/// Typed and pasted text: CRLF and CR become newlines, tabs become `INDENT`,
/// other control characters are dropped.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str(INDENT),
            ch if !ch.is_control() => out.push(ch),
            _ => {}
        }
    }
    out
}

fn floor_boundary(line: &str, byte: usize) -> usize {
    if byte >= line.len() {
        return line.len();
    }
    line.grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|index| *index <= byte)
        .last()
        .unwrap_or(0)
}

fn ceil_boundary(line: &str, byte: usize) -> usize {
    line.grapheme_indices(true)
        .map(|(index, _)| index)
        .find(|index| *index >= byte)
        .unwrap_or(line.len())
}

fn prev_boundary(line: &str, byte: usize) -> usize {
    line[..byte]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(line: &str, byte: usize) -> usize {
    byte + line[byte..].graphemes(true).next().map_or(0, str::len)
}

/// Soft-wraps one line greedily, breaking after the last whitespace when the
/// next word overflows and hard-breaking words wider than the row. Whitespace
/// hangs past the edge so a wrapped row never starts with the gap. A grapheme
/// wider than the row gets a row of its own.
fn wrap(line: &str, width: usize, mut push: impl FnMut(usize, usize)) {
    let width = width.max(1);
    let (mut start, mut col, mut space_end) = (0, 0, None);
    for (index, grapheme) in line.grapheme_indices(true) {
        let size = grapheme_cells(grapheme);
        let blank = grapheme.chars().all(char::is_whitespace);
        while !blank && index > start && col + size > width {
            let at = space_end.filter(|end| *end > start).unwrap_or(index);
            push(start, at);
            start = at;
            col = cells(&line[start..index]);
            space_end = None;
        }
        col += size;
        if blank {
            space_end = Some(index + grapheme.len());
        }
    }
    push(start, line.len());
}

/// Row holding `pos`. A wrap point belongs to the later row.
fn row_index(rows: &[VisualRow], (line, byte): Pos) -> usize {
    rows.partition_point(|row| (row.line, row.start) <= (line, byte))
        .saturating_sub(1)
}

fn last_row_of_line(rows: &[VisualRow], index: usize) -> bool {
    !matches!(
        (rows.get(index), rows.get(index + 1)),
        (Some(row), Some(next)) if next.line == row.line
    )
}

/// Markdown list or quote prefix: (its byte length, the prefix a continued
/// line starts with). Checkboxes continue unchecked; numbers increment.
fn list_prefix(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let rest = &line[indent..];
    let (marker, next) = if ["- [ ] ", "- [x] ", "- [X] "]
        .iter()
        .any(|box_| rest.starts_with(box_))
    {
        (6, "- [ ] ".to_owned())
    } else if let Some(bullet) = ["- ", "* ", "> "].iter().find(|b| rest.starts_with(*b)) {
        (2, (*bullet).to_owned())
    } else {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !rest[digits..].starts_with(". ") {
            return None;
        }
        let number: u64 = rest[..digits].parse().ok()?;
        (digits + 2, format!("{}. ", number.checked_add(1)?))
    };
    Some((indent + marker, format!("{}{next}", &line[..indent])))
}

impl NoteEditor {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            lines: split_lines(text),
            cursor: (0, 0),
            anchor: None,
            preferred_x: None,
            scroll: 0,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            typing: false,
        }
    }

    /// Lines joined with '\n'; a trailing newline survives as a final empty line.
    pub(crate) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(crate) fn lines(&self) -> &[String] {
        &self.lines
    }

    /// External reload: keeps the cursor (clamped), drops selection and history.
    pub(crate) fn set_text(&mut self, text: &str) {
        let lines = split_lines(text);
        if lines != self.lines {
            self.lines = lines;
            self.revision += 1;
        }
        self.anchor = None;
        self.preferred_x = None;
        self.undo.clear();
        self.redo.clear();
        self.typing = false;
        self.cursor = self.clamp(self.cursor);
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn scroll(&self) -> usize {
        self.scroll
    }

    pub(crate) fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    /// Moves the cursor, snapping back to a grapheme boundary, and clears selection.
    pub(crate) fn set_cursor(&mut self, line: usize, byte: usize) {
        let line = line.min(self.lines.len() - 1);
        self.cursor = (line, floor_boundary(&self.lines[line], byte));
        self.anchor = None;
        self.preferred_x = None;
        self.typing = false;
    }

    /// Ordered (start, end) of a non-empty selection.
    pub(crate) fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.anchor.filter(|anchor| *anchor != self.cursor)?;
        Some(if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    pub(crate) fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        if start.0 == end.0 {
            return Some(self.lines[start.0][start.1..end.1].to_owned());
        }
        let mut out = self.lines[start.0][start.1..].to_owned();
        for line in &self.lines[start.0 + 1..end.0] {
            out.push('\n');
            out.push_str(line);
        }
        out.push('\n');
        out.push_str(&self.lines[end.0][..end.1]);
        Some(out)
    }

    pub(crate) fn layout(&self, width: u16) -> Vec<VisualRow> {
        let mut rows = Vec::with_capacity(self.lines.len());
        for (line, text) in self.lines.iter().enumerate() {
            wrap(text, usize::from(width), |start, end| {
                rows.push(VisualRow { line, start, end })
            });
        }
        rows
    }

    /// Cursor (visual row, display column). The column can reach `width` at
    /// the end of a full row or inside hanging whitespace; the view clamps.
    pub(crate) fn cursor_visual(&self, width: u16) -> (usize, usize) {
        self.visual(&self.layout(width))
    }

    /// Text position under a visual cell, clamped to the document. Past the end
    /// of a wrapped row lands before the next row's first grapheme.
    pub(crate) fn visual_to_position(&self, row: usize, x: usize, width: u16) -> (usize, usize) {
        self.position_in(&self.layout(width), row, x)
    }

    pub(crate) fn ensure_cursor_visible(&mut self, width: u16, height: u16) {
        let rows = self.layout(width);
        let (row, _) = self.visual(&rows);
        let height = usize::from(height.max(1));
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + height {
            self.scroll = row + 1 - height;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(height));
    }

    /// Mouse wheel scrolling; does not move the cursor.
    pub(crate) fn scroll_by(&mut self, delta: isize, width: u16, height: u16) {
        let max = self
            .layout(width)
            .len()
            .saturating_sub(usize::from(height.max(1)));
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    /// Inserts at the cursor, replacing any selection. Used for paste.
    pub(crate) fn insert(&mut self, text: &str) -> bool {
        self.type_text(text, false)
    }

    /// Click on document visual `row` (scroll already applied by the caller).
    /// `extend` keeps the anchor for shift-click and drag.
    pub(crate) fn click(&mut self, row: usize, x: usize, width: u16, extend: bool) {
        let pos = self.visual_to_position(row, x, width);
        self.move_to(pos, extend);
        self.preferred_x = None;
    }

    /// Double-click: selects the word, punctuation run or whitespace run under the cell.
    pub(crate) fn select_word_at(&mut self, row: usize, x: usize, width: u16) {
        let (line_index, byte) = self.visual_to_position(row, x, width);
        let line = &self.lines[line_index];
        let byte = if byte == line.len() {
            prev_boundary(line, byte)
        } else {
            byte
        };
        let Some(class) = line[byte..].graphemes(true).next().map(word_class) else {
            self.move_to((line_index, byte), false);
            return;
        };
        let start = line[..byte]
            .grapheme_indices(true)
            .rev()
            .take_while(|(_, grapheme)| word_class(grapheme) == class)
            .last()
            .map_or(byte, |(index, _)| index);
        let end = byte
            + line[byte..]
                .graphemes(true)
                .take_while(|grapheme| word_class(grapheme) == class)
                .map(str::len)
                .sum::<usize>();
        self.move_to((line_index, start), false);
        self.move_to((line_index, end), true);
        self.preferred_x = None;
    }

    pub(crate) fn handle_key(
        &mut self,
        key: &crate::input::TerminalKey,
        width: u16,
        height: u16,
    ) -> EditorOutcome {
        if key.kind == KeyEventKind::Release {
            return EditorOutcome::Unhandled;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let extend = modifiers.contains(KeyModifiers::SHIFT);
        let base = modifiers.difference(KeyModifiers::SHIFT);
        let plain = base.is_empty();
        let ctrl = base == KeyModifiers::CONTROL;
        let word = ctrl || base == KeyModifiers::ALT;

        let movement = match code {
            KeyCode::Left if plain && !extend => Some(
                self.selection()
                    .map_or_else(|| self.left(self.cursor), |(start, _)| start),
            ),
            KeyCode::Right if plain && !extend => Some(
                self.selection()
                    .map_or_else(|| self.right(self.cursor), |(_, end)| end),
            ),
            KeyCode::Left if plain => Some(self.left(self.cursor)),
            KeyCode::Right if plain => Some(self.right(self.cursor)),
            KeyCode::Left if word => Some(self.word_left(self.cursor)),
            KeyCode::Right if word => Some(self.word_right(self.cursor)),
            KeyCode::Home if plain => Some(self.home(&self.layout(width))),
            KeyCode::End if plain => Some(self.end(&self.layout(width))),
            KeyCode::Home if ctrl => Some((0, 0)),
            KeyCode::End if ctrl => Some(self.doc_end()),
            _ => None,
        };
        if let Some(pos) = movement {
            self.move_to(pos, extend);
            self.preferred_x = None;
            self.ensure_cursor_visible(width, height);
            return EditorOutcome::Moved;
        }
        if plain
            && matches!(
                code,
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
            )
        {
            let step = if matches!(code, KeyCode::Up | KeyCode::Down) {
                1
            } else {
                usize::from(height.max(1))
            };
            let down = matches!(code, KeyCode::Down | KeyCode::PageDown);
            let (pos, x) = self.vertical(&self.layout(width), step, down);
            self.move_to(pos, extend);
            self.preferred_x = Some(x);
            self.ensure_cursor_visible(width, height);
            return EditorOutcome::Moved;
        }

        let changed = match code {
            KeyCode::Enter if plain => self.newline(),
            KeyCode::Backspace if plain => self.delete_back(false),
            KeyCode::Backspace if word => self.delete_back(true),
            KeyCode::Delete if plain => self.delete_forward(false),
            KeyCode::Delete if ctrl => self.delete_forward(true),
            KeyCode::Tab if plain && !extend => self.tab(),
            KeyCode::BackTab if plain => {
                let (first, last) = self
                    .selected_lines()
                    .unwrap_or((self.cursor.0, self.cursor.0));
                self.shift_lines(first, last, true)
            }
            KeyCode::Char(ch) if ctrl => {
                let shifted = extend || ch.is_ascii_uppercase();
                match ch.to_ascii_lowercase() {
                    'a' if !shifted => {
                        self.anchor = Some((0, 0));
                        self.cursor = self.doc_end();
                        self.typing = false;
                        self.preferred_x = None;
                        false
                    }
                    'c' if !shifted => {
                        return self
                            .selected_text()
                            .map_or(EditorOutcome::Unhandled, EditorOutcome::Copy);
                    }
                    'x' if !shifted => {
                        let (Some(text), Some((start, end))) =
                            (self.selected_text(), self.selection())
                        else {
                            return EditorOutcome::Unhandled;
                        };
                        self.remove(start, end);
                        self.ensure_cursor_visible(width, height);
                        return EditorOutcome::Copy(text);
                    }
                    'z' if !shifted => self.undo(),
                    'z' | 'y' => self.redo(),
                    _ => return EditorOutcome::Unhandled,
                }
            }
            KeyCode::Char(_) => {
                // Host-reported text is authoritative, including AltGr/composition.
                let text = key
                    .generated_text
                    .clone()
                    .filter(|text| !text.is_empty())
                    .or_else(|| crate::input::keybind_help_text_char(key).map(String::from));
                let Some(text) = text else {
                    return EditorOutcome::Unhandled;
                };
                let coalesce = text.chars().count() == 1 && !text.chars().any(char::is_whitespace);
                self.type_text(&text, coalesce)
            }
            _ => return EditorOutcome::Unhandled,
        };
        self.ensure_cursor_visible(width, height);
        if changed {
            EditorOutcome::Changed
        } else {
            EditorOutcome::Moved
        }
    }

    fn clamp(&self, (line, byte): Pos) -> Pos {
        let line = line.min(self.lines.len() - 1);
        (line, ceil_boundary(&self.lines[line], byte))
    }

    fn doc_end(&self) -> Pos {
        let last = self.lines.len() - 1;
        (last, self.lines[last].len())
    }

    fn move_to(&mut self, pos: Pos, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
        self.typing = false;
    }

    fn left(&self, (line, byte): Pos) -> Pos {
        if byte > 0 {
            (line, prev_boundary(&self.lines[line], byte))
        } else if line > 0 {
            (line - 1, self.lines[line - 1].len())
        } else {
            (line, byte)
        }
    }

    fn right(&self, (line, byte): Pos) -> Pos {
        if byte < self.lines[line].len() {
            (line, next_boundary(&self.lines[line], byte))
        } else if line + 1 < self.lines.len() {
            (line + 1, 0)
        } else {
            (line, byte)
        }
    }

    fn word_left(&self, (line, byte): Pos) -> Pos {
        if byte == 0 {
            self.left((line, byte))
        } else {
            (line, word_boundary(&self.lines[line], byte, true))
        }
    }

    fn word_right(&self, (line, byte): Pos) -> Pos {
        if byte == self.lines[line].len() {
            self.right((line, byte))
        } else {
            (line, word_boundary(&self.lines[line], byte, false))
        }
    }

    fn visual(&self, rows: &[VisualRow]) -> (usize, usize) {
        let index = row_index(rows, self.cursor);
        let x = rows.get(index).map_or(0, |row| {
            cells(&self.lines[row.line][row.start..self.cursor.1])
        });
        (index, x)
    }

    fn position_in(&self, rows: &[VisualRow], row: usize, x: usize) -> Pos {
        let index = row.min(rows.len().saturating_sub(1));
        let Some(row) = rows.get(index) else {
            return (0, 0);
        };
        let line = &self.lines[row.line];
        let mut col = 0;
        for (offset, grapheme) in line[row.start..row.end].grapheme_indices(true) {
            col += grapheme_cells(grapheme);
            if col > x {
                return (row.line, row.start + offset);
            }
        }
        if last_row_of_line(rows, index) {
            (row.line, row.end)
        } else {
            (row.line, prev_boundary(line, row.end).max(row.start))
        }
    }

    /// Target of moving `step` visual rows, and the sticky column used.
    fn vertical(&self, rows: &[VisualRow], step: usize, down: bool) -> (Pos, usize) {
        let (row, x) = self.visual(rows);
        let x = self.preferred_x.unwrap_or(x);
        let target = if down {
            row.checked_add(step).filter(|target| *target < rows.len())
        } else {
            row.checked_sub(step)
        };
        let pos = match target {
            Some(target) => self.position_in(rows, target, x),
            None if down => self.doc_end(),
            None => (0, 0),
        };
        (pos, x)
    }

    /// Visual row start; pressed again on a line's first row, the first non-space.
    fn home(&self, rows: &[VisualRow]) -> Pos {
        let Some(row) = rows.get(row_index(rows, self.cursor)) else {
            return self.cursor;
        };
        if self.cursor != (row.line, row.start) || row.start != 0 {
            return (row.line, row.start);
        }
        let line = &self.lines[row.line];
        let indent = line.len() - line.trim_start().len();
        (row.line, floor_boundary(line, indent))
    }

    fn end(&self, rows: &[VisualRow]) -> Pos {
        let index = row_index(rows, self.cursor);
        let Some(row) = rows.get(index) else {
            return self.cursor;
        };
        if last_row_of_line(rows, index) {
            (row.line, row.end)
        } else {
            (
                row.line,
                prev_boundary(&self.lines[row.line], row.end).max(row.start),
            )
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
        }
    }

    /// Records the pre-edit state. Consecutive word-character typing joins one
    /// undo step; whitespace, newlines and cursor moves start a new one.
    fn checkpoint(&mut self, coalesce: bool) {
        if !(coalesce && self.typing) {
            self.undo.push(self.snapshot());
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.typing = coalesce;
    }

    /// After every content change: bump the revision and repair positions,
    /// since an edit can merge graphemes across its boundary.
    fn changed(&mut self) {
        self.revision += 1;
        self.preferred_x = None;
        self.cursor = self.clamp(self.cursor);
        self.anchor = self.anchor.map(|anchor| self.clamp(anchor));
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        self.cursor = snapshot.cursor;
        self.anchor = None;
        self.typing = false;
        self.changed();
    }

    fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.restore(snapshot);
        true
    }

    fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snapshot());
        self.restore(snapshot);
        true
    }

    /// Removes `start..end` (ordered) without recording history.
    fn delete_range(&mut self, start: Pos, end: Pos) {
        if start.0 == end.0 {
            self.lines[start.0].replace_range(start.1..end.1, "");
        } else {
            let tail = self.lines[end.0][end.1..].to_owned();
            self.lines.drain(start.0 + 1..=end.0);
            let line = &mut self.lines[start.0];
            line.truncate(start.1);
            line.push_str(&tail);
        }
        self.cursor = start;
        self.anchor = None;
    }

    /// Inserts already normalized text at the cursor without recording history.
    fn insert_raw(&mut self, text: &str) {
        let (line, byte) = self.cursor;
        let tail = self.lines[line].split_off(byte);
        let head = std::mem::take(&mut self.lines[line]);
        let mut inserted: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let last = inserted.len() - 1;
        inserted[0].insert_str(0, &head);
        let cursor = inserted[last].len();
        inserted[last].push_str(&tail);
        self.lines.splice(line..=line, inserted);
        self.cursor = (line + last, cursor);
    }

    fn type_text(&mut self, text: &str, coalesce: bool) -> bool {
        let text = normalize(text);
        if text.is_empty() {
            return false;
        }
        let selection = self.selection();
        self.checkpoint(coalesce && selection.is_none());
        if let Some((start, end)) = selection {
            self.delete_range(start, end);
        }
        self.anchor = None;
        self.insert_raw(&text);
        self.changed();
        true
    }

    fn remove(&mut self, start: Pos, end: Pos) -> bool {
        if start == end {
            return false;
        }
        self.checkpoint(false);
        self.delete_range(start, end);
        self.changed();
        true
    }

    fn delete_back(&mut self, word: bool) -> bool {
        if let Some((start, end)) = self.selection() {
            return self.remove(start, end);
        }
        let start = if word {
            self.word_left(self.cursor)
        } else {
            self.left(self.cursor)
        };
        self.remove(start, self.cursor)
    }

    fn delete_forward(&mut self, word: bool) -> bool {
        if let Some((start, end)) = self.selection() {
            return self.remove(start, end);
        }
        let end = if word {
            self.word_right(self.cursor)
        } else {
            self.right(self.cursor)
        };
        self.remove(self.cursor, end)
    }

    /// Enter: continues a markdown list or quote, or ends it on an empty item.
    fn newline(&mut self) -> bool {
        self.checkpoint(false);
        if let Some((start, end)) = self.selection() {
            self.delete_range(start, end);
        }
        self.anchor = None;
        let (line, byte) = self.cursor;
        match list_prefix(&self.lines[line]) {
            Some((len, _)) if len == self.lines[line].len() => {
                self.lines[line].clear();
                self.cursor = (line, 0);
            }
            Some((len, next)) if byte >= len => self.insert_raw(&format!("\n{next}")),
            _ => self.insert_raw("\n"),
        }
        self.changed();
        true
    }

    /// Lines a selection touches; one ending at column 0 excludes that line.
    fn selected_lines(&self) -> Option<(usize, usize)> {
        let (start, end) = self.selection()?;
        let last = if end.0 > start.0 && end.1 == 0 {
            end.0 - 1
        } else {
            end.0
        };
        Some((start.0, last))
    }

    fn tab(&mut self) -> bool {
        if let Some((first, last)) = self.selected_lines().filter(|(f, l)| f != l) {
            return self.shift_lines(first, last, false);
        }
        if self.selection().is_none() && list_prefix(&self.lines[self.cursor.0]).is_some() {
            return self.shift_lines(self.cursor.0, self.cursor.0, false);
        }
        self.type_text(INDENT, false)
    }

    /// Indents lines by `INDENT`, or outdents up to two spaces or one tab,
    /// keeping cursor and selection on the same text.
    fn shift_lines(&mut self, first: usize, last: usize, outdent: bool) -> bool {
        let amounts: Vec<usize> = self.lines[first..=last]
            .iter()
            .map(|line| match (outdent, line.starts_with('\t')) {
                (false, _) => INDENT.len(),
                (true, true) => 1,
                (true, false) => line.bytes().take(2).take_while(|b| *b == b' ').count(),
            })
            .collect();
        if amounts.iter().all(|amount| *amount == 0) {
            return false;
        }
        self.checkpoint(false);
        for (line, amount) in (first..=last).zip(amounts) {
            if outdent {
                self.lines[line].replace_range(..amount, "");
            } else {
                self.lines[line].insert_str(0, INDENT);
            }
            for pos in std::iter::once(&mut self.cursor).chain(self.anchor.as_mut()) {
                if pos.0 == line {
                    pos.1 = if outdent {
                        pos.1.saturating_sub(amount)
                    } else {
                        pos.1 + amount
                    };
                }
            }
        }
        self.changed();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::TerminalKey;
    use KeyCode::*;

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALT: KeyModifiers = KeyModifiers::ALT;

    fn on_boundary(editor: &NoteEditor, (line, byte): Pos) -> bool {
        let text = &editor.lines[line];
        byte == text.len() || text.grapheme_indices(true).any(|(i, _)| i == byte)
    }

    fn press_at(
        editor: &mut NoteEditor,
        code: KeyCode,
        mods: KeyModifiers,
        width: u16,
    ) -> EditorOutcome {
        let outcome = editor.handle_key(&TerminalKey::new(code, mods), width, 5);
        assert!(on_boundary(editor, editor.cursor), "{code:?} {mods:?}");
        assert!(editor
            .anchor
            .is_none_or(|anchor| on_boundary(editor, anchor)));
        outcome
    }

    fn press(editor: &mut NoteEditor, code: KeyCode, mods: KeyModifiers) -> EditorOutcome {
        press_at(editor, code, mods, 40)
    }

    fn type_str(editor: &mut NoteEditor, text: &str) {
        for ch in text.chars() {
            let code = if ch == '\n' { Enter } else { Char(ch) };
            press(editor, code, NONE);
        }
    }

    fn at_end(text: &str) -> NoteEditor {
        let mut editor = NoteEditor::new(text);
        press(&mut editor, End, CTRL);
        editor
    }

    fn rows(text: &str, width: u16) -> Vec<(usize, usize)> {
        let editor = NoteEditor::new(text);
        editor
            .layout(width)
            .iter()
            .map(|r| (r.start, r.end))
            .collect()
    }

    #[test]
    fn typing_coalesces_into_undo_steps() {
        let mut editor = NoteEditor::new("");
        type_str(&mut editor, "hello world");
        assert_eq!(editor.revision(), 11);
        for expected in ["hello ", "hello", ""] {
            assert_eq!(press(&mut editor, Char('z'), CTRL), EditorOutcome::Changed);
            assert_eq!(editor.text(), expected);
        }
        assert_eq!(press(&mut editor, Char('z'), CTRL), EditorOutcome::Moved);
        press(&mut editor, Char('y'), CTRL);
        assert_eq!(editor.text(), "hello");
        press(&mut editor, Char('Z'), CTRL | SHIFT);
        assert_eq!(editor.text(), "hello ");

        // A cursor move breaks the group.
        let mut editor = NoteEditor::new("");
        type_str(&mut editor, "ab");
        press(&mut editor, Left, NONE);
        type_str(&mut editor, "c");
        press(&mut editor, Char('z'), CTRL);
        assert_eq!(editor.text(), "ab");

        // History is capped.
        let mut editor = NoteEditor::new("");
        for _ in 0..250 {
            editor.insert("x");
        }
        while press(&mut editor, Char('z'), CTRL) == EditorOutcome::Changed {}
        assert_eq!(editor.text(), "x".repeat(50));
    }

    #[test]
    fn enter_continues_and_ends_markdown_lists() {
        for (line, next) in [
            ("- [ ] task", "- [ ] "),
            ("- [x] done", "- [ ] "),
            ("- item", "- "),
            ("* item", "* "),
            ("> quote", "> "),
            ("  - nested", "  - "),
            ("\t* tabbed", "\t* "),
            ("9. nine", "10. "),
        ] {
            let mut editor = at_end(line);
            press(&mut editor, Enter, NONE);
            assert_eq!(editor.text(), format!("{line}\n{next}"));
            assert_eq!(editor.cursor(), (1, next.len()));
            press(&mut editor, Enter, NONE);
            assert_eq!(
                editor.text(),
                format!("{line}\n"),
                "empty item ends {line:?}"
            );
            assert_eq!(editor.cursor(), (1, 0));
        }
        let mut editor = at_end("1. one");
        type_str(&mut editor, "\ntwo\nthree");
        assert_eq!(editor.text(), "1. one\n2. two\n3. three");
        // Splitting mid-item continues; inside the prefix it is a plain newline.
        let mut editor = NoteEditor::new("- ab");
        editor.set_cursor(0, 3);
        press(&mut editor, Enter, NONE);
        assert_eq!(editor.text(), "- a\n- b");
        let mut editor = NoteEditor::new("- ab");
        press(&mut editor, Enter, NONE);
        assert_eq!(editor.text(), "\n- ab");
        for plain in ["-item", "1.x", "99999999999999999999999. x", ""] {
            let mut editor = at_end(plain);
            press(&mut editor, Enter, NONE);
            assert_eq!(editor.text(), format!("{plain}\n"));
        }
    }

    #[test]
    fn deletion_joins_lines_and_words() {
        let mut editor = NoteEditor::new("ab\ncd");
        editor.set_cursor(1, 0);
        press(&mut editor, Backspace, NONE);
        assert_eq!((editor.text().as_str(), editor.cursor()), ("abcd", (0, 2)));
        let mut editor = NoteEditor::new("ab\ncd");
        editor.set_cursor(0, 2);
        press(&mut editor, Delete, NONE);
        assert_eq!(editor.text(), "abcd");
        let mut editor = at_end("foo bar");
        press(&mut editor, Backspace, CTRL);
        assert_eq!(editor.text(), "foo ");
        press(&mut editor, Backspace, ALT);
        assert_eq!(editor.text(), "");
        assert_eq!(press(&mut editor, Backspace, NONE), EditorOutcome::Moved);
        let mut editor = NoteEditor::new("foo bar");
        press(&mut editor, Delete, CTRL);
        assert_eq!(editor.text(), " bar");
        let mut editor = at_end("e\u{301}中👩‍💻");
        for expected in ["e\u{301}中", "e\u{301}", ""] {
            press(&mut editor, Backspace, NONE);
            assert_eq!(editor.text(), expected);
        }
    }

    #[test]
    fn word_and_line_movement() {
        let mut editor = NoteEditor::new("one two\nthree");
        for expected in [(0, 3), (0, 7), (1, 0), (1, 5), (1, 5)] {
            press(&mut editor, Right, CTRL);
            assert_eq!(editor.cursor(), expected);
        }
        for expected in [(1, 0), (0, 7), (0, 4), (0, 0), (0, 0)] {
            press(&mut editor, Left, ALT);
            assert_eq!(editor.cursor(), expected);
        }
        press(&mut editor, End, NONE);
        press(&mut editor, Right, NONE);
        assert_eq!(editor.cursor(), (1, 0));
        press(&mut editor, Left, NONE);
        assert_eq!(editor.cursor(), (0, 7));
        // Home toggles between line start and first non-space.
        let mut editor = at_end("  indented");
        press(&mut editor, Home, NONE);
        assert_eq!(editor.cursor(), (0, 0));
        press(&mut editor, Home, NONE);
        assert_eq!(editor.cursor(), (0, 2));
        press(&mut editor, Home, NONE);
        assert_eq!(editor.cursor(), (0, 0));
    }

    #[test]
    fn wrap_layout() {
        assert_eq!(rows("hello world", 8), [(0, 6), (6, 11)]);
        assert_eq!(rows("abcdefghij", 4), [(0, 4), (4, 8), (8, 10)]);
        assert_eq!(rows("a bcdefghij", 4), [(0, 2), (2, 6), (6, 10), (10, 11)]);
        assert_eq!(rows("中文字", 5), [(0, 6), (6, 9)]);
        assert_eq!(rows("中文字", 4), [(0, 6), (6, 9)]);
        assert_eq!(rows("hello     world", 7), [(0, 10), (10, 15)]);
        for width in [0, 1] {
            assert_eq!(rows("ab中", width), [(0, 1), (1, 2), (2, 5)]);
        }
        assert_eq!(rows("", 10), [(0, 0)]);
        let editor = NoteEditor::new("a\n\nb");
        let lines: Vec<usize> = editor.layout(10).iter().map(|r| r.line).collect();
        assert_eq!(lines, [0, 1, 2]);

        for text in [
            "e\u{301}中👩‍💻 xyz  中a",
            "  - [ ] a long task item\twith tab",
            "\u{301}x",
        ] {
            for width in 0..12u16 {
                let mut next = 0;
                for (start, end) in rows(text, width) {
                    assert_eq!(start, next);
                    let row = &text[start..end];
                    assert!(
                        cells(row.trim_end()) <= usize::from(width.max(1))
                            || row.trim_end().graphemes(true).count() == 1,
                        "{text:?} {width} {row:?}"
                    );
                    next = end;
                }
                assert_eq!(next, text.len());
            }
        }
    }

    #[test]
    fn vertical_moves_keep_preferred_column() {
        let mut editor = NoteEditor::new("abcdefghij\nxy\nabcdefghij");
        editor.set_cursor(0, 8);
        for expected in [(1, 2), (2, 8)] {
            press(&mut editor, Down, NONE);
            assert_eq!(editor.cursor(), expected);
        }
        for expected in [(1, 2), (0, 8), (0, 0)] {
            press(&mut editor, Up, NONE);
            assert_eq!(editor.cursor(), expected);
        }

        // Rows: "aaaa " "bbbb " "cccc"
        let mut editor = NoteEditor::new("aaaa bbbb cccc");
        editor.set_cursor(0, 2);
        for expected in [(0, 7), (0, 12), (0, 14)] {
            press_at(&mut editor, Down, NONE, 5);
            assert_eq!(editor.cursor(), expected);
        }
        press_at(&mut editor, Up, NONE, 5);
        assert_eq!(editor.cursor(), (0, 7));
        press_at(&mut editor, End, NONE, 5);
        assert_eq!(editor.cursor(), (0, 9));
        press_at(&mut editor, Home, NONE, 5);
        assert_eq!(editor.cursor(), (0, 5));
        press_at(&mut editor, Down, SHIFT, 5);
        assert_eq!(editor.selected_text().as_deref(), Some("bbbb "));
        press_at(&mut editor, PageUp, NONE, 5);
        assert_eq!(editor.cursor(), (0, 0));
    }

    #[test]
    fn visual_positions_round_trip() {
        for text in [
            "hello world\n\n中文字 abc",
            "a     b\ne\u{301}👩‍💻x",
            "- [ ] tab\there and a long word abcdefghijklmnop",
        ] {
            let mut editor = NoteEditor::new(text);
            for width in [0, 1, 2, 3, 5, 8, 40] {
                for line in 0..editor.lines.len() {
                    let content = editor.lines[line].clone();
                    for byte in content
                        .grapheme_indices(true)
                        .map(|(i, _)| i)
                        .chain([content.len()])
                    {
                        editor.set_cursor(line, byte);
                        let (row, x) = editor.cursor_visual(width);
                        assert_eq!(
                            editor.visual_to_position(row, x, width),
                            (line, byte),
                            "{text:?} width {width}"
                        );
                    }
                }
            }
        }
        let editor = NoteEditor::new("ab\ncd");
        assert_eq!(editor.visual_to_position(99, 99, 10), (1, 2));
        assert_eq!(editor.visual_to_position(0, 99, 10), (0, 2));
    }

    #[test]
    fn selection_insert_cut_and_copy() {
        let mut editor = NoteEditor::new("hello world");
        assert_eq!(
            press(&mut editor, Char('c'), CTRL),
            EditorOutcome::Unhandled
        );
        for _ in 0..5 {
            press(&mut editor, Right, SHIFT);
        }
        assert_eq!(editor.selection(), Some(((0, 0), (0, 5))));
        type_str(&mut editor, "X");
        assert_eq!(editor.text(), "X world");
        press(&mut editor, Char('z'), CTRL);
        assert_eq!(editor.text(), "hello world");

        let mut editor = NoteEditor::new("one\ntwo\nthree");
        editor.set_cursor(0, 1);
        press(&mut editor, Down, SHIFT);
        assert_eq!(editor.selected_text().as_deref(), Some("ne\nt"));
        editor.insert("a\r\nb\tc");
        assert_eq!(editor.text(), "oa\nb  cwo\nthree");
        assert_eq!(editor.cursor(), (1, 4));

        let mut editor = NoteEditor::new("ab\ncd");
        press(&mut editor, Char('a'), CTRL);
        let revision = editor.revision();
        assert_eq!(
            press(&mut editor, Char('c'), CTRL),
            EditorOutcome::Copy("ab\ncd".into())
        );
        assert_eq!(editor.revision(), revision);
        assert_eq!(
            press(&mut editor, Char('x'), CTRL),
            EditorOutcome::Copy("ab\ncd".into())
        );
        assert_eq!(editor.text(), "");
        assert!(editor.revision() > revision);
        assert_eq!(
            press(&mut editor, Char('x'), CTRL),
            EditorOutcome::Unhandled
        );

        // Plain Left/Right collapse a selection to its edge.
        let mut editor = NoteEditor::new("abcd");
        editor.set_cursor(0, 1);
        press(&mut editor, Right, SHIFT);
        press(&mut editor, Right, SHIFT);
        press(&mut editor, Left, NONE);
        assert_eq!((editor.cursor(), editor.selection()), ((0, 1), None));
    }

    #[test]
    fn tab_indents_and_outdents() {
        let mut editor = NoteEditor::new("a\nb\nc\nd");
        press(&mut editor, Down, SHIFT);
        press(&mut editor, Down, SHIFT);
        press(&mut editor, Down, SHIFT); // ends at (3,0): line 3 excluded
        assert_eq!(press(&mut editor, Tab, NONE), EditorOutcome::Changed);
        assert_eq!(editor.text(), "  a\n  b\n  c\nd");
        assert_eq!(editor.selection(), Some(((0, 2), (3, 0))));
        press(&mut editor, BackTab, NONE);
        assert_eq!(editor.text(), "a\nb\nc\nd");
        assert_eq!(press(&mut editor, Tab, SHIFT), EditorOutcome::Moved);
        assert_eq!(editor.text(), "a\nb\nc\nd");

        let mut editor = at_end("- x");
        press(&mut editor, Tab, NONE);
        assert_eq!((editor.text().as_str(), editor.cursor()), ("  - x", (0, 5)));
        let mut editor = NoteEditor::new("\t- x");
        press(&mut editor, BackTab, NONE);
        assert_eq!(editor.text(), "- x");
        let mut editor = NoteEditor::new("ab");
        editor.set_cursor(0, 1);
        press(&mut editor, Tab, NONE);
        assert_eq!(editor.text(), "a  b");
    }

    #[test]
    fn text_round_trip_and_external_reload() {
        assert_eq!(NoteEditor::new("a\nb\n").text(), "a\nb\n");
        assert_eq!(NoteEditor::new("a\nb\n").lines().len(), 3);
        assert_eq!(NoteEditor::new("x\r\ny").text(), "x\ny");
        assert_eq!(NoteEditor::new("").lines(), [""]);

        let mut editor = NoteEditor::new("hello\nworld");
        type_str(&mut editor, "x");
        editor.set_cursor(1, 5);
        press(&mut editor, Left, SHIFT);
        let revision = editor.revision();
        editor.set_text("hi");
        assert_eq!(editor.cursor(), (0, 2));
        assert_eq!(editor.selection(), None);
        assert!(editor.revision() > revision);
        assert_eq!(press(&mut editor, Char('z'), CTRL), EditorOutcome::Moved);
        assert_eq!(editor.text(), "hi");
        let revision = editor.revision();
        editor.set_text("hi");
        assert_eq!(editor.revision(), revision);
        editor.set_cursor(0, 1);
        editor.set_text("中");
        assert_eq!(editor.cursor(), (0, 3));
    }

    #[test]
    fn scrolling_and_mouse() {
        let mut editor = NoteEditor::new(&"line\n".repeat(9));
        press(&mut editor, End, CTRL);
        assert_eq!(editor.scroll(), 5);
        editor.scroll_by(-100, 40, 5);
        assert_eq!(editor.scroll(), 0);
        editor.scroll_by(100, 40, 5);
        assert_eq!(editor.scroll(), 5);
        press(&mut editor, Home, CTRL);
        assert_eq!(editor.scroll(), 0);

        let mut editor = NoteEditor::new("hello world, again");
        editor.click(0, 7, 40, false);
        assert_eq!(editor.cursor(), (0, 7));
        editor.click(0, 2, 40, true);
        assert_eq!(editor.selection(), Some(((0, 2), (0, 7))));
        editor.select_word_at(0, 8, 40);
        assert_eq!(editor.selected_text().as_deref(), Some("world"));
        editor.select_word_at(0, 99, 40);
        assert_eq!(editor.selected_text().as_deref(), Some("again"));
        editor.select_word_at(0, 11, 40);
        assert_eq!(editor.selected_text().as_deref(), Some(","));
    }

    #[test]
    fn keys_outside_the_editor_are_unhandled() {
        let mut editor = NoteEditor::new("ab");
        for (code, mods) in [(Esc, NONE), (Char('q'), CTRL), (Enter, CTRL), (F(2), NONE)] {
            assert_eq!(press(&mut editor, code, mods), EditorOutcome::Unhandled);
        }
        let release = TerminalKey::new(Char('x'), NONE).with_kind(KeyEventKind::Release);
        assert_eq!(editor.handle_key(&release, 10, 5), EditorOutcome::Unhandled);
        let altgr = TerminalKey::new(Char('q'), CTRL | ALT).with_generated_text(Some("@".into()));
        assert_eq!(editor.handle_key(&altgr, 10, 5), EditorOutcome::Changed);
        assert_eq!(editor.text(), "@ab");
        assert_eq!(editor.revision(), 1);
    }

    #[test]
    fn random_input_never_panics() {
        let keys = [
            (Char('a'), NONE),
            (Char(' '), NONE),
            (Char('中'), NONE),
            (Char('\u{301}'), NONE),
            (Char('\u{200d}'), NONE),
            (Char('👩'), NONE),
            (Enter, NONE),
            (Tab, NONE),
            (BackTab, NONE),
            (Backspace, NONE),
            (Backspace, CTRL),
            (Delete, NONE),
            (Delete, CTRL),
            (Left, NONE),
            (Right, SHIFT),
            (Left, CTRL | SHIFT),
            (Right, ALT),
            (Up, SHIFT),
            (Down, NONE),
            (PageUp, NONE),
            (PageDown, SHIFT),
            (Home, NONE),
            (End, SHIFT),
            (Home, CTRL),
            (Char('a'), CTRL),
            (Char('x'), CTRL),
            (Char('z'), CTRL),
            (Char('y'), CTRL),
        ];
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) as usize
        };
        let mut editor = NoteEditor::new("- [ ] e\u{301}中👩‍💻\n\t1. x\n");
        for _ in 0..5000 {
            let width = (next() % 8) as u16;
            match next() % 10 {
                0 => editor.click(next() % 12, next() % 10, width, next() % 2 == 0),
                1 => editor.select_word_at(next() % 12, next() % 10, width),
                2 => editor.set_cursor(next() % 6, next() % 20),
                3 => editor.scroll_by(next() as isize % 7 - 3, width, 3),
                _ => {
                    let (code, mods) = keys[next() % keys.len()];
                    press_at(&mut editor, code, mods, width);
                }
            }
            let (row, x) = editor.cursor_visual(width);
            editor.visual_to_position(row, x, width);
            editor.selected_text();
        }
    }
}
