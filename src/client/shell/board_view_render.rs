//! Drawing the Board into the pane area. Layout lives in [`BoardGeometry`]
//! so the frame preparation that clamps scrolling and the drawing that
//! follows always agree.

use ratatui::style::Color;

use super::board_view::{
    BoardField, BoardFilter, BoardLoad, BoardMode, BoardRow, BoardSortKey, BoardTarget,
    BoardViewHits, ClientBoardView,
};
use super::git_view_render::{fit, wrap};
use super::render::{display_width, put_segment, put_text};
use super::*;
use crate::api::schema::TaskSummary;

/// The board keeps at least this many columns beside docked details;
/// narrower, details open as a popup instead.
const MIN_BOARD_WIDTH: u16 = 60;
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;
const SEARCH_WIDTH: u16 = 24;
/// The toolbar spells out filter names from this width.
const WIDE_TOOLBAR: u16 = 110;

pub(super) struct BoardGeometry {
    too_small: bool,
    header: Rect,
    toolbar: Rect,
    pub(super) body: Rect,
    hint: Rect,
    /// Docked details, beside the board, full height.
    docked: Option<Rect>,
    /// Details over the board, when there is no room to dock them.
    popup: Option<Rect>,
}

impl BoardGeometry {
    pub(super) fn new(area: Rect, view: &ClientBoardView) -> Self {
        let too_small = area.width < MIN_WIDTH || area.height < MIN_HEIGHT;
        let details = view.details_open && view.selected_task().is_some() && !too_small;
        let dock_width = (area.width * 3 / 10).clamp(34, 46);
        let docked = (details && area.width.saturating_sub(dock_width + 1) >= MIN_BOARD_WIDTH)
            .then(|| Rect::new(area.right() - dock_width, area.y, dock_width, area.height));
        let popup = (details && docked.is_none()).then(|| {
            let width = area.width.saturating_sub(4).min(78);
            let height = area.height.saturating_sub(2);
            Rect::new(
                area.x + (area.width - width) / 2,
                area.y + (area.height - height) / 2,
                width,
                height,
            )
        });
        let main_width = docked.map_or(area.width, |dock| area.width - dock.width - 1);
        let main = Rect::new(area.x, area.y, main_width, area.height);
        // One blank row above the header keeps it off the tab bar.
        let header = Rect::new(main.x + 1, main.y + 1, main.width.saturating_sub(2), 1);
        let toolbar = Rect::new(header.x, main.y + 3, header.width, 1);
        let body_top = main.y + 5;
        let hint = Rect::new(header.x, main.bottom().saturating_sub(1), header.width, 1);
        let body = Rect::new(
            header.x,
            body_top,
            header.width,
            hint.y.saturating_sub(body_top + 1),
        );
        Self {
            too_small,
            header,
            toolbar,
            body,
            hint,
            docked,
            popup,
        }
    }

    /// Where details text goes, inside the dock or the popup.
    fn details_text(&self) -> Option<Rect> {
        self.docked
            .map(|dock| {
                Rect::new(
                    dock.x + 1,
                    dock.y + 1,
                    dock.width.saturating_sub(2),
                    dock.height.saturating_sub(2),
                )
            })
            .or_else(|| {
                self.popup.map(|popup| {
                    Rect::new(
                        popup.x + 3,
                        popup.y + 1,
                        popup.width.saturating_sub(6),
                        popup.height.saturating_sub(2),
                    )
                })
            })
    }
}

/// List rows below the column header, at most.
fn list_viewport(geometry: &BoardGeometry) -> u16 {
    geometry.body.height.saturating_sub(2)
}

impl ClientBoardView {
    /// Fits scrolling to this frame's geometry, before the pure drawing pass.
    pub(super) fn prepare_frame(&mut self, area: Rect, palette: &Palette) {
        let geometry = BoardGeometry::new(area, self);
        if geometry.too_small {
            return;
        }
        let reveal = std::mem::take(&mut self.reveal_selection);
        let rows = self.list_rows();
        // The new task's field when one is open, else the selected row.
        let selected = rows.iter().position(|row| match row {
            BoardRow::Draft => true,
            BoardRow::Task(task) | BoardRow::Subtask { task, .. } => {
                self.draft.is_none() && Some(&task.id) == self.selected.as_ref()
            }
            _ => false,
        });
        let row_count = rows.len();
        let viewport = usize::from(list_viewport(&geometry));
        if let Some(index) = selected.filter(|_| reveal && viewport > 0) {
            if index < self.list_scroll {
                self.list_scroll = index;
            } else if index >= self.list_scroll + viewport {
                self.list_scroll = index + 1 - viewport;
            }
        }
        self.list_scroll = self.list_scroll.min(row_count.saturating_sub(viewport));
        let statuses: Vec<String> = self
            .shown_statuses()
            .into_iter()
            .map(str::to_owned)
            .collect();
        for status in statuses {
            // Cards differ in height; the column draws what fits from here.
            let cards = self.column(&status).len();
            if let Some(scroll) = self.column_scroll.get_mut(&status) {
                *scroll = (*scroll).min(cards.saturating_sub(1));
            }
        }
        if let Some(text) = geometry.details_text() {
            let lines = detail_lines(self, text.width, &Styles::new(palette)).len();
            self.details_scroll = self
                .details_scroll
                .min(lines.saturating_sub(usize::from(text.height)));
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
    panel: Style,
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
            panel: base.bg(p.panel_bg),
            contrast,
            palette: p.clone(),
        }
    }

    /// A status's color: the usual Backlog.md names get their own, others stay muted.
    /// ponytail: matched by name; a per-project color map can come with config.
    fn status(&self, status: &str) -> Style {
        let p = &self.palette;
        let color = match status.to_ascii_lowercase().as_str() {
            "to do" | "todo" | "backlog" => p.text,
            "in progress" | "doing" => p.accent,
            "review" | "in review" => p.mauve,
            "done" => p.green,
            "blocked" => p.red,
            _ => p.overlay1,
        };
        Style::default().fg(color)
    }

    fn priority(&self, priority: Option<&str>) -> Style {
        match priority {
            Some("high") => self.red,
            Some("medium") => self.yellow,
            Some("low") => self.green,
            _ => self.dim,
        }
    }

    /// An agent's color, picked by a hash of the name so it stays put.
    fn agent(&self, name: &str) -> Style {
        let p = &self.palette;
        let colors = [p.accent, p.mauve, p.yellow, p.peach, p.teal, p.blue];
        let hash = name.bytes().fold(0usize, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(usize::from(byte))
        });
        Style::default().fg(colors[hash % colors.len()])
    }

    fn card_bg(&self) -> Color {
        self.palette.surface0
    }

    /// A card's surface.
    fn card(&self) -> Style {
        self.base.bg(self.card_bg())
    }

    /// A solid pill: dark text on the style's color.
    fn pill(&self, style: Style) -> Style {
        Style::default()
            .fg(self.contrast)
            .bg(style.fg.unwrap_or(self.palette.overlay1))
            .add_modifier(Modifier::BOLD)
    }

    /// A status as a solid pill; To Do gets blue so it reads as a status, not text.
    fn status_pill(&self, status: &str) -> Style {
        match status.to_ascii_lowercase().as_str() {
            "to do" | "todo" | "backlog" => self.pill(Style::default().fg(self.palette.blue)),
            _ => self.pill(self.status(status)),
        }
    }

    /// A task type as a solid tag: bugs red, features blue, the rest by kind.
    fn type_tag(&self, kind: &str) -> Style {
        let p = &self.palette;
        let color = match kind.to_ascii_lowercase().as_str() {
            "bug" => p.red,
            "feature" => p.blue,
            "enhancement" => p.mauve,
            "docs" => p.teal,
            "spike" => p.peach,
            "chore" | "task" => p.overlay1,
            _ => p.overlay0,
        };
        self.pill(Style::default().fg(color))
    }

    /// A filled chip in `color` on a dark version of it.
    fn chip(&self, style: Style) -> Style {
        let fg = style.fg.unwrap_or(self.palette.overlay1);
        let bg = match (fg, self.contrast) {
            (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) => {
                let (r, g, b) = crate::ui::mix_rgb((r, g, b), (br, bg, bb), 0.8);
                Color::Rgb(r, g, b)
            }
            _ => self.palette.surface0,
        };
        Style::default().fg(fg).bg(bg)
    }
}

