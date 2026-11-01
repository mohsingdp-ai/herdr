//! Drawing Notes into the pane area. Layout lives in [`NotesGeometry`] so
//! the frame preparation that lays out and clamps scrolling and the drawing
//! that follows always agree.

use ratatui::style::Color;
use unicode_segmentation::UnicodeSegmentation;

use super::board_view_render::{draw_box, tilde};
use super::git_view_render::fit;
use super::notes_markdown::{self, DocRow, Look};
use super::notes_view::{
    folder, title, ClientNotesView, ListRow, NotesLoad, NotesTarget, NotesViewHits, SwitcherChoice,
    SwitcherPurpose,
};
use super::render::{display_width, put_segment, put_text};
use super::*;

const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 12;
/// The note list shows from this body width, the backlinks and outline from this one.
const LIST_FROM: u16 = 76;
const SIDE_FROM: u16 = 118;
const LIST_WIDTH: u16 = 26;
const SIDE_WIDTH: u16 = 30;
/// Notes read best at this many columns at most, centered when there is more room.
const PAGE_WIDTH: u16 = 100;
const SWITCHER_ROWS: usize = 12;

pub(super) struct NotesGeometry {
    too_small: bool,
    header: Rect,
    toolbar: Rect,
    body: Rect,
    hint: Rect,
    list: Option<Rect>,
    side: Option<Rect>,
    /// The middle column, scrollbar included.
    main: Rect,
    /// The note's name and buttons.
    title: Rect,
    /// The bar that asks what to do when the file changed under unsaved edits.
    conflict: Option<Rect>,
    /// Where the note's text goes.
    page: Rect,
}

impl NotesGeometry {
    pub(super) fn new(area: Rect, view: &ClientNotesView) -> Self {
        let too_small = area.width < MIN_WIDTH || area.height < MIN_HEIGHT;
        // One blank row above the header keeps it off the tab bar.
        let header = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
        let toolbar = Rect::new(header.x, area.y + 3, header.width, 1);
        let body_top = area.y + 5;
        let hint = Rect::new(header.x, area.bottom().saturating_sub(1), header.width, 1);
        let body = Rect::new(
            header.x,
            body_top,
            header.width,
            hint.y.saturating_sub(body_top + 1),
        );
        let list =
            (body.width >= LIST_FROM).then(|| Rect::new(body.x, body.y, LIST_WIDTH, body.height));
        let side = (body.width >= SIDE_FROM)
            .then(|| Rect::new(body.right() - SIDE_WIDTH, body.y, SIDE_WIDTH, body.height));
        // Each column beside the note is followed by a rule with a space each side.
        let main_x = list.map_or(body.x, |list| list.right() + 3);
        let main_right = side.map_or(body.right(), |side| side.x.saturating_sub(3));
        let main = Rect::new(
            main_x,
            body.y,
            main_right.saturating_sub(main_x),
            body.height,
        );
        // The scrollbar takes the last column, with a space before it.
        let room = main.width.saturating_sub(2);
        let page_width = room.min(PAGE_WIDTH);
        let page_x = main.x + (room - page_width) / 2;
        let title = Rect::new(page_x, main.y, page_width, 1);
        let conflict = view
            .conflict
            .then(|| Rect::new(page_x, main.y + 2, page_width, 1));
        let page_top = main.y + if conflict.is_some() { 4 } else { 2 };
        let page = Rect::new(
            page_x,
            page_top,
            page_width,
            main.bottom().saturating_sub(page_top),
        );
        Self {
            too_small,
            header,
            toolbar,
            body,
            hint,
            list,
            side,
            main,
            title,
            conflict,
            page,
        }
    }
}

impl ClientNotesView {
    /// Lays the note out for this frame's size and fits scrolling to it,
    /// before the pure drawing pass.
    pub(super) fn prepare_frame(&mut self, area: Rect) {
        let geometry = NotesGeometry::new(area, self);
        if geometry.too_small {
            return;
        }
        let page = geometry.page;
        self.editor_size = (page.width.max(1), page.height.max(1));
        if let Some(editor) = self.editor.as_mut() {
            if std::mem::take(&mut self.reveal_cursor) {
                editor.ensure_cursor_visible(page.width, page.height);
            }
            self.doc_rows.clear();
        } else {
            self.reveal_cursor = false;
            let text = self.open_text().unwrap_or_default();
            let rows =
                notes_markdown::layout(&text, page.width, &|name| self.resolve(name).is_some());
            self.doc_scroll = self
                .doc_scroll
                .min(rows.len().saturating_sub(usize::from(page.height)));
            self.doc_rows = rows;
        }
        if let Some(list) = geometry.list {
            let lines = list_lines(self).len();
            let viewport = usize::from(list.height.saturating_sub(1));
            self.list_scroll = self.list_scroll.min(lines.saturating_sub(viewport));
        }
        let choices = self.switcher_choices().len();
        if let Some(switcher) = self.switcher.as_mut() {
            switcher.selected = switcher.selected.min(choices.saturating_sub(1));
        }
    }
}

