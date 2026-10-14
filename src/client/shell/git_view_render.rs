//! Drawing the git view into the pane area. Layout lives in one place
//! ([`GitViewGeometry`]) so the frame preparation that clamps scrolling and
//! the drawing that follows always agree.

use ratatui::style::Color;

use super::git_view::{
    base_name, ClientGitView, GitArmed, GitDiffLineKind, GitDiffRow, GitDiffSource, GitTone,
    GitViewFocus, GitViewHits, GitViewTab, GitViewTarget,
};
use super::render::{display_width, put_right_text, put_segment, put_text};
use super::*;
use crate::api::schema::{GitChangeKind, GitCommitInfo};

/// Columns before diff text: old and new line numbers, then the +/- sign.
const DIFF_GUTTER: u16 = 12;
const COMMIT_BOX_HEIGHT: u16 = 5;
const MIN_WIDTH: u16 = 50;
const MIN_HEIGHT: u16 = 12;
/// Rows of a commit message body shown before "… more".
const BODY_PREVIEW_ROWS: usize = 2;
/// Rows a message (a git error can run long) may take above the hint row.
const MAX_MESSAGE_ROWS: usize = 4;

pub(super) struct GitViewGeometry {
    pub(super) too_small: bool,
    header: Rect,
    left: Rect,
    divider_x: u16,
    right: Rect,
    tabs: Rect,
    list_head: Rect,
    pub(super) list: Rect,
    commit_box: Rect,
    commit_head: Rect,
    pub(super) files: Rect,
    diff_head: Rect,
    pub(super) diff: Rect,
    message: Rect,
    hint: Rect,
}

impl GitViewGeometry {
    pub(super) fn new(area: Rect, view: &ClientGitView) -> Self {
        let mut geometry = Self {
            too_small: area.width < MIN_WIDTH || area.height < MIN_HEIGHT,
            header: Rect::new(area.x, area.y, area.width, 1),
            left: Rect::default(),
            divider_x: 0,
            right: Rect::default(),
            tabs: Rect::default(),
            list_head: Rect::default(),
            list: Rect::default(),
            commit_box: Rect::default(),
            commit_head: Rect::default(),
            files: Rect::default(),
            diff_head: Rect::default(),
            diff: Rect::default(),
            message: Rect::default(),
            hint: Rect::default(),
        };
        if geometry.too_small {
            return geometry;
        }
        let body = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
        let left_width = percent(area.width, 36).clamp(34, 60).min(area.width - 24);
        geometry.left = Rect::new(body.x, body.y, left_width, body.height);
        geometry.divider_x = body.x + left_width;
        geometry.right = Rect::new(
            geometry.divider_x + 1,
            body.y,
            area.right().saturating_sub(geometry.divider_x + 1),
            body.height,
        );
        let left = geometry.left;
        geometry.tabs = Rect::new(left.x, left.y, left.width, 1);
        geometry.list_head = Rect::new(left.x, left.y + 2, left.width, 1);
        let list_top = left.y + 3;
        let list_bottom = match view.tab {
            GitViewTab::Changes => {
                geometry.commit_box = Rect::new(
                    left.x,
                    left.bottom() - COMMIT_BOX_HEIGHT,
                    left.width,
                    COMMIT_BOX_HEIGHT,
                );
                geometry.commit_box.y
            }
            GitViewTab::History => left.bottom(),
        };
        geometry.list = Rect::new(
            left.x,
            list_top,
            left.width,
            list_bottom.saturating_sub(list_top),
        );

        let right = geometry.right;
        geometry.hint = Rect::new(right.x, right.bottom() - 1, right.width, 1);
        let message_rows = shown_message(view).map_or(0, |(text, _)| {
            wrap(text, right.width.saturating_sub(2))
                .len()
                .clamp(1, MAX_MESSAGE_ROWS) as u16
        });
        geometry.message = Rect::new(
            right.x,
            geometry.hint.y.saturating_sub(message_rows),
            right.width,
            message_rows,
        );
        let content_bottom = geometry.message.y;
        match view.tab {
            GitViewTab::Changes => {
                geometry.diff_head = Rect::new(right.x, right.y, right.width, 1);
                geometry.diff = Rect::new(
                    right.x,
                    right.y + 1,
                    right.width,
                    content_bottom.saturating_sub(right.y + 1),
                );
            }
            GitViewTab::History => {
                let body_rows = view.selected_commit().map_or(0, |commit| {
                    commit_body_rows(commit, right.width, view.body_open, right.height)
                });
                let head_height = (3 + body_rows as u16).min(right.height / 2);
                geometry.commit_head = Rect::new(right.x, right.y, right.width, head_height);
                let below_top = right.y + head_height;
                let below_height = content_bottom.saturating_sub(below_top);
                let files_width = percent(right.width, 36).clamp(26, 48).min(right.width / 2);
                geometry.files = Rect::new(right.x, below_top, files_width, below_height);
                let diff_x = right.x + files_width + 1;
                let diff_width = right.right().saturating_sub(diff_x);
                geometry.diff_head = Rect::new(diff_x, below_top, diff_width, 1);
                geometry.diff = Rect::new(
                    diff_x,
                    below_top + 1,
                    diff_width,
                    below_height.saturating_sub(1),
                );
            }
        }
        geometry
    }

    /// Columns of diff text: the diff area less its gutter and scrollbar.
    pub(super) fn diff_text_width(&self) -> u16 {
        self.diff.width.saturating_sub(DIFF_GUTTER + 1).max(1)
    }
}

/// `share` percent of `width`, without overflowing on very wide terminals.
fn percent(width: u16, share: u32) -> u16 {
    (u32::from(width) * share / 100) as u16
}

/// The message under the diff: the last result, else what is running.
fn shown_message(view: &ClientGitView) -> Option<(&str, GitTone)> {
    view.message
        .as_ref()
        .map(|(text, tone)| (text.as_str(), *tone))
        .or_else(|| view.busy.map(|busy| (busy.label(), GitTone::Info)))
}