/// "@developer" → "Developer".
fn agent_name(agent: &str) -> String {
    let name = agent.trim_start_matches('@');
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

pub(super) struct BoardViewFrame {
    pub(super) hits: BoardViewHits,
    pub(super) cursor: Option<crate::protocol::CursorState>,
}

/// Draws the Board over `area`, which it owns entirely.
pub(super) fn render_board_view(
    buffer: &mut Buffer,
    area: Rect,
    view: &ClientBoardView,
    space_label: &str,
    palette: &Palette,
) -> BoardViewFrame {
    let area = area.intersection(buffer.area);
    let styles = Styles::new(palette);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].set_symbol(" ").set_style(Style::reset());
        }
    }
    let mut frame = BoardViewFrame {
        hits: BoardViewHits {
            area,
            ..BoardViewHits::default()
        },
        cursor: None,
    };
    let geometry = BoardGeometry::new(area, view);
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
            "the window is too small for the Board",
            styles.dim,
        );
        return frame;
    }
    match &view.load {
        BoardLoad::Ready => {
            frame.cursor = render_toolbar(buffer, &geometry, view, &styles, &mut frame.hits);
            frame.hits.list = geometry.body;
            let draft_cursor = match view.mode {
                BoardMode::List => render_list(buffer, &geometry, view, &styles, &mut frame.hits),
                BoardMode::Kanban => {
                    render_kanban(buffer, &geometry, view, &styles, &mut frame.hits)
                }
            };
            frame.cursor = frame.cursor.or(draft_cursor);
        }
        BoardLoad::Loading => put_text(
            buffer,
            geometry.toolbar.x,
            geometry.toolbar.y,
            geometry.toolbar.width,
            "loading…",
            styles.dim,
        ),
        BoardLoad::NotInitialized => render_init_box(
            buffer,
            &geometry,
            view,
            space_label,
            &styles,
            &mut frame.hits,
        ),
        BoardLoad::NotInstalled => render_notice(
            buffer,
            &geometry,
            &[
                ("Backlog.md is not installed", styles.bold),
                (
                    "The Board reads tasks through the `backlog` command.",
                    styles.muted,
                ),
                (
                    "Install it with  npm i -g backlog.md  then retry.",
                    styles.muted,
                ),
            ],
            &styles,
            &mut frame.hits,
        ),
        BoardLoad::Unsupported => put_text(
            buffer,
            geometry.toolbar.x,
            geometry.toolbar.y,
            geometry.toolbar.width,
            "this server has no task support yet · update and restart it",
            styles.yellow,
        ),
        BoardLoad::Failed(message) => render_notice(
            buffer,
            &geometry,
            &[
                ("The tasks could not be read", styles.bold),
                (message, styles.red),
            ],
            &styles,
            &mut frame.hits,
        ),
    }
    render_hint(buffer, &geometry, view, &styles);
    if view.load == BoardLoad::Ready {
        let details_cursor = render_details(buffer, &geometry, view, &styles, &mut frame.hits);
        frame.cursor = details_cursor.or(frame.cursor);
        render_held_card(buffer, area, view, &styles);
    }
    frame
}

/// The card under the pointer while it is dragged.
fn render_held_card(buffer: &mut Buffer, area: Rect, view: &ClientBoardView, styles: &Styles) {
    let Some(drag) = view.drag.as_ref().filter(|drag| drag.active) else {
        return;
    };
    let Some(task) = view.task(&drag.id) else {
        return;
    };
    let (x, y) = (drag.at.0.saturating_add(1), drag.at.1);
    if !super::contains(area, (x, y)) {
        return;
    }
    let label = format!(" {} {} ", task.id, fit(&task.title, 26));
    put_text(
        buffer,
        x,
        y,
        area.right().saturating_sub(x),
        &label,
        styles.primary,
    );
}

/// The status a held card is over, when it would move there.
fn drop_target(view: &ClientBoardView) -> Option<&str> {
    let drag = view.drag.as_ref().filter(|drag| drag.active)?;
    let over = drag.over.as_deref()?;
    (view.task(&drag.id).map(|task| task.status.as_str()) != Some(over)).then_some(over)
}

fn held(view: &ClientBoardView, id: &str) -> bool {
    view.drag
        .as_ref()
        .is_some_and(|drag| drag.active && drag.id == id)
}

/// Rows the new task's field takes at the top of its column.
const DRAFT_HEIGHT: u16 = 3;
/// Subtasks listed on a card before "+N more".
const CARD_SUBTASKS: usize = 4;
/// Description rows on a card.
const CARD_DESCRIPTION_ROWS: usize = 2;
/// Columns between kanban columns.
const COLUMN_GAP: u16 = 2;

/// `▲ High`, `◆ Medium`, `▼ Low`.
fn priority_label(priority: &str) -> String {
    let icon = match priority {
        "high" => "▲",
        "medium" => "◆",
        "low" => "▼",
        _ => "•",
    };
    let mut name = priority.chars();
    let name: String = name
        .next()
        .map(|first| first.to_uppercase().chain(name).collect())
        .unwrap_or_default();
    format!(" {icon} {name} ")
}

/// The description rows a card shows for `task` at `width`.
fn card_description(task: &TaskSummary, width: u16) -> Vec<String> {
    let Some(preview) = task.description_preview.as_deref() else {
        return Vec::new();
    };
    let mut rows = wrap(preview, width);
    if rows.len() > CARD_DESCRIPTION_ROWS {
        let rest = rows[CARD_DESCRIPTION_ROWS - 1..].join(" ");
        rows.truncate(CARD_DESCRIPTION_ROWS - 1);
        rows.push(fit(&rest, width).into_owned());
    }
    rows
}

/// Rows a card takes: padding, header, title, description, meta, subtasks.
fn card_height(view: &ClientBoardView, task: &TaskSummary, width: u16) -> u16 {
    let inner = width.saturating_sub(4);
    let subtasks = view.subtasks(&task.id).len();
    let subtask_rows = subtasks.min(CARD_SUBTASKS) + usize::from(subtasks > CARD_SUBTASKS);
    (5 + card_description(task, inner).len() + subtask_rows) as u16
}