struct Styles {
    base: Style,
    dim: Style,
    muted: Style,
    bold: Style,
    accent: Style,
    rule: Style,
    button: Style,
    primary: Style,
    selected_row: Style,
    green: Style,
    red: Style,
    yellow: Style,
    contrast: Color,
    palette: Palette,
}

impl Styles {
    fn new(p: &Palette) -> Self {
        let base = Style::default().fg(p.text);
        let contrast = match p.panel_bg {
            Color::Reset => p.surface_dim,
            color => color,
        };
        Self {
            base,
            dim: Style::default().fg(p.overlay0),
            muted: Style::default().fg(p.overlay1),
            bold: base.add_modifier(Modifier::BOLD),
            accent: Style::default().fg(p.accent),
            rule: Style::default().fg(p.surface1),
            button: Style::default().fg(p.overlay1).bg(p.surface0),
            primary: Style::default()
                .fg(contrast)
                .bg(p.accent)
                .add_modifier(Modifier::BOLD),
            selected_row: base.bg(p.surface0),
            green: Style::default().fg(p.green),
            red: Style::default().fg(p.red),
            yellow: Style::default().fg(p.yellow),
            contrast,
            palette: p.clone(),
        }
    }

    fn tag(&self) -> Style {
        Style::default().fg(self.palette.peach)
    }

    /// How markdown looks, in read mode and tinted in edit mode.
    fn look(&self, look: Look) -> Style {
        let p = &self.palette;
        match look {
            Look::Plain => self.base,
            Look::Heading(1) => Style::default().fg(p.mauve).add_modifier(Modifier::BOLD),
            Look::Heading(2) => Style::default().fg(p.blue).add_modifier(Modifier::BOLD),
            Look::Heading(_) => Style::default().fg(p.teal).add_modifier(Modifier::BOLD),
            Look::Bold => self.bold,
            Look::Code => Style::default().fg(p.green).bg(p.surface0),
            Look::Link { exists: true } => Style::default()
                .fg(p.blue)
                .add_modifier(Modifier::UNDERLINED),
            Look::Link { exists: false } => Style::default()
                .fg(p.overlay1)
                .add_modifier(Modifier::UNDERLINED | Modifier::ITALIC),
            Look::Image => Style::default()
                .fg(p.mauve)
                .bg(p.surface0)
                .add_modifier(Modifier::BOLD),
            Look::Tag => self.tag(),
            Look::FileRef => Style::default().fg(p.teal),
            Look::Url => Style::default()
                .fg(p.blue)
                .add_modifier(Modifier::UNDERLINED),
            Look::Bullet => self.muted,
            Look::Checkbox { checked: false } => Style::default().fg(p.blue),
            Look::Checkbox { checked: true } => self.green,
            Look::Done => Style::default()
                .fg(p.overlay0)
                .add_modifier(Modifier::CROSSED_OUT),
            Look::Quote => Style::default()
                .fg(p.subtext0)
                .add_modifier(Modifier::ITALIC),
            Look::QuoteBar => Style::default().fg(p.accent),
            Look::CodeBlock => Style::default().fg(p.green),
            Look::Rule => self.rule,
            Look::Mark => self.dim,
        }
    }
}

pub(super) struct NotesViewFrame {
    pub(super) hits: NotesViewHits,
    pub(super) cursor: Option<crate::protocol::CursorState>,
}

/// Draws Notes over `area`, which it owns entirely.
pub(super) fn render_notes_view(
    buffer: &mut Buffer,
    area: Rect,
    view: &ClientNotesView,
    space_label: &str,
    palette: &Palette,
) -> NotesViewFrame {
    let area = area.intersection(buffer.area);
    let styles = Styles::new(palette);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].set_symbol(" ").set_style(Style::reset());
        }
    }
    let mut frame = NotesViewFrame {
        hits: NotesViewHits {
            area,
            ..NotesViewHits::default()
        },
        cursor: None,
    };
    let geometry = NotesGeometry::new(area, view);
    render_header(
        buffer,
        &geometry,
        view,
        space_label,
        &styles,
        &mut frame.hits,
    );
    if geometry.too_small {
        put_text(
            buffer,
            area.x + 1,
            area.y + 3.min(area.height.saturating_sub(1)),
            area.width.saturating_sub(2),
            "the window is too small for Notes",
            styles.dim,
        );
        return frame;
    }
    match &view.load {
        NotesLoad::Ready => {
            render_toolbar(buffer, &geometry, view, &styles, &mut frame.hits);
            if let Some(list) = geometry.list {
                render_list(buffer, list, view, &styles, &mut frame.hits);
                render_rule(buffer, list.right() + 1, geometry.body, &styles);
            }
            if let Some(side) = geometry.side {
                render_rule(buffer, side.x - 2, geometry.body, &styles);
                render_side(buffer, side, view, &styles, &mut frame.hits);
            }
            if view.notes().is_empty() && view.open.is_none() {
                render_empty(buffer, &geometry, view, &styles, &mut frame.hits);
            } else {
                frame.cursor = render_note(buffer, &geometry, view, &styles, &mut frame.hits);
            }
        }
        NotesLoad::Loading => put_text(
            buffer,
            geometry.toolbar.x,
            geometry.toolbar.y,
            geometry.toolbar.width,
            "loading…",
            styles.dim,
        ),
        NotesLoad::Unsupported => put_text(
            buffer,
            geometry.toolbar.x,
            geometry.toolbar.y,
            geometry.toolbar.width,
            "this server has no notes support yet · update and restart it",
            styles.yellow,
        ),
        NotesLoad::Failed(message) => {
            put_text(
                buffer,
                geometry.toolbar.x,
                geometry.toolbar.y,
                geometry.toolbar.width,
                "The notes could not be read",
                styles.bold,
            );
            put_text(
                buffer,
                geometry.toolbar.x,
                geometry.toolbar.y + 2,
                geometry.toolbar.width,
                &fit(message, geometry.toolbar.width),
                styles.red,
            );
            let label = " Retry ";
            let rect = Rect::new(
                geometry.toolbar.x,
                geometry.toolbar.y + 4,
                display_width(label),
                1,
            );
            put_text(buffer, rect.x, rect.y, rect.width, label, styles.primary);
            frame.hits.targets.push((rect, NotesTarget::Retry));
        }
    }
    render_hint(buffer, &geometry, view, &styles);
    if view.load == NotesLoad::Ready && view.switcher.is_some() {
        frame.cursor = render_switcher(buffer, area, view, &styles, &mut frame.hits);
    }
    frame
}