/// How many list rows the left column holds, and where the selection sits.
pub(super) fn list_rows(view: &ClientGitView) -> (usize, Option<(usize, usize)>) {
    match view.tab {
        GitViewTab::Changes => (
            view.files().len(),
            view.selected_file_index().map(|index| (index, 1)),
        ),
        GitViewTab::History => (
            view.commit_list().len() * 2 + usize::from(view.has_more),
            view.selected_commit_index().map(|index| (index * 2, 2)),
        ),
    }
}

fn commit_body_rows(commit: &GitCommitInfo, width: u16, open: bool, height: u16) -> usize {
    if commit.body.is_empty() {
        return 0;
    }
    let rows = wrap(&commit.body, width.saturating_sub(2)).len();
    if open {
        rows.min(usize::from(height / 3).max(BODY_PREVIEW_ROWS))
    } else {
        rows.min(BODY_PREVIEW_ROWS)
    }
}

/// Splits text into rows of at most `width` display columns, keeping line breaks.
fn wrap(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut row = String::new();
        let mut used = 0usize;
        for ch in line.chars() {
            let ch = if ch == '\t' { ' ' } else { ch };
            let cols = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cols > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(ch);
            used += cols;
        }
        rows.push(row);
    }
    rows
}

/// `text` cut to `width` columns, ending in "…" when it did not fit.
fn fit(text: &str, width: u16) -> std::borrow::Cow<'_, str> {
    let width = usize::from(width);
    if UnicodeWidthStr::width(text) <= width {
        return std::borrow::Cow::Borrowed(text);
    }
    if width == 0 {
        return std::borrow::Cow::Borrowed("");
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cols = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cols + 1 > width {
            break;
        }
        out.push(ch);
        used += cols;
    }
    out.push('…');
    std::borrow::Cow::Owned(out)
}

/// The `skip`-th chunk of `width` columns of `text`.
fn wrapped_chunk(text: &str, width: usize, skip: usize) -> &str {
    let mut chunk = 0usize;
    let mut used = 0usize;
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        let cols = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cols > width && used > 0 {
            if chunk == skip {
                return &text[start..index];
            }
            chunk += 1;
            used = 0;
            start = index;
        }
        used += cols;
    }
    if chunk == skip {
        &text[start..]
    } else {
        ""
    }
}

fn clamp_scroll(scroll: &mut usize, rows: usize, viewport: u16, reveal: Option<(usize, usize)>) {
    let viewport = usize::from(viewport);
    if let Some((top, height)) = reveal.filter(|_| viewport > 0) {
        if top < *scroll {
            *scroll = top;
        } else if top + height > *scroll + viewport {
            *scroll = (top + height).saturating_sub(viewport);
        }
    }
    *scroll = (*scroll).min(rows.saturating_sub(viewport));
}

impl ClientGitView {
    /// Fits scrolling to this frame's geometry and lays out the diff, before
    /// the pure drawing pass.
    pub(super) fn prepare_frame(&mut self, area: Rect) {
        let geometry = GitViewGeometry::new(area, self);
        if geometry.too_small {
            return;
        }
        let reveal = std::mem::take(&mut self.reveal_selection);
        let (rows, selection) = list_rows(self);
        clamp_scroll(
            &mut self.list_scroll,
            rows,
            geometry.list.height,
            selection.filter(|_| reveal),
        );
        let file_count = self.shown_commit_files().map_or(0, <[_]>::len);
        let selected_file = (self.selected_commit_file, 1);
        clamp_scroll(
            &mut self.files_scroll,
            file_count,
            geometry.files.height,
            reveal.then_some(selected_file),
        );
        let wanted = self.wanted_diff();
        let text_width = geometry.diff_text_width();
        let diff_rows = match self
            .diff
            .as_mut()
            .filter(|doc| Some(&doc.source) == wanted.as_ref())
        {
            Some(doc) => {
                doc.ensure_layout(text_width);
                doc.total_rows()
            }
            None => 0,
        };
        clamp_scroll(&mut self.diff_scroll, diff_rows, geometry.diff.height, None);
    }
}

struct Styles {
    base: Style,
    dim: Style,
    muted: Style,
    bold: Style,
    accent: Style,
    rule: Style,
    selected: Style,
    selected_unfocused: Style,
    button: Style,
    primary: Style,
    green: Style,
    red: Style,
    yellow: Style,
    mauve: Style,
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
            selected: Style::default().fg(contrast).bg(p.accent),
            selected_unfocused: base.bg(p.surface1),
            button: base.bg(p.surface0),
            primary: Style::default()
                .fg(contrast)
                .bg(p.accent)
                .add_modifier(Modifier::BOLD),
            green: Style::default().fg(p.green),
            red: Style::default().fg(p.red),
            yellow: Style::default().fg(p.yellow),
            mauve: Style::default().fg(p.mauve),
        }
    }

    fn tone(&self, tone: GitTone) -> Style {
        match tone {
            GitTone::Info => self.muted,
            GitTone::Warning => self.yellow,
            GitTone::Success => self.green,
            GitTone::Error => self.red,
        }
    }
}

pub(super) struct GitViewFrame {
    pub(super) hits: GitViewHits,
    pub(super) cursor: Option<crate::protocol::CursorState>,
}

/// Draws the view over `area`, which it owns entirely.
pub(super) fn render_git_view(
    buffer: &mut Buffer,
    area: Rect,
    view: &ClientGitView,
    space_label: &str,
    palette: &Palette,
) -> GitViewFrame {
    let area = area.intersection(buffer.area);
    let styles = Styles::new(palette);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].set_symbol(" ").set_style(Style::reset());
        }
    }
    let mut frame = GitViewFrame {
        hits: GitViewHits {
            area,
            ..GitViewHits::default()
        },
        cursor: None,
    };
    let geometry = GitViewGeometry::new(area, view);
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
            area.y + 2.min(area.height.saturating_sub(1)),
            area.width.saturating_sub(2),
            "the window is too small for the git view",
            styles.dim,
        );
        return frame;
    }
    let rule = "─".repeat(usize::from(area.width));
    put_text(buffer, area.x, area.y + 1, area.width, &rule, styles.rule);
    for y in geometry.left.y..geometry.left.bottom() {
        put_text(buffer, geometry.divider_x, y, 1, "│", styles.rule);
    }
    frame.hits.list = geometry.list;
    frame.hits.files = geometry.files;
    frame.hits.diff = geometry.diff;

    render_tabs(buffer, &geometry, view, &styles, &mut frame.hits);
    if view.unsupported {
        put_text(
            buffer,
            geometry.list.x + 1,
            geometry.list.y,
            geometry.list.width.saturating_sub(2),
            "this server has no git support yet · update and restart it",
            styles.yellow,
        );
        return frame;
    }
    match view.tab {
        GitViewTab::Changes => {
            render_changes_list(buffer, &geometry, view, &styles, &mut frame.hits);
            frame.cursor = render_commit_box(buffer, &geometry, view, &styles, &mut frame.hits);
            render_changes_diff(buffer, &geometry, view, &styles, palette, &mut frame.hits);
        }
        GitViewTab::History => {
            render_history_list(buffer, &geometry, view, &styles, &mut frame.hits);
            render_commit(buffer, &geometry, view, &styles, palette, &mut frame.hits);
        }
    }
    render_hint(buffer, &geometry, view, &styles);
    frame
}