fn render_kanban(
    buffer: &mut Buffer,
    geometry: &BoardGeometry,
    view: &ClientBoardView,
    styles: &Styles,
    hits: &mut BoardViewHits,
) -> Option<crate::protocol::CursorState> {
    let area = geometry.body;
    let statuses = view.shown_statuses();
    let count = statuses.len().max(1) as u16;
    let width = area.width.saturating_sub((count - 1) * COLUMN_GAP) / count;
    let target = drop_target(view);
    let mut cursor = None;
    for (index, status) in statuses.iter().enumerate() {
        let x = area.x + index as u16 * (width + COLUMN_GAP);
        // The last column takes what rounding left over.
        let column_width = if index + 1 == statuses.len() {
            area.right().saturating_sub(x)
        } else {
            width
        };
        let rect = Rect::new(x, area.y, column_width, area.height);
        if rect.width < 12 || rect.height < 4 {
            continue;
        }
        let tasks = view.column(status);
        let over = target == Some(*status);
        let x = put_segment(
            buffer,
            rect.x,
            rect.y,
            rect.right().saturating_sub(4),
            &format!("{status} "),
            styles.status(status).add_modifier(Modifier::BOLD),
        );
        put_segment(
            buffer,
            x,
            rect.y,
            rect.right().saturating_sub(4),
            &format!(" {} ", tasks.len()),
            styles.button,
        );
        let plus = Rect::new(rect.right().saturating_sub(3), rect.y, 3, 1);
        put_text(buffer, plus.x, plus.y, 3, " + ", styles.muted);
        hits.targets
            .push((plus, BoardTarget::NewTask((*status).to_owned())));
        let underline = if over {
            styles.accent
        } else {
            styles.status(status).add_modifier(Modifier::DIM)
        };
        put_text(
            buffer,
            rect.x,
            rect.y + 1,
            rect.width,
            &"─".repeat(usize::from(rect.width)),
            underline,
        );
        hits.drop_zones.push((rect, (*status).to_owned()));
        hits.columns.push((rect, (*status).to_owned()));

        let bottom = rect.bottom().saturating_sub(1);
        let mut y = rect.y + 2;
        let drafting = view.draft.as_ref().filter(|draft| draft.status == *status);
        if let Some(draft) = drafting {
            let field = Rect::new(rect.x, y, rect.width, DRAFT_HEIGHT);
            draw_box(buffer, field, styles.accent);
            let text = Rect::new(field.x + 2, y + 1, field.width.saturating_sub(4), 1);
            cursor = super::text_editor::render(buffer, text, &draft.title, styles.base);
            if draft.title.is_empty() {
                put_text(
                    buffer,
                    text.x + 1,
                    text.y,
                    text.width.saturating_sub(1),
                    "title · enter to add",
                    styles.dim,
                );
            }
            hits.targets.push((field, BoardTarget::Draft));
            y += DRAFT_HEIGHT + 1;
        }
        let scroll = view
            .column_scroll
            .get(*status)
            .copied()
            .unwrap_or(0)
            .min(tasks.len().saturating_sub(1));
        let mut shown = 0;
        for task in tasks.iter().skip(scroll) {
            let height = card_height(view, task, rect.width);
            // A card that does not fit whole waits for scrolling.
            if y + height > bottom && shown > 0 {
                break;
            }
            let card = Rect::new(rect.x, y, rect.width, height.min(bottom.saturating_sub(y)));
            render_card(buffer, card, task, view, styles, hits);
            y += height + 1;
            shown += 1;
        }
        if tasks.is_empty() && drafting.is_none() {
            let text = if over { "drop here" } else { "No tasks" };
            put_text(
                buffer,
                rect.x + 1,
                rect.y + 3,
                rect.width.saturating_sub(1),
                text,
                styles.dim,
            );
        }
        let below = tasks.len().saturating_sub(scroll + shown);
        if below > 0 {
            let text = format!(" ↓ {below} more ");
            put_text(
                buffer,
                rect.x,
                bottom,
                display_width(&text),
                &text,
                styles.muted,
            );
        }
        if scroll > 0 {
            let text = format!(" ↑ {scroll} ");
            let width = display_width(&text);
            put_text(
                buffer,
                rect.right().saturating_sub(width),
                bottom,
                width,
                &text,
                styles.muted,
            );
        }
    }
    cursor
}

/// A filled card: type and ID, title, description, priority and counts,
/// then its subtasks with their own status.
fn render_card(
    buffer: &mut Buffer,
    rect: Rect,
    task: &TaskSummary,
    view: &ClientBoardView,
    styles: &Styles,
    hits: &mut BoardViewHits,
) {
    // The card first, so the pills and links on it take their own clicks.
    hits.targets
        .push((rect, BoardTarget::Card(task.id.clone())));
    let selected = view.selected.as_ref() == Some(&task.id) && view.details_open;
    let lifted = held(view, &task.id);
    let card = styles.card();
    for y in rect.y..rect.bottom() {
        put_text(
            buffer,
            rect.x,
            y,
            rect.width,
            &" ".repeat(usize::from(rect.width)),
            card,
        );
        if selected {
            put_text(buffer, rect.x, y, 1, "▌", card.fg(styles.palette.accent));
        }
    }
    let on = |style: Style| style.bg(styles.card_bg());
    let inner = Rect::new(
        rect.x + 2,
        rect.y + 1,
        rect.width.saturating_sub(4),
        rect.height,
    );
    let end = inner.right();
    let mut y = inner.y;
    let line = |y: u16| y < rect.bottom();

    // Type and ID, with the agent on the right.
    if line(y) {
        let mut x = inner.x;
        if let Some(kind) = task.task_type.as_deref() {
            x = put_segment(
                buffer,
                x,
                y,
                end,
                &format!(" {} ", kind.to_uppercase()),
                styles.type_tag(kind),
            );
            x += 1;
        }
        x = put_segment(buffer, x, y, end, &task.id, on(styles.muted));
        if let Some(agent) = task.assignees.first() {
            let name = format!("● {}", agent_name(agent));
            let width = display_width(&name);
            if x + 2 + width <= end {
                put_text(
                    buffer,
                    end - width,
                    y,
                    width,
                    &name,
                    on(styles.agent(agent)),
                );
            }
        }
        y += 1;
    }
    if line(y) {
        let title = if lifted { styles.dim } else { styles.bold };
        put_text(
            buffer,
            inner.x,
            y,
            inner.width,
            &fit(&task.title, inner.width),
            on(title),
        );
        y += 1;
    }
    for row in card_description(task, inner.width) {
        if line(y) {
            put_text(buffer, inner.x, y, inner.width, &row, on(styles.dim));
        }
        y += 1;
    }
    // Priority, progress, pull request.
    if line(y) {
        let mut x = inner.x;
        if let Some(priority) = task.priority.as_deref() {
            x = put_segment(
                buffer,
                x,
                y,
                end,
                &priority_label(priority),
                styles.pill(styles.priority(Some(priority))),
            );
            x += 2;
        }
        let subtasks = view.subtasks(&task.id);
        if !subtasks.is_empty() {
            let done = subtasks
                .iter()
                .filter(|child| view.statuses().last() == Some(&child.status))
                .count();
            x = put_segment(
                buffer,
                x,
                y,
                end,
                &format!("◫ {done}/{}", subtasks.len()),
                on(styles.muted),
            );
            x += 2;
        }
        if task.acceptance_criteria_total > 0 {
            let checks = format!(
                "✓ {}/{}",
                task.acceptance_criteria_checked, task.acceptance_criteria_total
            );
            x = put_segment(buffer, x, y, end, &checks, on(styles.muted));
            x += 2;
        }
        if let Some((url, label)) = super::board_view::pull_request(task) {
            let label = format!("⎇ {label}");
            let start = x;
            x = put_segment(buffer, x, y, end, &label, on(styles.accent));
            if x > start {
                hits.targets.push((
                    Rect::new(start, y, x - start, 1),
                    BoardTarget::OpenUrl(url.to_owned()),
                ));
            }
        }
        if view.is_busy(&task.id) {
            let note = "saving…";
            let width = display_width(note);
            if x + 1 + width <= end {
                put_text(buffer, end - width, y, width, note, on(styles.dim));
            }
        }
        y += 1;
    }
    // Subtasks: a click on the name opens one, on its pill moves it on.
    let subtasks = view.subtasks(&task.id);
    for child in subtasks.iter().take(CARD_SUBTASKS) {
        if !line(y) {
            break;
        }
        // The parent's card already names it: `PAY-101.2` reads as `101.2`.
        let short_id = child
            .id
            .rsplit_once('-')
            .map_or(child.id.as_str(), |(_, rest)| rest);
        let id_width = display_width(short_id);
        let title_x = inner.x + id_width + 2;
        let full = format!(" {} ", child.status);
        // A long status leaves the title too little room: it shrinks to a dot.
        let pill = if end.saturating_sub(title_x + display_width(&full) + 1) >= 10 {
            full
        } else {
            " ● ".to_owned()
        };
        let pill_width = display_width(&pill);
        let pill_x = end.saturating_sub(pill_width);
        put_text(buffer, inner.x, y, id_width, short_id, on(styles.muted));
        put_text(
            buffer,
            title_x,
            y,
            pill_x.saturating_sub(title_x + 1),
            &fit(&child.title, pill_x.saturating_sub(title_x + 1)),
            on(styles.base),
        );
        put_text(
            buffer,
            pill_x,
            y,
            pill_width,
            &pill,
            styles.status_pill(&child.status),
        );
        hits.targets.push((
            Rect::new(inner.x, y, pill_x.saturating_sub(inner.x), 1),
            BoardTarget::Task(child.id.clone()),
        ));
        hits.targets.push((
            Rect::new(pill_x, y, pill_width, 1),
            BoardTarget::NextStatus(child.id.clone()),
        ));
        y += 1;
    }
    if subtasks.len() > CARD_SUBTASKS && line(y) {
        let more = format!("+{} more", subtasks.len() - CARD_SUBTASKS);
        put_text(buffer, inner.x, y, inner.width, &more, on(styles.dim));
    }
}