fn render_header(
    buffer: &mut Buffer,
    geometry: &NotesGeometry,
    view: &ClientNotesView,
    space_label: &str,
    styles: &Styles,
    hits: &mut NotesViewHits,
) {
    let row = geometry.header;
    let close = " esc ";
    let close_x = row.right().saturating_sub(display_width(close));
    put_text(
        buffer,
        close_x,
        row.y,
        display_width(close),
        close,
        styles.dim,
    );
    hits.targets.push((
        Rect::new(close_x, row.y, display_width(close), 1),
        NotesTarget::Close,
    ));
    let end = close_x.saturating_sub(1);
    let mut x = put_segment(buffer, row.x, row.y, end, "✎ Notes", styles.bold);
    x = put_segment(buffer, x, row.y, end, "  ", styles.base);
    x = put_segment(buffer, x, row.y, end, space_label, styles.muted);
    if let Some(vault) = view.vault.as_ref() {
        x = put_segment(buffer, x, row.y, end, "  ·  ", styles.dim);
        let count = vault.notes.len();
        let label = format!("{count} note{}", if count == 1 { "" } else { "s" });
        x = put_segment(buffer, x, row.y, end, &label, styles.muted);
        let root = tilde(&vault.root);
        let width = display_width(&root);
        if x + 4 + width <= end {
            put_text(buffer, end - width, row.y, width, &root, styles.dim);
        }
    }
}

fn render_toolbar(
    buffer: &mut Buffer,
    geometry: &NotesGeometry,
    view: &ClientNotesView,
    styles: &Styles,
    hits: &mut NotesViewHits,
) {
    let row = geometry.toolbar;
    let mut x = row.x;
    for (label, target, style) in [
        (" + New note ", NotesTarget::New, styles.primary),
        (" ☀ Today ", NotesTarget::Today, styles.button),
        (" ⌕ Find  ctrl+o ", NotesTarget::Find, styles.button),
    ] {
        let width = display_width(label);
        if x + width > row.right() {
            break;
        }
        put_text(buffer, x, row.y, width, label, style);
        hits.targets.push((Rect::new(x, row.y, width, 1), target));
        x += width + 1;
    }
    let notice = view
        .vault
        .as_ref()
        .filter(|vault| vault.truncated)
        .map(|_| "some files are too big or too many to show");
    if let Some(notice) = notice {
        let width = display_width(notice);
        if x + 2 + width <= row.right() {
            put_text(
                buffer,
                row.right() - width,
                row.y,
                width,
                notice,
                styles.yellow,
            );
        }
    }
}

fn render_rule(buffer: &mut Buffer, x: u16, body: Rect, styles: &Styles) {
    for y in body.y..body.bottom() {
        put_text(buffer, x, y, 1, "│", styles.rule);
    }
}

/// One line of the note list column and what clicking it does.
struct ListLine {
    spans: Vec<(String, Style)>,
    target: Option<NotesTarget>,
    selected: bool,
}

/// The note list, then the tags; the column's header is drawn apart.
fn list_lines(view: &ClientNotesView) -> Vec<ListLine> {
    // Styles are filled in at drawing time; counting needs none.
    list_lines_styled(view, None)
}