fn render_header(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    space_label: &str,
    styles: &Styles,
    hits: &mut GitViewHits,
) {
    let row = geometry.header;
    let mut right = row.right();
    // Right side first, so the branch text yields when space runs out.
    let close = " esc ";
    right = right.saturating_sub(display_width(close));
    put_text(
        buffer,
        right,
        row.y,
        display_width(close),
        close,
        styles.dim,
    );
    hits.targets.push((
        Rect::new(right, row.y, display_width(close), 1),
        GitViewTarget::Close,
    ));
    let status = view.status.as_ref();
    let (ahead, behind) = status.map_or((0, 0), |status| (status.ahead, status.behind));
    if !geometry.too_small {
        for (label, target, style) in [
            (
                format!(" ↑ push {ahead} "),
                GitViewTarget::Push,
                if ahead > 0 {
                    styles.primary
                } else {
                    styles.button
                },
            ),
            (
                format!(" ↓ pull {behind} "),
                GitViewTarget::Pull,
                styles.button,
            ),
        ] {
            let width = display_width(&label);
            right = right.saturating_sub(width + 1);
            put_text(buffer, right, row.y, width, &label, style);
            hits.targets
                .push((Rect::new(right, row.y, width, 1), target));
        }
    }
    let end = right.saturating_sub(1);
    let mut x = put_segment(buffer, row.x + 1, row.y, end, "git", styles.bold);
    x = put_segment(buffer, x, row.y, end, " · ", styles.dim);
    x = put_segment(buffer, x, row.y, end, space_label, styles.bold);
    if let Some(status) = status {
        let branch = status.branch.as_deref().unwrap_or("detached HEAD");
        x = put_segment(buffer, x, row.y, end, "   ⎇ ", styles.mauve);
        x = put_segment(buffer, x, row.y, end, branch, styles.mauve);
        let upstream = status.upstream.as_deref().map_or_else(
            || "  no upstream".to_owned(),
            |upstream| format!("  → {upstream}"),
        );
        put_segment(buffer, x, row.y, end, &upstream, styles.dim);
    } else if let Some(error) = view.error.as_deref() {
        put_segment(buffer, x, row.y, end, &format!("   {error}"), styles.red);
    } else {
        put_segment(buffer, x, row.y, end, "   loading…", styles.dim);
    }
}

fn render_tabs(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    styles: &Styles,
    hits: &mut GitViewHits,
) {
    let row = geometry.tabs;
    let half = row.width / 2;
    let ahead = view.status.as_ref().map_or(0, |status| status.ahead);
    let changes = match view.status.as_ref() {
        Some(status) if status.truncated => format!("Changes {}+", status.files.len()),
        Some(status) => format!("Changes {}", status.files.len()),
        None => "Changes".to_owned(),
    };
    let history = if ahead > 0 {
        format!("History ↑{ahead}")
    } else {
        "History".to_owned()
    };
    for (index, (label, tab)) in [
        (changes, GitViewTab::Changes),
        (history, GitViewTab::History),
    ]
    .into_iter()
    .enumerate()
    {
        let rect = Rect::new(
            row.x + half * index as u16,
            row.y,
            if index == 0 { half } else { row.width - half },
            1,
        );
        let active = view.tab == tab;
        let width = display_width(&label).min(rect.width);
        put_text(
            buffer,
            rect.x + (rect.width - width) / 2,
            rect.y,
            width,
            &label,
            if active { styles.bold } else { styles.dim },
        );
        let underline = if active { "━" } else { "─" };
        put_text(
            buffer,
            rect.x,
            rect.y + 1,
            rect.width,
            &underline.repeat(usize::from(rect.width)),
            if active { styles.accent } else { styles.rule },
        );
        hits.targets.push((rect, GitViewTarget::Tab(tab)));
    }
}

fn change_letter(kind: GitChangeKind) -> (&'static str, fn(&Styles) -> Style) {
    match kind {
        GitChangeKind::Modified => ("M", |s| s.yellow),
        GitChangeKind::Added => ("A", |s| s.green),
        GitChangeKind::Deleted => ("D", |s| s.red),
        GitChangeKind::Renamed => ("R", |s| s.yellow),
        GitChangeKind::Copied => ("C", |s| s.green),
        GitChangeKind::TypeChanged => ("T", |s| s.yellow),
        GitChangeKind::Untracked => ("U", |s| s.green),
        GitChangeKind::Conflicted => ("!", |s| s.red),
        GitChangeKind::Unknown => ("?", |s| s.muted),
    }
}

fn short_dir(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    match parts.len() {
        0 | 1 => String::new(),
        2 | 3 => parts[..parts.len() - 1].join("/"),
        len => format!("…/{}", parts[len - 3..len - 1].join("/")),
    }
}