fn render_header(
    buffer: &mut Buffer,
    geometry: &BoardGeometry,
    view: &ClientBoardView,
    space_label: &str,
    styles: &Styles,
    hits: &mut BoardViewHits,
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
        BoardTarget::Close,
    ));
    let end = close_x.saturating_sub(1);
    let mut x = put_segment(buffer, row.x, row.y, end, "▦ Board", styles.bold);
    x = put_segment(buffer, x, row.y, end, "  ", styles.base);
    x = put_segment(buffer, x, row.y, end, space_label, styles.muted);
    x = put_segment(buffer, x, row.y, end, "  ·  ", styles.dim);
    let ready = view.board.is_some();
    x = put_segment(
        buffer,
        x,
        row.y,
        end,
        "● ",
        if ready { styles.green } else { styles.dim },
    );
    let state = match view.load {
        BoardLoad::NotInitialized => "Backlog.md not set up",
        BoardLoad::NotInstalled => "Backlog.md not installed",
        _ => "Backlog.md",
    };
    x = put_segment(buffer, x, row.y, end, state, styles.muted);
    if let Some(root) = view.board.as_ref().map(|board| tilde(&board.root)) {
        let width = display_width(&root);
        if x + 4 + width <= end {
            put_text(buffer, end - width, row.y, width, &root, styles.dim);
        }
    }
}

/// `/home/me/x` → `~/x`.
pub(super) fn tilde(path: &str) -> String {
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .and_then(|home| path.strip_prefix(&home).map(|rest| format!("~{rest}")))
        .unwrap_or_else(|| path.to_owned())
}

/// The view switch on the left; search and filters fill in from the right
/// and stop before they would run into it.
fn render_toolbar(
    buffer: &mut Buffer,
    geometry: &BoardGeometry,
    view: &ClientBoardView,
    styles: &Styles,
    hits: &mut BoardViewHits,
) -> Option<crate::protocol::CursorState> {
    let row = geometry.toolbar;
    let mut x = row.x;
    for (label, mode) in [
        (" ▦ Kanban ", BoardMode::Kanban),
        (" ≡ List ", BoardMode::List),
    ] {
        let width = display_width(label);
        let style = if view.mode == mode {
            styles.primary
        } else {
            styles.button
        };
        put_text(buffer, x, row.y, width, label, style);
        hits.targets
            .push((Rect::new(x, row.y, width, 1), BoardTarget::Mode(mode)));
        x += width;
    }
    let left_end = x + 1;
    let wide = row.width >= WIDE_TOOLBAR;
    let mut right = row.right();
    if let Some(first) = view.statuses().first() {
        let label = " + New task ";
        let width = display_width(label);
        if right > left_end + width {
            right -= width;
            put_text(buffer, right, row.y, width, label, styles.primary);
            hits.targets.push((
                Rect::new(right, row.y, width, 1),
                BoardTarget::NewTask(first.clone()),
            ));
            right -= 1;
        }
    }
    for (name, filter) in [
        ("status", BoardFilter::Status),
        ("priority", BoardFilter::Priority),
        ("agent", BoardFilter::Agent),
    ] {
        let value = view.filter_label(filter);
        let label = if wide {
            format!(" {name}: {value} ▾ ")
        } else {
            format!(" {value} ▾ ")
        };
        let width = display_width(&label);
        if right < left_end + width + 1 {
            return None;
        }
        right -= width;
        let style = if value == "all" {
            styles.button
        } else {
            styles.button.fg(styles.palette.text)
        };
        put_text(buffer, right, row.y, width, &label, style);
        hits.targets.push((
            Rect::new(right, row.y, width, 1),
            BoardTarget::Filter(filter),
        ));
        right -= 1;
    }
    if right < left_end + SEARCH_WIDTH {
        return None;
    }
    let search = Rect::new(right - SEARCH_WIDTH, row.y, SEARCH_WIDTH, 1);
    let field = styles.button;
    put_text(
        buffer,
        search.x,
        row.y,
        search.width,
        &" ".repeat(usize::from(search.width)),
        field,
    );
    put_text(
        buffer,
        search.x + 1,
        row.y,
        2,
        "⌕ ",
        field.fg(styles.palette.overlay0),
    );
    hits.targets.push((search, BoardTarget::Search));
    let text_area = Rect::new(search.x + 3, row.y, search.width.saturating_sub(4), 1);
    if view.search_focused {
        return super::text_editor::render(
            buffer,
            text_area,
            &view.search,
            field.fg(styles.palette.text),
        );
    }
    let (text, style) = if view.search.is_empty() {
        ("search tasks…  /", field.fg(styles.palette.overlay0))
    } else {
        (view.search.as_str(), field.fg(styles.palette.text))
    };
    put_text(
        buffer,
        text_area.x,
        row.y,
        text_area.width,
        &fit(text, text_area.width),
        style,
    );
    None
}