fn list_lines_styled(view: &ClientNotesView, styles: Option<&Styles>) -> Vec<ListLine> {
    let style = |pick: fn(&Styles) -> Style| styles.map_or_else(Style::default, pick);
    let mut lines = Vec::new();
    let rows = view.list_rows();
    if rows.is_empty() {
        lines.push(ListLine {
            spans: vec![(
                if view.tag_filter.is_some() {
                    "no notes with this tag"
                } else {
                    "no notes yet"
                }
                .to_owned(),
                style(|s| s.dim),
            )],
            target: None,
            selected: false,
        });
    }
    for row in rows {
        match row {
            ListRow::Folder {
                path,
                name,
                depth,
                open,
            } => lines.push(ListLine {
                spans: vec![
                    ("  ".repeat(depth), style(|s| s.base)),
                    (
                        (if open { "▾ " } else { "▸ " }).to_owned(),
                        style(|s| s.dim),
                    ),
                    (format!("{name}/"), style(|s| s.muted)),
                ],
                target: Some(NotesTarget::Folder(path)),
                selected: false,
            }),
            ListRow::Note { path, title, depth } => {
                let selected = view.open.as_ref() == Some(&path);
                lines.push(ListLine {
                    spans: vec![
                        ("  ".repeat(depth), style(|s| s.base)),
                        (
                            title,
                            if selected {
                                style(|s| s.accent.add_modifier(Modifier::BOLD))
                            } else {
                                style(|s| s.base)
                            },
                        ),
                    ],
                    target: Some(NotesTarget::Open(path)),
                    selected,
                });
            }
        }
    }
    if !view.index.tags.is_empty() {
        lines.push(ListLine {
            spans: Vec::new(),
            target: None,
            selected: false,
        });
        lines.push(ListLine {
            spans: vec![("TAGS".to_owned(), style(|s| s.dim))],
            target: None,
            selected: false,
        });
        for (tag, notes) in &view.index.tags {
            let selected = view.tag_filter.as_ref() == Some(tag);
            lines.push(ListLine {
                spans: vec![
                    (format!("#{tag}"), style(Styles::tag)),
                    (format!("  {}", notes.len()), style(|s| s.dim)),
                ],
                target: Some(NotesTarget::Tag(tag.clone())),
                selected,
            });
        }
    }
    lines
}

fn render_list(
    buffer: &mut Buffer,
    list: Rect,
    view: &ClientNotesView,
    styles: &Styles,
    hits: &mut NotesViewHits,
) {
    hits.list = list;
    // The header stays put while the list scrolls under it.
    match view.tag_filter.as_ref() {
        Some(tag) => {
            let mut x = put_segment(buffer, list.x, list.y, list.right(), "#", styles.tag());
            x = put_segment(buffer, x, list.y, list.right(), tag, styles.tag());
            let clear = " × ";
            let width = display_width(clear);
            let at = (x + 1).min(list.right().saturating_sub(width));
            put_text(buffer, at, list.y, width, clear, styles.button);
            hits.targets
                .push((Rect::new(at, list.y, width, 1), NotesTarget::ClearTag));
        }
        None => {
            put_text(buffer, list.x, list.y, list.width, "NOTES", styles.dim);
        }
    }
    let rows = Rect::new(
        list.x,
        list.y + 1,
        list.width,
        list.height.saturating_sub(1),
    );
    let lines = list_lines_styled(view, Some(styles));
    let total = lines.len();
    for (index, line) in lines.into_iter().skip(view.list_scroll).enumerate() {
        let y = rows.y + index as u16;
        if y >= rows.bottom() {
            break;
        }
        let rect = Rect::new(rows.x, y, rows.width.saturating_sub(1), 1);
        if line.selected {
            buffer.set_style(rect, styles.selected_row);
        }
        let mut x = rect.x + 1;
        for (text, style) in &line.spans {
            let style = if line.selected {
                style.bg(styles.palette.surface0)
            } else {
                *style
            };
            x = put_segment(
                buffer,
                x,
                y,
                rect.right(),
                &fit(text, rect.right().saturating_sub(x)),
                style,
            );
        }
        if let Some(target) = line.target {
            hits.targets.push((rect, target));
        }
    }
    render_scrollbar(buffer, rows, total, view.list_scroll, styles);
}

fn render_scrollbar(buffer: &mut Buffer, track: Rect, rows: usize, scroll: usize, styles: &Styles) {
    let viewport = usize::from(track.height);
    if rows <= viewport || track.width == 0 {
        return;
    }
    let max = rows - viewport;
    crate::ui::render_scrollbar_buffer(
        buffer,
        crate::pane::ScrollMetrics {
            offset_from_bottom: max.saturating_sub(scroll),
            max_offset_from_bottom: max,
            viewport_rows: viewport,
        },
        Rect::new(track.right() - 1, track.y, 1, track.height),
        styles.rule.fg.unwrap_or(Color::Reset),
        styles.dim.fg.unwrap_or(Color::Reset),
        "▐",
    );
}