fn render_changes_list(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    styles: &Styles,
    hits: &mut GitViewHits,
) {
    let head = geometry.list_head;
    let files = view.files();
    let staged = files.iter().filter(|file| file.staged).count();
    let fully = files.iter().all(|file| file.staged && !file.unstaged);
    let all_box = if files.is_empty() || staged == 0 {
        "□"
    } else if fully {
        "▣"
    } else {
        "◪"
    };
    if view.status.is_some() {
        let label = match files.len() {
            1 => "1 changed file".to_owned(),
            count => format!("{count} changed files"),
        };
        let box_style = if fully && !files.is_empty() {
            styles.green
        } else {
            styles.muted
        };
        put_text(buffer, head.x + 1, head.y, 1, all_box, box_style);
        hits.targets
            .push((Rect::new(head.x, head.y, 3, 1), GitViewTarget::StageAll));
        put_text(
            buffer,
            head.x + 3,
            head.y,
            head.width.saturating_sub(3),
            &label,
            styles.muted,
        );
        if files.iter().any(|file| file.unstaged) {
            let armed = matches!(view.armed, Some(GitArmed::DiscardAll(_)));
            let text = if armed {
                " confirm discard all "
            } else {
                " ↶ discard all "
            };
            let width = display_width(text);
            let x = head.right().saturating_sub(width + 1);
            put_text(
                buffer,
                x,
                head.y,
                width,
                text,
                if armed { styles.red } else { styles.dim },
            );
            hits.targets
                .push((Rect::new(x, head.y, width, 1), GitViewTarget::DiscardAll));
        }
    }
    let list = geometry.list;
    if view.status.is_none() {
        let (text, style) = match view.error.as_deref() {
            Some(error) => (error, styles.red),
            None => ("loading…", styles.dim),
        };
        put_text(
            buffer,
            list.x + 2,
            list.y,
            list.width.saturating_sub(2),
            text,
            style,
        );
        return;
    }
    if files.is_empty() {
        put_text(
            buffer,
            list.x + 2,
            list.y + 1,
            list.width.saturating_sub(2),
            "no changes · working tree clean",
            styles.dim,
        );
        return;
    }
    let selected = view.selected_file_index();
    for (offset, (index, file)) in files
        .iter()
        .enumerate()
        .skip(view.list_scroll)
        .take(usize::from(list.height))
        .enumerate()
    {
        let y = list.y + offset as u16;
        let rect = Rect::new(list.x, y, list.width.saturating_sub(1), 1);
        let is_selected = selected == Some(index);
        let row_style = if is_selected {
            if view.focus == GitViewFocus::List {
                styles.selected
            } else {
                styles.selected_unfocused
            }
        } else {
            styles.base
        };
        let on_selected = |style: Style| if is_selected { row_style } else { style };
        buffer.set_style(rect, row_style);
        let checkbox = match (file.staged, file.unstaged) {
            (true, false) => ("▣", styles.green),
            (true, true) => ("◪", styles.yellow),
            _ => ("□", styles.muted),
        };
        put_text(
            buffer,
            rect.x + 1,
            y,
            1,
            checkbox.0,
            on_selected(checkbox.1),
        );
        // Later targets win a click: the row first, then its checkbox and buttons.
        hits.targets.push((rect, GitViewTarget::File(index)));
        hits.targets
            .push((Rect::new(rect.x, y, 3, 1), GitViewTarget::FileBox(index)));
        let (letter, letter_style) = change_letter(file.kind);
        // Right edge: status letter, and the discard button on the selected row.
        let mut right = rect.right().saturating_sub(2);
        put_text(
            buffer,
            right,
            y,
            1,
            letter,
            on_selected(letter_style(styles)),
        );
        if is_selected && file.unstaged {
            let armed = view.armed == Some(GitArmed::DiscardFile(file.path.clone()));
            let text = if armed { " discard? " } else { " ↶ " };
            let width = display_width(text);
            right = right.saturating_sub(width + 1);
            let style = if armed {
                Style::default()
                    .fg(Color::Reset)
                    .bg(styles.red.fg.unwrap_or(Color::Red))
            } else {
                row_style
            };
            put_text(buffer, right, y, width, text, style);
            hits.targets.push((
                Rect::new(right, y, width, 1),
                GitViewTarget::DiscardFile(index),
            ));
        }
        let name_end = right.saturating_sub(1);
        let name = fit(base_name(&file.path), name_end.saturating_sub(rect.x + 3));
        let x = put_segment(buffer, rect.x + 3, y, name_end, &name, row_style);
        let dir = short_dir(&file.path);
        if !dir.is_empty() {
            put_segment(buffer, x + 1, y, name_end, &dir, on_selected(styles.dim));
        }
    }
    render_list_scrollbar(buffer, list, files.len(), view.list_scroll, styles);
}

fn render_list_scrollbar(
    buffer: &mut Buffer,
    list: Rect,
    rows: usize,
    scroll: usize,
    styles: &Styles,
) {
    let viewport = usize::from(list.height);
    if rows <= viewport || list.width == 0 {
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
        Rect::new(list.right() - 1, list.y, 1, list.height),
        styles.rule.fg.unwrap_or(Color::Reset),
        styles.dim.fg.unwrap_or(Color::Reset),
        "▐",
    );
}