/// Column starts of the grouped list, and whether it has room for Deps.
/// Where each list column starts. Type, Status and Deps drop out, in that
/// order, when the board is narrow.
struct ListColumns {
    id: u16,
    title: u16,
    title_width: u16,
    kind: Option<u16>,
    priority: u16,
    status: Option<u16>,
    agent: u16,
    deps: Option<u16>,
}

const ID_WIDTH: u16 = 16;
const TYPE_WIDTH: u16 = 13;
const PRIORITY_WIDTH: u16 = 12;
const STATUS_WIDTH: u16 = 15;
const AGENT_WIDTH: u16 = 14;
const DEPS_WIDTH: u16 = 12;
const MIN_TITLE_WIDTH: u16 = 20;

impl ListColumns {
    fn new(area: Rect) -> Self {
        let fixed = ID_WIDTH + PRIORITY_WIDTH + AGENT_WIDTH;
        let room = |extra: u16| area.width >= fixed + extra + MIN_TITLE_WIDTH;
        let show_status = room(STATUS_WIDTH);
        let show_kind = room(STATUS_WIDTH + TYPE_WIDTH);
        let show_deps = room(STATUS_WIDTH + TYPE_WIDTH + DEPS_WIDTH);
        let used = fixed
            + if show_status { STATUS_WIDTH } else { 0 }
            + if show_kind { TYPE_WIDTH } else { 0 }
            + if show_deps { DEPS_WIDTH } else { 0 };
        let title_width = area.width.saturating_sub(used).max(10);
        let title = area.x + ID_WIDTH;
        let mut x = title + title_width;
        let mut next = |show: bool, width: u16| {
            show.then(|| {
                let start = x;
                x += width;
                start
            })
        };
        let kind = next(show_kind, TYPE_WIDTH);
        let priority = next(true, PRIORITY_WIDTH).unwrap_or_default();
        let status = next(show_status, STATUS_WIDTH);
        let agent = next(true, AGENT_WIDTH).unwrap_or_default();
        let deps = next(show_deps, DEPS_WIDTH);
        Self {
            id: area.x + 2,
            title,
            title_width,
            kind,
            priority,
            status,
            agent,
            deps,
        }
    }
}

fn render_list(
    buffer: &mut Buffer,
    geometry: &BoardGeometry,
    view: &ClientBoardView,
    styles: &Styles,
    hits: &mut BoardViewHits,
) -> Option<crate::protocol::CursorState> {
    let area = geometry.body;
    let columns = ListColumns::new(area);
    let mut heads = vec![
        ("ID", columns.id, Some(BoardSortKey::Id)),
        ("Title", columns.title, Some(BoardSortKey::Title)),
    ];
    heads.extend(columns.kind.map(|x| ("Type", x, None)));
    heads.push(("Priority", columns.priority, Some(BoardSortKey::Priority)));
    heads.extend(columns.status.map(|x| ("Status", x, None)));
    heads.push(("Agent", columns.agent, Some(BoardSortKey::Agent)));
    heads.extend(columns.deps.map(|x| ("Deps", x, Some(BoardSortKey::Deps))));
    for (index, (label, x, key)) in heads.iter().enumerate() {
        let next = heads.get(index + 1).map_or(area.right(), |head| head.1);
        let width = next.saturating_sub(*x);
        let Some(key) = key else {
            put_text(buffer, *x, area.y, width, label, styles.dim);
            continue;
        };
        let on = view.sort == *key;
        let text = match (on, view.sort_ascending) {
            (true, true) => format!("{label} ▲"),
            (true, false) => format!("{label} ▼"),
            (false, _) => (*label).to_owned(),
        };
        put_text(
            buffer,
            *x,
            area.y,
            width,
            &text,
            if on { styles.bold } else { styles.dim },
        );
        hits.targets
            .push((Rect::new(*x, area.y, width, 1), BoardTarget::Sort(*key)));
    }
    let rows = view.list_rows();
    let top = area.y + 2;
    let viewport = list_viewport(geometry);
    if rows
        .iter()
        .all(|row| !matches!(row, BoardRow::Task(_) | BoardRow::Subtask { .. }))
        && !view.search.is_empty()
    {
        put_text(
            buffer,
            area.x + 2,
            top,
            area.width,
            "No tasks match",
            styles.dim,
        );
    }
    let target = drop_target(view);
    let mut cursor = None;
    // Each group's rows on screen, where a held row can land.
    let mut zones: Vec<(String, u16, u16)> = Vec::new();
    let mut group = rows
        .iter()
        .take(view.list_scroll)
        .rev()
        .find_map(|row| match row {
            BoardRow::Group { status, .. } => Some((*status).to_owned()),
            _ => None,
        });
    for (offset, row) in rows
        .iter()
        .skip(view.list_scroll)
        .take(usize::from(viewport))
        .enumerate()
    {
        let y = top + offset as u16;
        if let BoardRow::Group { status, .. } = row {
            group = Some((*status).to_owned());
        }
        if let (Some(status), false) = (group.as_ref(), matches!(row, BoardRow::Gap)) {
            match zones.last_mut() {
                Some((last, _, end)) if last == status => *end = y,
                _ => zones.push((status.clone(), y, y)),
            }
        }
        match row {
            BoardRow::Group {
                status,
                count,
                folded,
            } => {
                let plus = Rect::new(area.right().saturating_sub(3), y, 3, 1);
                put_text(buffer, plus.x, y, 3, " + ", styles.dim);
                hits.targets
                    .push((plus, BoardTarget::NewTask((*status).to_owned())));
                let label = format!("{} {status} ", if *folded { "▸" } else { "▾" });
                let x = put_segment(
                    buffer,
                    area.x,
                    y,
                    area.right(),
                    &label,
                    styles.status(status).add_modifier(Modifier::BOLD),
                );
                let end = put_segment(buffer, x, y, area.right(), &count.to_string(), styles.dim);
                hits.targets.push((
                    Rect::new(area.x, y, end - area.x, 1),
                    BoardTarget::Group((*status).to_owned()),
                ));
            }
            BoardRow::Empty => {
                let over = group
                    .as_deref()
                    .is_some_and(|status| target == Some(status));
                let text = if over { "drop here" } else { "No tasks" };
                put_text(buffer, area.x + 2, y, area.width, text, styles.dim);
            }
            BoardRow::Draft => {
                if let Some(draft) = view.draft.as_ref() {
                    let field = Rect::new(columns.title, y, columns.title_width, 1);
                    put_text(buffer, columns.id, y, 6, "new", styles.accent);
                    cursor = super::text_editor::render(
                        buffer,
                        field,
                        &draft.title,
                        styles.button.fg(styles.palette.text),
                    );
                    if draft.title.is_empty() {
                        put_text(
                            buffer,
                            field.x + 1,
                            y,
                            field.width.saturating_sub(1),
                            "title · enter to add · esc to cancel",
                            styles.button,
                        );
                    }
                    hits.targets
                        .push((Rect::new(area.x, y, area.width, 1), BoardTarget::Draft));
                }
            }
            BoardRow::Gap => {}
            BoardRow::Task(task) => {
                hits.targets.push((
                    Rect::new(area.x, y, area.width, 1),
                    BoardTarget::Card(task.id.clone()),
                ));
                let children = view.subtasks(&task.id).len();
                let tree = (children > 0).then(|| {
                    if view.folded_tasks.contains(&task.id) {
                        "▸ "
                    } else {
                        "▾ "
                    }
                });
                if tree.is_some() {
                    hits.targets.push((
                        Rect::new(columns.id.saturating_sub(1), y, 3, 1),
                        BoardTarget::FoldTask(task.id.clone()),
                    ));
                }
                let row = ListRow {
                    task,
                    tree: tree.unwrap_or("  "),
                    indent: 0,
                    children,
                };
                render_list_row(buffer, area, &columns, y, row, view, styles);
            }
            BoardRow::Subtask { task, last } => {
                hits.targets.push((
                    Rect::new(area.x, y, area.width, 1),
                    BoardTarget::Card(task.id.clone()),
                ));
                let row = ListRow {
                    task,
                    tree: if *last { "  └─ " } else { "  ├─ " },
                    indent: 2,
                    children: 0,
                };
                render_list_row(buffer, area, &columns, y, row, view, styles);
            }
        }
    }
    for (status, start, end) in zones {
        let rect = Rect::new(
            area.x.saturating_sub(1),
            start,
            area.width + 1,
            end - start + 1,
        );
        if target == Some(status.as_str()) {
            for y in start..=end {
                put_text(buffer, rect.x, y, 1, "▌", styles.accent);
            }
        }
        hits.drop_zones.push((rect, status));
    }
    cursor
}