/// Backlinks above, the open note's headings below.
fn render_side(
    buffer: &mut Buffer,
    side: Rect,
    view: &ClientNotesView,
    styles: &Styles,
    hits: &mut NotesViewHits,
) {
    let backlinks = view.backlinks();
    let mut y = side.y;
    let header = format!("BACKLINKS  {}", backlinks.len());
    put_text(buffer, side.x, y, side.width, &header, styles.dim);
    y += 1;
    // Backlinks take at most half the column; the outline gets the rest.
    let backlink_rows = (side.height / 2).saturating_sub(2).max(1);
    if backlinks.is_empty() {
        put_text(
            buffer,
            side.x + 1,
            y,
            side.width,
            "no other note links here",
            styles.dim,
        );
        y += 1;
    }
    for (index, path) in backlinks.iter().enumerate() {
        if index as u16 >= backlink_rows {
            let more = format!("+{} more", backlinks.len() - index);
            put_text(buffer, side.x + 1, y, side.width, &more, styles.dim);
            y += 1;
            break;
        }
        let rect = Rect::new(side.x, y, side.width, 1);
        let mut x = put_segment(buffer, side.x + 1, y, side.right(), "← ", styles.dim);
        x = put_segment(
            buffer,
            x,
            y,
            side.right(),
            &fit(title(path), side.right().saturating_sub(x)),
            styles.accent,
        );
        let dir = folder(path);
        if !dir.is_empty() && x + 2 < side.right() {
            put_segment(
                buffer,
                x + 1,
                y,
                side.right(),
                &fit(dir, side.right().saturating_sub(x + 1)),
                styles.dim,
            );
        }
        hits.targets
            .push((rect, NotesTarget::Open((*path).to_owned())));
        y += 1;
    }
    y += 1;
    if y >= side.bottom() {
        return;
    }
    put_text(buffer, side.x, y, side.width, "OUTLINE", styles.dim);
    y += 1;
    let headings = view
        .open_text()
        .map(|text| notes_markdown::headings(&text))
        .unwrap_or_default();
    if headings.is_empty() && y < side.bottom() {
        put_text(buffer, side.x + 1, y, side.width, "no headings", styles.dim);
    }
    for (level, heading, line) in headings {
        if y >= side.bottom() {
            break;
        }
        let indent = 1 + 2 * u16::from(level.saturating_sub(1));
        let style = if level == 1 {
            styles.base
        } else {
            styles.muted
        };
        let x = side.x + indent;
        put_text(
            buffer,
            x,
            y,
            side.right().saturating_sub(x),
            &fit(&heading, side.right().saturating_sub(x)),
            style,
        );
        hits.targets
            .push((Rect::new(side.x, y, side.width, 1), NotesTarget::Line(line)));
        y += 1;
    }
}

/// No notes yet: where they live, and the two ways to start.
fn render_empty(
    buffer: &mut Buffer,
    geometry: &NotesGeometry,
    view: &ClientNotesView,
    styles: &Styles,
    hits: &mut NotesViewHits,
) {
    let page = geometry.page;
    let root = view
        .vault
        .as_ref()
        .map(|vault| tilde(&vault.root))
        .unwrap_or_default();
    let project = view
        .vault
        .as_ref()
        .map(|vault| vault.project.clone())
        .unwrap_or_default();
    let lines = [
        (format!("No notes for {project} yet"), styles.bold),
        (String::new(), styles.base),
        ("Notes are plain markdown files in".to_owned(), styles.muted),
        (root, styles.accent),
        (
            "Open that folder in Obsidian to use the same notes there.".to_owned(),
            styles.dim,
        ),
    ];
    let top = geometry.main.y + geometry.main.height / 4;
    for (index, (text, style)) in lines.iter().enumerate() {
        let width = display_width(text).min(page.width);
        let x = page.x + (page.width - width) / 2;
        put_text(
            buffer,
            x,
            top + index as u16,
            width,
            &fit(text, width),
            *style,
        );
    }
    let buttons = [
        (" + New note ", NotesTarget::New, styles.primary),
        (" ☀ Today ", NotesTarget::Today, styles.button),
    ];
    let total: u16 = buttons
        .iter()
        .map(|(label, ..)| display_width(label) + 2)
        .sum();
    let mut x = page.x + page.width.saturating_sub(total) / 2;
    let y = top + lines.len() as u16 + 1;
    for (label, target, style) in buttons {
        let width = display_width(label);
        put_text(buffer, x, y, width, label, style);
        hits.targets.push((Rect::new(x, y, width, 1), target));
        x += width + 2;
    }
}