fn render_commit_box(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    styles: &Styles,
    hits: &mut GitViewHits,
) -> Option<crate::protocol::CursorState> {
    let area = geometry.commit_box;
    let rule = "─".repeat(usize::from(area.width));
    put_text(buffer, area.x, area.y, area.width, &rule, styles.rule);
    let field_x = area.x + 1;
    let field_width = area.width.saturating_sub(2);
    let mut cursor = None;
    for (row, editor, placeholder, focus) in [
        (
            area.y + 1,
            &view.summary,
            "summary (required)",
            GitViewFocus::Summary,
        ),
        (
            area.y + 2,
            &view.description,
            "description",
            GitViewFocus::Description,
        ),
    ] {
        let rect = Rect::new(field_x, row, field_width, 1);
        let focused = view.focus == focus;
        let style = if focused {
            styles.base.bg(styles.button.bg.unwrap_or(Color::Reset))
        } else {
            styles.button
        };
        buffer.set_style(rect, style);
        let text_rect = Rect::new(rect.x + 1, row, rect.width.saturating_sub(2), 1);
        if focused {
            cursor = text_editor::render(buffer, text_rect, editor, style);
        } else if editor.as_str().is_empty() {
            put_text(
                buffer,
                text_rect.x,
                row,
                text_rect.width,
                placeholder,
                style.patch(styles.dim),
            );
        } else {
            let (text, _) = editor.viewport(text_rect.width);
            put_text(buffer, text_rect.x, row, text_rect.width, text, style);
        }
        if focused {
            // The focused field's edge, in the accent colour.
            put_text(
                buffer,
                rect.x,
                row,
                1,
                "▌",
                styles.accent.bg(style.bg.unwrap_or(Color::Reset)),
            );
        }
        hits.targets.push((rect, GitViewTarget::Field(focus)));
    }
    let button = Rect::new(field_x, area.y + 3, field_width, 1);
    let branch = view
        .status
        .as_ref()
        .and_then(|status| status.branch.as_deref())
        .unwrap_or("HEAD");
    let staged = view.staged_count();
    let files = view.files().len();
    let (label, style) = match view.busy {
        Some(busy) => (busy.label().to_owned(), styles.button),
        None if files == 0 => (
            "nothing to commit".to_owned(),
            styles.button.patch(styles.dim),
        ),
        None if staged == 0 => (format!("✓ Commit all {files} to {branch}"), styles.primary),
        None if staged == 1 => (format!("✓ Commit 1 file to {branch}"), styles.primary),
        None => (
            format!("✓ Commit {staged} files to {branch}"),
            styles.primary,
        ),
    };
    buffer.set_style(button, style);
    let width = display_width(&label).min(button.width);
    put_text(
        buffer,
        button.x + (button.width - width) / 2,
        button.y,
        width,
        &label,
        style,
    );
    hits.targets.push((button, GitViewTarget::CommitButton));
    let status_row = Rect::new(field_x, area.y + 4, field_width, 1);
    if let Some(undo) = view.undo.as_ref() {
        let label = format!("committed {} · ", short_ago(undo.at.elapsed().as_secs()));
        let x = put_segment(
            buffer,
            status_row.x,
            status_row.y,
            status_row.right(),
            &label,
            styles.muted,
        );
        let undo_label = " ↶ undo ";
        let width = display_width(undo_label).min(status_row.right().saturating_sub(x));
        put_text(buffer, x, status_row.y, width, undo_label, styles.button);
        hits.targets
            .push((Rect::new(x, status_row.y, width, 1), GitViewTarget::Undo));
    } else {
        put_text(
            buffer,
            status_row.x,
            status_row.y,
            status_row.width,
            "↵ commit · alt+↵ commit & push",
            styles.dim,
        );
    }
    cursor
}

fn short_ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn render_changes_diff(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    styles: &Styles,
    palette: &Palette,
    hits: &mut GitViewHits,
) {
    let head = geometry.diff_head;
    let Some(file) = view.selected_file() else {
        let text = if view.status.is_some() && view.files().is_empty() {
            "nothing changed · Tab shows the history"
        } else {
            ""
        };
        put_text(
            buffer,
            geometry.diff.x + 2,
            geometry.diff.y + 1,
            geometry.diff.width.saturating_sub(2),
            text,
            styles.dim,
        );
        return;
    };
    buffer.set_style(head, Style::default().bg(palette.panel_bg));
    let state = match (file.kind, file.staged, file.unstaged) {
        (GitChangeKind::Untracked, _, _) => "new file",
        (_, true, false) => "staged",
        (_, true, true) => "partly staged",
        _ => "not staged",
    };
    let mut right = head.right();
    let index = view.selected_file_index();
    let mut buttons = Vec::new();
    if file.unstaged {
        let armed = view.armed == Some(GitArmed::DiscardFile(file.path.clone()));
        buttons.push((
            if armed {
                " confirm discard "
            } else {
                " ↶ discard "
            },
            index.map(GitViewTarget::DiscardFile),
            if armed {
                styles.red.bg(palette.surface0)
            } else {
                styles.button
            },
        ));
    }
    let fully_staged = file.staged && !file.unstaged;
    buttons.push((
        if fully_staged {
            " − unstage file "
        } else {
            " + stage file "
        },
        index.map(GitViewTarget::FileBox),
        styles.button,
    ));
    for (label, target, style) in buttons {
        let width = display_width(label);
        right = right.saturating_sub(width + 1);
        put_text(buffer, right, head.y, width, label, style);
        if let Some(target) = target {
            hits.targets
                .push((Rect::new(right, head.y, width, 1), target));
        }
    }
    let end = right.saturating_sub(1);
    let mut x = put_segment(buffer, head.x + 1, head.y, end, &file.path, styles.bold);
    if let Some(orig) = file.orig_path.as_deref() {
        x = put_segment(buffer, x, head.y, end, &format!(" ← {orig}"), styles.dim);
    }
    put_segment(buffer, x, head.y, end, &format!("  {state}"), styles.dim);
    render_diff(buffer, geometry, view, styles, hits);
}