/// One task's line in the list, with its place in the tree.
struct ListRow<'a> {
    task: &'a TaskSummary,
    /// `▾ `/`▸ ` for a parent, `├─ `/`└─ ` for a subtask.
    tree: &'static str,
    /// Columns the title is pushed in.
    indent: u16,
    /// Subtasks, shown as `(n)` after a parent's title.
    children: usize,
}

fn render_list_row(
    buffer: &mut Buffer,
    area: Rect,
    columns: &ListColumns,
    y: u16,
    row: ListRow<'_>,
    view: &ClientBoardView,
    styles: &Styles,
) {
    let task = row.task;
    let selected = view.selected.as_ref() == Some(&task.id) && view.details_open;
    if selected {
        put_text(
            buffer,
            area.x,
            y,
            area.width,
            &" ".repeat(usize::from(area.width)),
            styles.selected_row,
        );
        put_text(
            buffer,
            area.x,
            y,
            1,
            "▌",
            styles.accent.bg(styles.palette.surface0),
        );
    }
    let bg = |style: Style| {
        if selected {
            style.bg(styles.palette.surface0)
        } else {
            style
        }
    };
    let id_width = columns.title - columns.id - 1;
    let x = put_segment(
        buffer,
        columns.id.saturating_sub(1),
        y,
        columns.title - 1,
        row.tree,
        bg(styles.dim),
    );
    put_text(
        buffer,
        x,
        y,
        (columns.title - 1).saturating_sub(x),
        &fit(&task.id, id_width.saturating_sub(x - columns.id)),
        bg(styles.muted),
    );
    let title = if row.children > 0 {
        format!("{} ({})", task.title, row.children)
    } else {
        task.title.clone()
    };
    let title_width = columns.title_width.saturating_sub(row.indent + 1);
    put_text(
        buffer,
        columns.title + row.indent,
        y,
        title_width,
        &fit(&title, title_width),
        bg(if held(view, &task.id) {
            styles.dim
        } else if row.children > 0 {
            styles.bold
        } else {
            styles.base
        }),
    );
    if let Some(x) = columns.kind {
        match task.task_type.as_deref() {
            Some(kind) => {
                let label = format!(" {} ", capitalized(kind));
                put_text(
                    buffer,
                    x,
                    y,
                    TYPE_WIDTH - 1,
                    &fit(&label, TYPE_WIDTH - 1),
                    styles.type_tag(kind),
                );
            }
            None => put_text(buffer, x, y, 1, "—", bg(styles.dim)),
        }
    }
    match task.priority.as_deref() {
        Some(priority) => {
            let label = priority_label(priority);
            put_text(
                buffer,
                columns.priority,
                y,
                PRIORITY_WIDTH - 1,
                &fit(&label, PRIORITY_WIDTH - 1),
                styles.pill(styles.priority(Some(priority))),
            );
        }
        None => put_text(buffer, columns.priority, y, 1, "—", bg(styles.dim)),
    }
    if let Some(x) = columns.status {
        let label = format!(" {} ", task.status);
        put_text(
            buffer,
            x,
            y,
            STATUS_WIDTH - 1,
            &fit(&label, STATUS_WIDTH - 1),
            styles.status_pill(&task.status),
        );
    }
    match task.assignees.first() {
        Some(agent) => {
            let text = format!("● {}", agent_name(agent));
            put_text(
                buffer,
                columns.agent,
                y,
                AGENT_WIDTH - 1,
                &fit(&text, AGENT_WIDTH - 1),
                bg(styles.agent(agent)),
            );
        }
        None => put_text(buffer, columns.agent, y, 1, "—", bg(styles.dim)),
    }
    if let Some(deps) = columns.deps {
        let width = area.right().saturating_sub(deps);
        let text = if task.dependencies.is_empty() {
            "—".to_owned()
        } else {
            task.dependencies.join(", ")
        };
        put_text(buffer, deps, y, width, &fit(&text, width), bg(styles.dim));
    }
}

/// "feature" → "Feature".
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn render_init_box(
    buffer: &mut Buffer,
    geometry: &BoardGeometry,
    view: &ClientBoardView,
    space_label: &str,
    styles: &Styles,
    hits: &mut BoardViewHits,
) {
    let area = geometry.body;
    let width = area.width.min(64);
    let height = 10.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 3;
    draw_box(buffer, Rect::new(x, y, width, height), styles.rule);
    let inner = width.saturating_sub(6);
    put_text(
        buffer,
        x + 3,
        y + 2,
        inner,
        "Backlog.md is not set up in this space",
        styles.bold,
    );
    put_text(buffer, x + 3, y + 3, inner, space_label, styles.dim);
    put_text(
        buffer,
        x + 3,
        y + 5,
        inner,
        "Tasks live in the project's own backlog/ folder.",
        styles.muted,
    );
    let label = if view.init_busy {
        " setting up… "
    } else {
        " Initialize Backlog.md "
    };
    let button = Rect::new(x + 3, y + 7, display_width(label).min(inner), 1);
    put_text(
        buffer,
        button.x,
        button.y,
        button.width,
        label,
        styles.primary,
    );
    hits.targets.push((button, BoardTarget::Init));
    let note_x = button.right() + 2;
    put_text(
        buffer,
        note_x,
        button.y,
        (x + width).saturating_sub(note_x + 2),
        "adds To Do · In Progress · Review · Done",
        styles.dim,
    );
    if let Some(message) = view.message.as_deref() {
        put_text(
            buffer,
            x + 3,
            y + 8,
            inner,
            &fit(message, inner),
            styles.red,
        );
    }
}