/// The open note: its name and buttons, then the text, read or edited.
fn render_note(
    buffer: &mut Buffer,
    geometry: &NotesGeometry,
    view: &ClientNotesView,
    styles: &Styles,
    hits: &mut NotesViewHits,
) -> Option<crate::protocol::CursorState> {
    let Some(path) = view.open.as_deref() else {
        let page = geometry.page;
        put_text(
            buffer,
            page.x,
            page.y,
            page.width,
            "pick a note on the left, or press ctrl+o",
            styles.dim,
        );
        return None;
    };
    render_title(buffer, geometry, view, path, styles, hits);
    let row = geometry.title;
    put_text(
        buffer,
        row.x,
        row.y + 1,
        row.width,
        &"─".repeat(usize::from(row.width)),
        styles.rule,
    );
    if let Some(bar) = geometry.conflict {
        render_conflict(buffer, bar, styles, hits);
    }
    let page = geometry.page;
    let track = Rect::new(
        geometry.main.right().saturating_sub(1),
        page.y,
        1,
        page.height,
    );
    hits.doc = Rect::new(geometry.main.x, page.y, geometry.main.width, page.height);
    match view.editor.as_ref() {
        Some(editor) => {
            hits.editor = page;
            hits.targets.push((hits.doc, NotesTarget::Editor));
            let rows = editor.layout(page.width);
            let total = rows.len();
            let scroll = editor.scroll();
            render_editor(buffer, page, view, styles);
            render_scrollbar(buffer, track, total, scroll, styles);
            let (row, x) = editor.cursor_visual(page.width);
            let row = row.checked_sub(scroll)?;
            // Hanging spaces at a wrap can put the cursor past the edge.
            let x = u16::try_from(x).unwrap_or(u16::MAX);
            (row < usize::from(page.height)).then_some(crate::protocol::CursorState {
                x: page.x + x.min(page.width.saturating_sub(1)),
                y: page.y + row as u16,
                visible: true,
                shape: 0,
            })
        }
        None => {
            hits.targets.push((hits.doc, NotesTarget::Doc));
            hits.doc = page;
            let empty = view.open_text().is_none_or(|text| text.trim().is_empty());
            if empty {
                put_text(
                    buffer,
                    page.x,
                    page.y,
                    page.width,
                    "Empty note · click to start writing",
                    styles.dim,
                );
                return None;
            }
            for (index, row) in view.doc_rows.iter().skip(view.doc_scroll).enumerate() {
                let y = page.y + index as u16;
                if y >= page.bottom() {
                    break;
                }
                render_doc_row(buffer, page.x, y, page.right(), row, styles);
            }
            render_scrollbar(buffer, track, view.doc_rows.len(), view.doc_scroll, styles);
            None
        }
    }
}

fn render_doc_row(buffer: &mut Buffer, x: u16, y: u16, right: u16, row: &DocRow, styles: &Styles) {
    let mut x = x;
    for piece in &row.pieces {
        x = put_segment(buffer, x, y, right, &piece.text, styles.look(piece.look));
    }
}