fn render_diff(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    styles: &Styles,
    hits: &mut GitViewHits,
) {
    let area = geometry.diff;
    let Some(doc) = view.shown_diff() else {
        put_text(
            buffer,
            area.x + 2,
            area.y + 1,
            area.width.saturating_sub(2),
            "loading…",
            styles.dim,
        );
        return;
    };
    // Too narrow for the line numbers and any text.
    if area.width <= DIFF_GUTTER + 1 {
        return;
    }
    let text_width = geometry.diff_text_width();
    // A renamed file is staged or unstaged as a whole.
    let stageable = matches!(doc.source, GitDiffSource::WorkingTree { .. })
        && view.busy.is_none()
        && view
            .selected_file()
            .is_none_or(|file| file.orig_path.is_none());
    for offset in 0..area.height {
        let Some((index, skip)) = doc.locate(view.diff_scroll + usize::from(offset)) else {
            break;
        };
        let y = area.y + offset;
        match &doc.rows[index] {
            GitDiffRow::Gap => {}
            GitDiffRow::Note(text) => {
                put_text(
                    buffer,
                    area.x + 2,
                    y,
                    area.width.saturating_sub(3),
                    text,
                    styles.dim,
                );
            }
            GitDiffRow::Hunk { header, staged } => {
                let row = Rect::new(area.x, y, area.width.saturating_sub(1), 1);
                buffer.set_style(row, styles.button);
                let mut right = row.right();
                if let Some(staged) = *staged {
                    if stageable {
                        let label = if staged {
                            " − unstage hunk "
                        } else {
                            " + stage hunk "
                        };
                        let width = display_width(label);
                        right = right.saturating_sub(width);
                        put_text(buffer, right, y, width, label, styles.primary);
                        hits.targets.push((
                            Rect::new(right, y, width, 1),
                            GitViewTarget::Hunk {
                                header: header.clone(),
                                staged,
                            },
                        ));
                    }
                    if staged {
                        let tag = " ✓ staged ";
                        let width = display_width(tag);
                        right = right.saturating_sub(width);
                        put_text(
                            buffer,
                            right,
                            y,
                            width,
                            tag,
                            styles.green.bg(styles.button.bg.unwrap_or(Color::Reset)),
                        );
                    }
                }
                put_text(
                    buffer,
                    row.x + 1,
                    y,
                    right.saturating_sub(row.x + 2),
                    header,
                    styles.accent.bg(styles.button.bg.unwrap_or(Color::Reset)),
                );
            }
            GitDiffRow::Line {
                kind,
                old,
                new,
                text,
                ..
            } => {
                let (sign, style) = match kind {
                    GitDiffLineKind::Added => ("+", styles.green),
                    GitDiffLineKind::Removed => ("-", styles.red),
                    GitDiffLineKind::Context => (" ", styles.base),
                    GitDiffLineKind::Marker => ("", styles.dim),
                };
                if skip == 0 {
                    let number =
                        |value: &Option<u32>| value.map_or_else(String::new, |n| n.to_string());
                    put_text(
                        buffer,
                        area.x,
                        y,
                        5,
                        &format!("{:>5}", number(old)),
                        styles.dim,
                    );
                    put_text(
                        buffer,
                        area.x + 5,
                        y,
                        5,
                        &format!("{:>5}", number(new)),
                        styles.dim,
                    );
                    put_text(buffer, area.x + 11, y, 1, sign, style);
                }
                let chunk = wrapped_chunk(text, usize::from(text_width), skip);
                put_text(buffer, area.x + DIFF_GUTTER, y, text_width, chunk, style);
            }
        }
    }
    let total = doc.total_rows();
    let viewport = usize::from(area.height);
    if total > viewport {
        let max = total - viewport;
        crate::ui::render_scrollbar_buffer(
            buffer,
            crate::pane::ScrollMetrics {
                offset_from_bottom: max.saturating_sub(view.diff_scroll),
                max_offset_from_bottom: max,
                viewport_rows: viewport,
            },
            Rect::new(area.right() - 1, area.y, 1, area.height),
            styles.rule.fg.unwrap_or(Color::Reset),
            styles.dim.fg.unwrap_or(Color::Reset),
            "▐",
        );
    }
}