/// A boxed message with a Retry button.
fn render_notice(
    buffer: &mut Buffer,
    geometry: &BoardGeometry,
    lines: &[(&str, Style)],
    styles: &Styles,
    hits: &mut BoardViewHits,
) {
    let area = geometry.body;
    let width = area.width.min(64);
    let inner = width.saturating_sub(6);
    let wrapped: Vec<(String, Style)> = lines
        .iter()
        .flat_map(|(text, style)| wrap(text, inner).into_iter().map(move |row| (row, *style)))
        .collect();
    let height = (wrapped.len() as u16 + 6).min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 3;
    draw_box(buffer, Rect::new(x, y, width, height), styles.rule);
    for (index, (text, style)) in wrapped.iter().enumerate() {
        put_text(buffer, x + 3, y + 2 + index as u16, inner, text, *style);
    }
    let label = " Retry ";
    let button = Rect::new(x + 3, y + height.saturating_sub(2), display_width(label), 1);
    put_text(
        buffer,
        button.x,
        button.y,
        button.width,
        label,
        styles.primary,
    );
    hits.targets.push((button, BoardTarget::Retry));
}

pub(super) fn draw_box(buffer: &mut Buffer, rect: Rect, style: Style) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let inner = usize::from(rect.width - 2);
    put_text(
        buffer,
        rect.x,
        rect.y,
        rect.width,
        &format!("╭{}╮", "─".repeat(inner)),
        style,
    );
    for y in rect.y + 1..rect.bottom() - 1 {
        put_text(buffer, rect.x, y, 1, "│", style);
        put_text(buffer, rect.right() - 1, y, 1, "│", style);
    }
    put_text(
        buffer,
        rect.x,
        rect.bottom() - 1,
        rect.width,
        &format!("╰{}╯", "─".repeat(inner)),
        style,
    );
}

fn render_hint(
    buffer: &mut Buffer,
    geometry: &BoardGeometry,
    view: &ClientBoardView,
    styles: &Styles,
) {
    let row = geometry.hint;
    let (text, style) = match view.message.as_deref() {
        Some(message) if view.load == BoardLoad::Ready => (message, styles.red),
        _ => (
            "click a task to open · drag to move · / search · n new · r reload · esc close",
            styles.dim,
        ),
    };
    put_text(
        buffer,
        row.x,
        row.y,
        row.width,
        &fit(text, row.width),
        style,
    );
}

/// One row of the details panel and what clicking it does.
struct DetailLine {
    spans: Vec<(String, Style)>,
    target: Option<BoardTarget>,
    /// The field being typed into follows the spans on this line.
    input: bool,
}

impl DetailLine {
    fn new(spans: Vec<(String, Style)>) -> Self {
        Self {
            spans,
            target: None,
            input: false,
        }
    }

    fn on_click(mut self, target: BoardTarget) -> Self {
        self.target = Some(target);
        self
    }

    /// `prefix` then the field being typed into.
    fn input(prefix: Vec<(String, Style)>) -> Self {
        Self {
            spans: prefix,
            target: Some(BoardTarget::Editing),
            input: true,
        }
    }

    fn text(text: impl Into<String>, style: Style) -> Self {
        Self::new(vec![(text.into(), style)])
    }

    fn blank() -> Self {
        Self::new(Vec::new())
    }
}

fn detail_lines(view: &ClientBoardView, width: u16, styles: &Styles) -> Vec<DetailLine> {
    let Some(task) = view.selected_task() else {
        return Vec::new();
    };
    let detail = view.shown_detail();
    let editing = view
        .editing
        .as_ref()
        .filter(|editing| editing.id == task.id)
        .map(|editing| editing.field);
    let mut lines = vec![DetailLine::new(vec![
        (task.id.clone(), styles.muted),
        (
            if view.is_busy(&task.id) {
                "  saving…".to_owned()
            } else {
                String::new()
            },
            styles.dim,
        ),
    ])];
    if editing == Some(BoardField::Title) {
        lines.push(DetailLine::input(Vec::new()));
    } else {
        for row in wrap(&task.title, width.saturating_sub(2)) {
            lines.push(
                DetailLine::text(row, styles.bold).on_click(BoardTarget::Field(BoardField::Title)),
            );
        }
    }
    let mut chips = Vec::new();
    if let Some(kind) = task.task_type.as_deref() {
        chips.push((format!(" {kind} "), styles.button));
        chips.push((" ".to_owned(), styles.base));
    }
    for label in &task.labels {
        chips.push((format!(" {label} "), styles.chip(styles.agent(label))));
        chips.push((" ".to_owned(), styles.base));
    }
    if let Some(priority) = task.priority.as_deref() {
        chips.push((
            format!(" {priority} "),
            styles.chip(styles.priority(Some(priority))),
        ));
    }
    if !chips.is_empty() {
        lines.push(DetailLine::new(chips));
    }
    lines.push(DetailLine::blank());
    let label = |label: &str| (format!("{label:<10}"), styles.muted);
    let field = |name: &str, value: String, style: Style, target: BoardTarget| {
        DetailLine::new(vec![
            label(name),
            (format!(" {value} ▾ "), styles.chip(style)),
        ])
        .on_click(target)
    };
    lines.push(field(
        "Status",
        task.status.clone(),
        styles.status(&task.status),
        BoardTarget::CycleStatus,
    ));
    lines.push(if editing == Some(BoardField::Assignee) {
        DetailLine::input(vec![label("Assignee"), (" ".to_owned(), styles.base)])
    } else {
        match task.assignees.first() {
            Some(agent) => field(
                "Assignee",
                format!(
                    "● {}",
                    task.assignees
                        .iter()
                        .map(|a| agent_name(a))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                styles.agent(agent),
                BoardTarget::Field(BoardField::Assignee),
            ),
            None => field(
                "Assignee",
                "○ Unassigned".into(),
                styles.muted,
                BoardTarget::Field(BoardField::Assignee),
            ),
        }
    });
    lines.push(field(
        "Priority",
        task.priority.clone().unwrap_or_else(|| "none".into()),
        styles.priority(task.priority.as_deref()),
        BoardTarget::CyclePriority,
    ));
    lines.push(if editing == Some(BoardField::Type) {
        DetailLine::input(vec![label("Type"), (" ".to_owned(), styles.base)])
    } else {
        field(
            "Type",
            task.task_type.clone().unwrap_or_else(|| "none".into()),
            styles.muted,
            BoardTarget::Field(BoardField::Type),
        )
    });
    lines.push(DetailLine::blank());
    lines.push(DetailLine::text(
        "─".repeat(usize::from(width)),
        styles.rule,
    ));
    let Some(detail) = detail else {
        lines.push(DetailLine::text("loading…", styles.dim));
        return lines;
    };
    lines.push(DetailLine::text("Description", styles.bold));
    let edit_description = BoardTarget::Field(BoardField::Description);
    if editing == Some(BoardField::Description) {
        lines.push(DetailLine::input(Vec::new()));
    } else {
        match detail
            .description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            Some(text) => {
                lines.extend(wrap(text, width).into_iter().map(|row| {
                    DetailLine::text(row, styles.muted).on_click(edit_description.clone())
                }))
            }
            None => lines.push(
                DetailLine::text("No description · click to add", styles.dim)
                    .on_click(edit_description),
            ),
        }
    }
    lines.push(DetailLine::blank());
    let criteria = &detail.acceptance_criteria;
    let done = criteria.iter().filter(|item| item.checked).count();
    lines.push(DetailLine::new(vec![
        ("Acceptance criteria  ".into(), styles.bold),
        (
            if criteria.is_empty() {
                String::new()
            } else {
                format!("{done}/{}", criteria.len())
            },
            styles.dim,
        ),
    ]));
    if criteria.is_empty() {
        lines.push(DetailLine::text("none", styles.dim));
    }
    for item in criteria {
        let (mark, mark_style, text_style) = if item.checked {
            ("[x] ", styles.green, styles.dim)
        } else {
            ("[ ] ", styles.muted, styles.base)
        };
        for (index, row) in wrap(&item.text, width.saturating_sub(4))
            .into_iter()
            .enumerate()
        {
            let prefix = if index == 0 { mark } else { "    " };
            lines.push(
                DetailLine::new(vec![(prefix.to_owned(), mark_style), (row, text_style)])
                    .on_click(BoardTarget::ToggleAc(item.index)),
            );
        }
    }
    lines.push(DetailLine::blank());
    lines.push(DetailLine::text("Dependencies", styles.bold));
    if task.dependencies.is_empty() {
        lines.push(DetailLine::text("No dependencies", styles.dim));
    }
    for dep in &task.dependencies {
        let other = view.task(dep);
        let status = other.map_or_else(|| "missing".to_owned(), |task| task.status.clone());
        let title_width = width.saturating_sub(display_width(dep) + display_width(&status) + 4);
        let mut line = DetailLine::new(vec![
            (format!("{dep}  "), styles.accent),
            (
                format!(
                    "{:<1$}",
                    fit(other.map_or("", |task| task.title.as_str()), title_width),
                    usize::from(title_width)
                ),
                styles.muted,
            ),
            (
                format!("  {status}"),
                if other.is_some() {
                    styles.status(&status)
                } else {
                    styles.red
                },
            ),
        ]);
        line.target = other.map(|task| BoardTarget::Task(task.id.clone()));
        lines.push(line);
    }
    let subtasks = view.subtasks(&task.id);
    if !subtasks.is_empty() {
        let done = subtasks
            .iter()
            .filter(|child| view.statuses().last() == Some(&child.status))
            .count();
        lines.push(DetailLine::blank());
        lines.push(DetailLine::new(vec![
            ("Subtasks  ".into(), styles.bold),
            (format!("{done}/{}", subtasks.len()), styles.dim),
        ]));
        for child in subtasks {
            let pill = format!(" {} ", child.status);
            let title_width =
                width.saturating_sub(display_width(&child.id) + display_width(&pill) + 3);
            lines.push(
                DetailLine::new(vec![
                    (format!("{}  ", child.id), styles.accent),
                    (
                        format!(
                            "{:<1$} ",
                            fit(&child.title, title_width),
                            usize::from(title_width)
                        ),
                        styles.base,
                    ),
                    (pill, styles.status_pill(&child.status)),
                ])
                .on_click(BoardTarget::Task(child.id.clone())),
            );
        }
    }
    if !task.references.is_empty() {
        lines.push(DetailLine::blank());
        lines.push(DetailLine::text("Links", styles.bold));
        for reference in &task.references {
            let web = reference.starts_with("https://") || reference.starts_with("http://");
            let line = DetailLine::text(
                fit(reference, width).into_owned(),
                if web { styles.accent } else { styles.muted },
            );
            lines.push(if web {
                line.on_click(BoardTarget::OpenUrl(reference.clone()))
            } else {
                line
            });
        }
    }
    lines.push(DetailLine::blank());
    lines.push(DetailLine::new(vec![
        ("Comments  ".into(), styles.bold),
        (detail.comments.len().to_string(), styles.dim),
    ]));
    for comment in &detail.comments {
        let author = comment.author.as_deref().unwrap_or("someone");
        lines.push(DetailLine::text(
            author,
            styles.agent(author).add_modifier(Modifier::BOLD),
        ));
        for row in wrap(&comment.body, width.saturating_sub(2)) {
            lines.push(DetailLine::text(format!("  {row}"), styles.muted));
        }
    }
    lines.push(if editing == Some(BoardField::Comment) {
        DetailLine::input(vec![("› ".to_owned(), styles.accent)])
    } else {
        DetailLine::text(
            format!("{:<1$}", "› Add a comment…", usize::from(width)),
            styles.button.fg(styles.palette.overlay0),
        )
        .on_click(BoardTarget::Field(BoardField::Comment))
    });
    lines
}