fn render_title(
    buffer: &mut Buffer,
    geometry: &NotesGeometry,
    view: &ClientNotesView,
    path: &str,
    styles: &Styles,
    hits: &mut NotesViewHits,
) {
    let row = geometry.title;
    let editing = view.editor.is_some();
    // Buttons fill in from the right and stop before the name.
    let delete = if view.confirm_delete {
        (
            " Delete? click again ",
            Style::default()
                .fg(styles.contrast)
                .bg(styles.palette.red)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (" Delete ", styles.button)
    };
    let buttons = [
        (delete.0, NotesTarget::Delete, delete.1),
        (" Rename ", NotesTarget::Rename, styles.button),
        (" → Agent ", NotesTarget::ToAgent, styles.button),
        (
            " Edit ",
            NotesTarget::Edit,
            if editing {
                styles.primary
            } else {
                styles.button
            },
        ),
        (
            " Read ",
            NotesTarget::Read,
            if editing {
                styles.button
            } else {
                styles.primary
            },
        ),
    ];
    let name = title(path);
    let name_room = display_width(name).min(16) + 2;
    let mut right = row.right();
    for (index, (label, target, style)) in buttons.into_iter().enumerate() {
        let width = display_width(label);
        // Read and Edit sit together as one switch.
        let gap = if index == 4 { 0 } else { 1 };
        let x = right.saturating_sub(width);
        if x < row.x + name_room {
            break;
        }
        put_text(buffer, x, row.y, width, label, style);
        hits.targets.push((Rect::new(x, row.y, width, 1), target));
        right = x.saturating_sub(gap);
        if index == 2 {
            // A wider gap between the note's actions and the mode switch.
            right = right.saturating_sub(1);
        }
    }
    let end = right.saturating_sub(2);
    let x = put_segment(
        buffer,
        row.x,
        row.y,
        end,
        &fit(name, end.saturating_sub(row.x)),
        styles.bold,
    );
    let dir = folder(path);
    if !dir.is_empty() && x + 3 < end {
        put_segment(
            buffer,
            x + 2,
            row.y,
            end,
            &fit(&format!("{dir}/"), end.saturating_sub(x + 2)),
            styles.dim,
        );
    }
}

fn render_conflict(buffer: &mut Buffer, bar: Rect, styles: &Styles, hits: &mut NotesViewHits) {
    let warn = Style::default()
        .fg(styles.contrast)
        .bg(styles.palette.yellow);
    buffer.set_style(bar, warn);
    let buttons = [
        (" Use disk version ", NotesTarget::LoadDisk),
        (" Keep mine ", NotesTarget::KeepMine),
    ];
    // The buttons come first; the message says as much as the rest of the bar holds.
    let buttons_width: u16 = buttons
        .iter()
        .map(|(label, _)| display_width(label) + 1)
        .sum();
    let room = bar.width.saturating_sub(buttons_width + 2);
    let message = [
        "This note changed on disk while you were typing.",
        "Changed on disk while you typed.",
        "Changed on disk.",
    ]
    .into_iter()
    .find(|message| display_width(message) <= room)
    .unwrap_or("");
    put_segment(
        buffer,
        bar.x + 1,
        bar.y,
        bar.right(),
        message,
        warn.add_modifier(Modifier::BOLD),
    );
    let mut x = bar.right().saturating_sub(buttons_width);
    for (label, target) in buttons {
        let width = display_width(label);
        put_text(buffer, x, bar.y, width, label, styles.primary);
        hits.targets.push((Rect::new(x, bar.y, width, 1), target));
        x += width + 1;
    }
}

/// The note's raw text, wrapped like the editor wraps it and tinted with
/// the markdown it contains; the selection shows on a lighter surface.
fn render_editor(buffer: &mut Buffer, page: Rect, view: &ClientNotesView, styles: &Styles) {
    let Some(editor) = view.editor.as_ref() else {
        return;
    };
    let text = editor.text();
    let lines: Vec<&str> = text.split('\n').collect();
    let parsed = notes_markdown::parse(&text);
    let selection = editor.selection();
    let selected_bg = styles.palette.surface1;
    let rows = editor.layout(page.width);
    for (index, row) in rows.iter().skip(editor.scroll()).enumerate() {
        let y = page.y + index as u16;
        if y >= page.bottom() {
            break;
        }
        let Some(line) = lines.get(row.line) else {
            continue;
        };
        let tint = parsed
            .get(row.line)
            .map(|parsed| notes_markdown::tint(line, parsed))
            .unwrap_or_default();
        let Some(slice) = line.get(row.start..row.end) else {
            continue;
        };
        let mut x = page.x;
        for (offset, grapheme) in slice.grapheme_indices(true) {
            let byte = row.start + offset;
            let look = tint
                .iter()
                .find(|(range, _)| range.contains(&byte))
                .map_or(Look::Plain, |(_, look)| *look);
            let mut style = styles.look(look);
            let selected = selection
                .is_some_and(|(start, end)| (row.line, byte) >= start && (row.line, byte) < end);
            if selected {
                style = style.bg(selected_bg);
            }
            // The editor counts a tab as two cells.
            let shown = if grapheme == "\t" { "  " } else { grapheme };
            x = put_segment(buffer, x, y, page.right(), shown, style);
        }
        // A selected line break shows as one selected cell at the line's end.
        let break_selected = selection.is_some_and(|(start, end)| {
            row.end == line.len() && (row.line, line.len()) >= start && (row.line, line.len()) < end
        });
        if break_selected && x < page.right() {
            put_text(buffer, x, y, 1, " ", Style::default().bg(selected_bg));
        }
    }
}

fn render_hint(
    buffer: &mut Buffer,
    geometry: &NotesGeometry,
    view: &ClientNotesView,
    styles: &Styles,
) {
    let row = geometry.hint;
    let (status, status_style) = if view.load != NotesLoad::Ready || view.open.is_none() {
        (String::new(), styles.dim)
    } else if view.conflict {
        ("● not saved".to_owned(), styles.red)
    } else if view.write_in_flight {
        ("saving…".to_owned(), styles.dim)
    } else if view.dirty() {
        ("● unsaved".to_owned(), styles.yellow)
    } else if view.editor.is_some() {
        ("✓ saved".to_owned(), styles.green)
    } else {
        let words = view
            .open_text()
            .map_or(0, |text| text.split_whitespace().count());
        (
            format!("{words} word{}", if words == 1 { "" } else { "s" }),
            styles.dim,
        )
    };
    let status_width = display_width(&status);
    if status_width > 0 {
        put_text(
            buffer,
            row.right().saturating_sub(status_width),
            row.y,
            status_width,
            &status,
            status_style,
        );
    }
    let room = row.width.saturating_sub(status_width + 2);
    let (text, style) = match view.message.as_deref() {
        Some(message) => (message, styles.yellow),
        None if view.switcher.is_some() => (
            "↑↓ choose · ↵ open · #tag filters · esc close",
            styles.dim,
        ),
        None if view.editor.is_some() => (
            "esc done · ctrl+s save · ctrl+z undo · ctrl+o find · ctrl+n new",
            styles.dim,
        ),
        None => (
            "click text to edit · [[link]] opens · #tag filters · ctrl+o find · n new · t today · [ ] prev/next · esc close",
            styles.dim,
        ),
    };
    put_text(buffer, row.x, row.y, room, &fit(text, room), style);
}

/// The quick switcher over the view, near the top like a command palette.
fn render_switcher(
    buffer: &mut Buffer,
    area: Rect,
    view: &ClientNotesView,
    styles: &Styles,
    hits: &mut NotesViewHits,
) -> Option<crate::protocol::CursorState> {
    let switcher = view.switcher.as_ref()?;
    let choices = view.switcher_choices();
    let shown = choices.len().clamp(1, SWITCHER_ROWS) as u16;
    let width = area.width.saturating_sub(4).min(72);
    let height = (shown + 5).min(area.height.saturating_sub(2));
    let popup = Rect::new(area.x + (area.width - width) / 2, area.y + 2, width, height);
    // Styles patch rather than replace: wipe the cells so nothing below shows through.
    for y in popup.y..popup.bottom() {
        for x in popup.x..popup.right() {
            buffer[(x, y)].reset();
            buffer[(x, y)].set_style(Style::default().bg(styles.palette.panel_bg));
        }
    }
    let frame_style = Style::default()
        .fg(styles.palette.accent)
        .bg(styles.palette.panel_bg);
    draw_box(buffer, popup, frame_style);
    hits.switcher = popup;
    hits.targets.push((popup, NotesTarget::Nothing));
    let label = match switcher.purpose {
        SwitcherPurpose::Find => " Find a note ",
        SwitcherPurpose::New => " New note ",
        SwitcherPurpose::Rename => " Rename note ",
    };
    put_text(
        buffer,
        popup.x + 2,
        popup.y,
        display_width(label),
        label,
        frame_style.add_modifier(Modifier::BOLD),
    );
    let inner = Rect::new(
        popup.x + 2,
        popup.y + 1,
        popup.width.saturating_sub(4),
        popup.height.saturating_sub(2),
    );
    let panel = Style::default().bg(styles.palette.panel_bg);
    let prompt_x = put_segment(
        buffer,
        inner.x,
        inner.y,
        inner.right(),
        "› ",
        styles.accent.patch(panel),
    );
    let input = Rect::new(prompt_x, inner.y, inner.right().saturating_sub(prompt_x), 1);
    hits.targets.push((
        Rect::new(inner.x, inner.y, inner.width, 1),
        NotesTarget::SwitcherInput,
    ));
    let cursor = if switcher.query.as_str().is_empty() {
        let placeholder = match switcher.purpose {
            SwitcherPurpose::Find => "type a name, or #tag",
            SwitcherPurpose::New => "name the note · folder/name makes a folder",
            SwitcherPurpose::Rename => "new name",
        };
        put_text(
            buffer,
            input.x,
            input.y,
            input.width,
            placeholder,
            styles.dim.patch(panel),
        );
        Some(crate::protocol::CursorState {
            x: input.x,
            y: input.y,
            visible: true,
            shape: 0,
        })
    } else {
        super::text_editor::render(buffer, input, &switcher.query, styles.base.patch(panel))
    };
    put_text(
        buffer,
        inner.x,
        inner.y + 1,
        inner.width,
        &"─".repeat(usize::from(inner.width)),
        styles.rule.patch(panel),
    );
    let list_top = inner.y + 2;
    let rows = usize::from(inner.bottom().saturating_sub(list_top + 1));
    // Keep the selected row in the window.
    let first = switcher.selected.saturating_sub(rows.saturating_sub(1));
    if choices.is_empty() {
        let empty = match switcher.purpose {
            SwitcherPurpose::Rename => "type a new name",
            _ => "no notes match",
        };
        put_text(
            buffer,
            inner.x,
            list_top,
            inner.width,
            empty,
            styles.dim.patch(panel),
        );
    }
    for (offset, choice) in choices.iter().enumerate().skip(first).take(rows) {
        let y = list_top + (offset - first) as u16;
        let rect = Rect::new(inner.x, y, inner.width, 1);
        let selected = offset == switcher.selected;
        let bg = if selected {
            styles.palette.surface0
        } else {
            styles.palette.panel_bg
        };
        buffer.set_style(rect, Style::default().bg(bg));
        let on = |style: Style| style.bg(bg);
        let mut x = rect.x;
        match choice {
            SwitcherChoice::Open(path) => {
                x = put_segment(
                    buffer,
                    x,
                    y,
                    rect.right(),
                    if selected { "› " } else { "  " },
                    on(styles.accent),
                );
                x = put_segment(
                    buffer,
                    x,
                    y,
                    rect.right(),
                    &fit(title(path), rect.right().saturating_sub(x)),
                    on(if selected { styles.bold } else { styles.base }),
                );
                let dir = folder(path);
                if !dir.is_empty() && x + 2 < rect.right() {
                    put_segment(
                        buffer,
                        x + 2,
                        y,
                        rect.right(),
                        &fit(dir, rect.right().saturating_sub(x + 2)),
                        on(styles.dim),
                    );
                }
            }
            SwitcherChoice::Create(path) => {
                x = put_segment(
                    buffer,
                    x,
                    y,
                    rect.right(),
                    "+ Create ",
                    on(styles.green.add_modifier(Modifier::BOLD)),
                );
                put_segment(
                    buffer,
                    x,
                    y,
                    rect.right(),
                    &fit(
                        &format!("“{}”", path.trim_end_matches(".md")),
                        rect.right().saturating_sub(x),
                    ),
                    on(styles.base),
                );
            }
            SwitcherChoice::Rename(path) => {
                x = put_segment(
                    buffer,
                    x,
                    y,
                    rect.right(),
                    "→ Rename to ",
                    on(styles.accent),
                );
                put_segment(
                    buffer,
                    x,
                    y,
                    rect.right(),
                    &fit(path, rect.right().saturating_sub(x)),
                    on(styles.base),
                );
            }
        }
        hits.targets.push((rect, NotesTarget::Switcher(offset)));
    }
    let footer = match switcher.purpose {
        SwitcherPurpose::Rename => "↵ rename · esc cancel",
        _ => "↑↓ choose · ↵ open · esc close",
    };
    put_text(
        buffer,
        inner.x,
        inner.bottom().saturating_sub(1),
        inner.width,
        footer,
        styles.dim.patch(panel),
    );
    cursor
}