fn render_history_list(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    styles: &Styles,
    hits: &mut GitViewHits,
) {
    let head = geometry.list_head;
    if let Some(status) = view.status.as_ref() {
        let text = if status.ahead > 0 {
            format!("↑ {} not pushed", status.ahead)
        } else if let Some(upstream) = status.upstream.as_deref() {
            format!("all pushed to {upstream}")
        } else {
            "no upstream yet · push publishes the branch".to_owned()
        };
        let style = if status.ahead > 0 {
            styles.green
        } else {
            styles.muted
        };
        put_text(
            buffer,
            head.x + 1,
            head.y,
            head.width.saturating_sub(2),
            &text,
            style,
        );
        if status.behind > 0 {
            let label = format!(" ↓ {} new · pull ", status.behind);
            let width = display_width(&label);
            let x = head.right().saturating_sub(width + 1);
            put_text(buffer, x, head.y, width, &label, styles.red);
            hits.targets
                .push((Rect::new(x, head.y, width, 1), GitViewTarget::Pull));
        }
    }
    let list = geometry.list;
    let Some(commits) = view.commits.as_deref() else {
        put_text(
            buffer,
            list.x + 2,
            list.y,
            list.width.saturating_sub(2),
            "loading…",
            styles.dim,
        );
        return;
    };
    if commits.is_empty() {
        put_text(
            buffer,
            list.x + 2,
            list.y + 1,
            list.width.saturating_sub(2),
            "no commits yet",
            styles.dim,
        );
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let local_now = crate::platform::local_datetime();
    let selected = view.selected_commit_index();
    let (rows, _) = list_rows(view);
    for offset in 0..list.height {
        let row_index = view.list_scroll + usize::from(offset);
        if row_index >= rows {
            break;
        }
        let y = list.y + offset;
        let rect = Rect::new(list.x, y, list.width.saturating_sub(1), 1);
        let index = row_index / 2;
        let Some(commit) = commits.get(index) else {
            put_text(
                buffer,
                rect.x + 2,
                y,
                rect.width.saturating_sub(2),
                &format!("showing the newest {}", commits.len()),
                styles.dim,
            );
            continue;
        };
        let is_selected = selected == Some(index);
        let row_style = if is_selected {
            styles.selected
        } else {
            styles.base
        };
        buffer.set_style(rect, row_style);
        hits.targets.push((rect, GitViewTarget::Commit(index)));
        let on_selected = |style: Style| if is_selected { row_style } else { style };
        if row_index.is_multiple_of(2) {
            let mark = if commit.pushed { "" } else { "↑" };
            let right = rect.right().saturating_sub(2);
            if !mark.is_empty() {
                put_text(buffer, right, y, 1, mark, on_selected(styles.green));
            }
            let subject_style = if commit.merge {
                on_selected(styles.muted)
            } else {
                row_style
            };
            let room = right.saturating_sub(rect.x + 2);
            put_text(
                buffer,
                rect.x + 1,
                y,
                room,
                &fit(&commit.subject, room),
                subject_style,
            );
        } else {
            let clock = commit_clock(commit.time, now, local_now);
            let ago = short_ago(now.saturating_sub(commit.time));
            let files = match commit.files_changed {
                0 => String::new(),
                1 => "1 file".to_owned(),
                count => format!("{count} files"),
            };
            let files_width = display_width(&files);
            let right = rect.right().saturating_sub(files_width + 1);
            put_text(
                buffer,
                right,
                y,
                files_width,
                &files,
                on_selected(styles.dim),
            );
            let x = put_segment(
                buffer,
                rect.x + 1,
                y,
                right.saturating_sub(1),
                &clock,
                on_selected(styles.muted),
            );
            put_segment(
                buffer,
                x,
                y,
                right.saturating_sub(1),
                &format!(" · {ago} · {}", commit.author),
                on_selected(styles.dim),
            );
        }
    }
    render_list_scrollbar(buffer, list, rows, view.list_scroll, styles);
}

/// "18:44" today, "yesterday 18:44", "Mon 18:44" this week, "Oct 3 18:44"
/// this year, else "Oct 3 2024".
fn commit_clock(time: u64, now: u64, local_now: Option<time::PrimitiveDateTime>) -> String {
    let ago = time::Duration::seconds(now.saturating_sub(time) as i64);
    let Some((at, today)) = local_now.and_then(|local| Some((local.checked_sub(ago)?, local)))
    else {
        return String::new();
    };
    let clock = format!("{:02}:{:02}", at.hour(), at.minute());
    let days = (today.date() - at.date()).whole_days();
    match days {
        0 => clock,
        1 => format!("yesterday {clock}"),
        2..=6 => format!("{} {clock}", &at.weekday().to_string()[..3]),
        _ if at.year() == today.year() => {
            format!("{} {} {clock}", &at.month().to_string()[..3], at.day())
        }
        _ => format!(
            "{} {} {}",
            &at.month().to_string()[..3],
            at.day(),
            at.year()
        ),
    }
}

fn render_commit(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    styles: &Styles,
    palette: &Palette,
    hits: &mut GitViewHits,
) {
    let head = geometry.commit_head;
    let Some(commit) = view.selected_commit() else {
        let text = if view.commits.is_some() {
            "select a commit"
        } else {
            ""
        };
        put_text(
            buffer,
            head.x + 2,
            head.y + 1,
            head.width.saturating_sub(2),
            text,
            styles.dim,
        );
        return;
    };
    buffer.set_style(head, Style::default().bg(palette.panel_bg));
    let panel = |style: Style| style.bg(palette.panel_bg);
    let where_text = if commit.pushed {
        view.status
            .as_ref()
            .and_then(|status| status.upstream.clone())
            .map_or_else(|| "pushed".to_owned(), |upstream| format!("on {upstream}"))
    } else {
        "↑ not pushed".to_owned()
    };
    let where_style = if commit.pushed {
        styles.dim
    } else {
        styles.green
    };
    let where_width = display_width(&where_text);
    let right = head.right().saturating_sub(where_width + 1);
    put_text(
        buffer,
        right,
        head.y,
        where_width,
        &where_text,
        panel(where_style),
    );
    put_text(
        buffer,
        head.x + 1,
        head.y,
        right.saturating_sub(head.x + 2),
        &commit.subject,
        panel(styles.bold),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let clock = commit_clock(commit.time, now, crate::platform::local_datetime());
    let files = match commit.files_changed {
        1 => "1 file".to_owned(),
        count => format!("{count} files"),
    };
    let mut x = put_segment(
        buffer,
        head.x + 1,
        head.y + 1,
        head.right(),
        &format!(
            "{} · {clock} ({}) · {} · {files} ",
            commit.author,
            short_ago(now.saturating_sub(commit.time)),
            commit.short_commit
        ),
        panel(styles.muted),
    );
    x = put_segment(
        buffer,
        x,
        head.y + 1,
        head.right(),
        &format!("+{}", commit.insertions),
        panel(styles.green),
    );
    put_segment(
        buffer,
        x,
        head.y + 1,
        head.right(),
        &format!(" −{}", commit.deletions),
        panel(styles.red),
    );
    if !commit.body.is_empty() {
        let rows = wrap(&commit.body, head.width.saturating_sub(2));
        let room = usize::from(head.height.saturating_sub(3));
        let shown = commit_body_rows(
            commit,
            geometry.right.width,
            view.body_open,
            geometry.right.height,
        )
        .min(room);
        for (offset, row) in rows.iter().take(shown).enumerate() {
            put_text(
                buffer,
                head.x + 1,
                head.y + 2 + offset as u16,
                head.width.saturating_sub(2),
                row,
                panel(styles.muted),
            );
        }
        if rows.len() > shown || view.body_open {
            let label = if rows.len() > shown {
                " … more "
            } else {
                " less "
            };
            let width = display_width(label);
            let y = head.y + 1 + shown as u16;
            let x = head.right().saturating_sub(width + 1);
            put_text(
                buffer,
                x,
                y.max(head.y + 1),
                width,
                label,
                panel(styles.accent),
            );
            hits.targets.push((
                Rect::new(x, y.max(head.y + 1), width, 1),
                GitViewTarget::BodyToggle,
            ));
        }
    }
    let rule_y = head.bottom().saturating_sub(1);
    put_text(
        buffer,
        head.x,
        rule_y,
        head.width,
        &"─".repeat(usize::from(head.width)),
        styles.rule,
    );

    let files_area = geometry.files;
    for y in files_area.y..files_area.bottom() {
        put_text(buffer, files_area.right(), y, 1, "│", styles.rule);
    }
    let Some(files) = view.shown_commit_files() else {
        put_text(
            buffer,
            files_area.x + 1,
            files_area.y,
            files_area.width.saturating_sub(1),
            "loading…",
            styles.dim,
        );
        return;
    };
    if files.is_empty() {
        let text = if commit.merge {
            "no changes of its own"
        } else {
            "no files"
        };
        put_text(
            buffer,
            files_area.x + 1,
            files_area.y,
            files_area.width.saturating_sub(1),
            text,
            styles.dim,
        );
        return;
    }
    for (offset, (index, file)) in files
        .iter()
        .enumerate()
        .skip(view.files_scroll)
        .take(usize::from(files_area.height))
        .enumerate()
    {
        let y = files_area.y + offset as u16;
        let rect = Rect::new(files_area.x, y, files_area.width, 1);
        let is_selected = index == view.selected_commit_file;
        let row_style = if is_selected {
            styles.selected
        } else {
            styles.base
        };
        let on_selected = |style: Style| if is_selected { row_style } else { style };
        buffer.set_style(rect, row_style);
        hits.targets.push((rect, GitViewTarget::CommitFile(index)));
        let (letter, letter_style) = change_letter(file.kind);
        put_text(
            buffer,
            rect.x + 1,
            y,
            1,
            letter,
            on_selected(letter_style(styles)),
        );
        let stats = if file.binary {
            "bin".to_owned()
        } else {
            format!("+{} −{}", file.insertions, file.deletions)
        };
        let stats_width = display_width(&stats);
        let right = rect.right().saturating_sub(stats_width + 1);
        put_text(
            buffer,
            right,
            y,
            stats_width,
            &stats,
            on_selected(styles.dim),
        );
        let room = right.saturating_sub(rect.x + 4);
        put_text(
            buffer,
            rect.x + 3,
            y,
            room,
            &fit(base_name(&file.path), room),
            row_style,
        );
    }
    render_list_scrollbar(
        buffer,
        Rect::new(
            files_area.x,
            files_area.y,
            files_area.width + 1,
            files_area.height,
        ),
        files.len(),
        view.files_scroll,
        styles,
    );

    let diff_head = geometry.diff_head;
    if let Some(file) = files.get(view.selected_commit_file) {
        buffer.set_style(diff_head, Style::default().bg(palette.panel_bg));
        let x = put_segment(
            buffer,
            diff_head.x + 1,
            diff_head.y,
            diff_head.right(),
            &file.path,
            panel(styles.bold),
        );
        if let Some(orig) = file.orig_path.as_deref() {
            put_segment(
                buffer,
                x,
                diff_head.y,
                diff_head.right(),
                &format!(" ← {orig}"),
                panel(styles.dim),
            );
        }
    }
    render_diff(buffer, geometry, view, styles, hits);
}

fn render_hint(
    buffer: &mut Buffer,
    geometry: &GitViewGeometry,
    view: &ClientGitView,
    styles: &Styles,
) {
    let row = geometry.hint;
    // The longest hint that fits.
    let hints: &[&str] = match view.tab {
        GitViewTab::Changes => &[
            "↑↓ file · space stage · a all · d discard · c commit · p push · l pull · tab history · esc close",
            "space stage · d discard · c commit · p push · l pull · tab history · esc",
            "space stage · c commit · tab history · esc",
        ],
        GitViewTab::History => &[
            "↑↓ commit · [ ] file · pgup/pgdn scroll · p push · l pull · tab changes · esc close",
            "↑↓ commit · [ ] file · p push · l pull · tab changes · esc",
            "[ ] file · tab changes · esc",
        ],
    };
    let room = row.width.saturating_sub(2);
    if let Some(text) = hints.iter().find(|hint| display_width(hint) <= room) {
        put_right_text(
            buffer,
            Rect::new(row.x, row.y, row.width.saturating_sub(1), 1),
            row.y,
            text,
            styles.dim,
        );
    }
    let area = geometry.message;
    if let Some((message, tone)) = shown_message(view).filter(|_| area.height > 0) {
        let rows = wrap(message, area.width.saturating_sub(2));
        let style = styles
            .tone(tone)
            .bg(styles.button.bg.unwrap_or(Color::Reset));
        buffer.set_style(area, style);
        for (offset, row) in rows.iter().take(usize::from(area.height)).enumerate() {
            let last =
                offset + 1 == usize::from(area.height) && rows.len() > usize::from(area.height);
            let text = if last {
                format!("{row} …")
            } else {
                row.clone()
            };
            put_text(
                buffer,
                area.x + 1,
                area.y + offset as u16,
                area.width.saturating_sub(2),
                &text,
                style,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_cuts_with_an_ellipsis_only_when_needed() {
        assert_eq!(fit("short.rs", 10), "short.rs");
        assert_eq!(fit("checkpoint_picker.rs", 10), "checkpoin…");
        assert_eq!(fit("漢字かな", 5), "漢字…");
        assert_eq!(fit("abc", 0), "");
    }

    #[test]
    fn commit_times_read_naturally() {
        let now = time::PrimitiveDateTime::new(
            time::Date::from_calendar_date(2026, time::Month::October, 14).unwrap(),
            time::Time::from_hms(18, 50, 0).unwrap(),
        );
        let at = |minutes_ago: i64| {
            let ago = (now - time::Duration::minutes(minutes_ago))
                .assume_utc()
                .unix_timestamp();
            commit_clock(
                ago as u64,
                now.assume_utc().unix_timestamp() as u64,
                Some(now),
            )
        };
        assert_eq!(at(6), "18:44");
        assert_eq!(at(24 * 60), "yesterday 18:50");
        assert_eq!(at(3 * 24 * 60), "Sun 18:50");
        assert_eq!(at(30 * 24 * 60), "Sep 14 18:50");
        assert_eq!(at(400 * 24 * 60), "Sep 9 2025");
    }

    #[test]
    fn wrapping_splits_by_display_width_and_keeps_line_breaks() {
        assert_eq!(wrap("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap("ab\ncd", 4), ["ab", "cd"]);
        assert_eq!(wrap("漢字かな", 4), ["漢字", "かな"]);
        assert_eq!(wrapped_chunk("0123456789", 4, 0), "0123");
        assert_eq!(wrapped_chunk("0123456789", 4, 2), "89");
        assert_eq!(wrapped_chunk("0123456789", 4, 3), "");
        assert_eq!(wrapped_chunk("漢字かな", 3, 1), "字");
    }

    #[test]
    fn scroll_clamps_to_content_and_reveals_the_selection() {
        let mut scroll = 50;
        clamp_scroll(&mut scroll, 20, 10, None);
        assert_eq!(scroll, 10);
        clamp_scroll(&mut scroll, 20, 10, Some((2, 1)));
        assert_eq!(scroll, 2);
        clamp_scroll(&mut scroll, 20, 10, Some((15, 2)));
        assert_eq!(scroll, 7);
        assert_eq!(short_dir("a/b/c/d/e.rs"), "…/c/d");
        assert_eq!(short_dir("a/e.rs"), "a");
        assert_eq!(short_dir("e.rs"), "");
    }
}