fn render_details(
    buffer: &mut Buffer,
    geometry: &BoardGeometry,
    view: &ClientBoardView,
    styles: &Styles,
    hits: &mut BoardViewHits,
) -> Option<crate::protocol::CursorState> {
    let text = geometry.details_text()?;
    let mut cursor = None;
    let frame = geometry.docked.or(geometry.popup).unwrap_or(text);
    if let Some(dock) = geometry.docked {
        for y in dock.y..dock.bottom() {
            put_text(buffer, dock.x - 1, y, 1, "│", styles.rule);
        }
    }
    if let Some(popup) = geometry.popup {
        // Nothing under the popup takes clicks; a click outside it closes it.
        hits.targets.clear();
        hits.targets.push((hits.area, BoardTarget::CloseDetails));
        for y in popup.y..popup.bottom() {
            put_text(
                buffer,
                popup.x,
                y,
                popup.width,
                &" ".repeat(usize::from(popup.width)),
                styles.panel,
            );
        }
        draw_box(buffer, popup, styles.accent.bg(styles.palette.panel_bg));
        let title = view
            .selected
            .as_deref()
            .map(|id| format!(" task {id} "))
            .unwrap_or_default();
        put_text(
            buffer,
            popup.x + 2,
            popup.y,
            display_width(&title),
            &title,
            styles
                .accent
                .bg(styles.palette.panel_bg)
                .add_modifier(Modifier::BOLD),
        );
        let hint = " esc close ";
        put_text(
            buffer,
            popup.right().saturating_sub(display_width(hint) + 2),
            popup.bottom() - 1,
            display_width(hint),
            hint,
            styles.dim.bg(styles.palette.panel_bg),
        );
        hits.targets.push((popup, BoardTarget::Nothing));
    }
    hits.details = frame;
    let lines = detail_lines(view, text.width, styles);
    let on_panel = geometry.popup.is_some();
    for (offset, line) in lines
        .iter()
        .skip(view.details_scroll)
        .take(usize::from(text.height))
        .enumerate()
    {
        let y = text.y + offset as u16;
        let mut x = text.x;
        for (span, style) in &line.spans {
            let style = if on_panel && style.bg.is_none() {
                style.bg(styles.palette.panel_bg)
            } else {
                *style
            };
            x = put_segment(buffer, x, y, text.right(), span, style);
        }
        if let (true, Some(editing)) = (line.input, view.editing.as_ref()) {
            let field = Rect::new(x, y, text.right().saturating_sub(x), 1);
            cursor = super::text_editor::render(
                buffer,
                field,
                &editing.text,
                styles.button.fg(styles.palette.text),
            );
        }
        if let Some(target) = &line.target {
            hits.targets
                .push((Rect::new(text.x, y, text.width, 1), target.clone()));
        }
    }
    if view.details_scroll == 0 {
        let close = Rect::new(text.right().saturating_sub(2), text.y, 3, 1);
        put_text(
            buffer,
            text.right().saturating_sub(1),
            text.y,
            1,
            "×",
            styles.muted,
        );
        hits.targets.push((close, BoardTarget::CloseDetails));
    }
    if lines.len() > view.details_scroll + usize::from(text.height) {
        put_text(
            buffer,
            text.right().saturating_sub(1),
            text.bottom() - 1,
            1,
            "↓",
            styles.dim,
        );
    }
    cursor
}
