use super::*;

/// Most closed spaces shown at once; the list scrolls to the selection.
const VISIBLE_CLOSED: usize = 10;

pub(super) fn render_space_picker_overlay(
    b: &mut Buffer,
    picker: &ClientSpacePickerOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let rows = picker.rows();
    let closed_rows = rows
        .iter()
        .take_while(|row| matches!(row, ClientSpacePickerRow::Closed(_)))
        .count();
    let list_height = closed_rows.clamp(1, VISIBLE_CLOSED) as u16;
    let status = if picker.opening {
        Some((" opening…".to_owned(), p.accent))
    } else {
        picker
            .error
            .as_ref()
            .map(|error| (format!(" {error}"), p.red))
    };
    // title, search, divider, list, divider, two actions, status
    let height = 3 + list_height + 3 + u16::from(status.is_some()) + 2;
    let popup = popup(b.area, 72, height)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    let base = Style::default().fg(p.text).bg(p.panel_bg);
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        " spaces",
        base.add_modifier(Modifier::BOLD),
    );
    put_right_text(b, inner, inner.y, "↵ open  esc close ", dim);

    let search = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    put_text(b, search.x, search.y, 3, " / ", dim);
    let cursor = text_editor::render(
        b,
        Rect::new(search.x + 3, search.y, search.width.saturating_sub(4), 1),
        &picker.query,
        base,
    );
    if picker.query.is_empty() {
        put_text(
            b,
            search.x + 3,
            search.y,
            search.width.saturating_sub(4),
            "search closed spaces",
            dim,
        );
    }
    let divider = "─".repeat(inner.width as usize);
    let divider_style = Style::default().fg(p.surface1).bg(p.panel_bg);
    put_text(
        b,
        inner.x,
        inner.y + 2,
        inner.width,
        &divider,
        divider_style,
    );

    let list = Rect::new(inner.x, inner.y + 3, inner.width, list_height);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let home = std::env::var("HOME").ok();
    let start = picker
        .selected
        .min(closed_rows.saturating_sub(1))
        .saturating_sub(VISIBLE_CLOSED - 1);
    let mut row_hits = Vec::new();
    let row_style = |position: usize| {
        if position == picker.selected {
            Style::default().fg(contrast(p)).bg(p.accent)
        } else {
            base
        }
    };
    for (offset, position) in (start..closed_rows).take(VISIBLE_CLOSED).enumerate() {
        let ClientSpacePickerRow::Closed(index) = rows[position] else {
            continue;
        };
        let Some(entry) = picker.entries.get(index) else {
            continue;
        };
        let rect = Rect::new(list.x, list.y + offset as u16, list.width, 1);
        let style = row_style(position);
        b.set_style(rect, style);
        let age = super::super::space_picker::closed_age(entry.closed_at, now);
        let label = format!(" {}  ", entry.label);
        let label_width = display_width(&label).min(rect.width);
        put_text(
            b,
            rect.x,
            rect.y,
            label_width,
            &label,
            style.add_modifier(Modifier::BOLD),
        );
        let cwd = match home.as_deref() {
            Some(home) if entry.cwd.starts_with(home) => format!("~{}", &entry.cwd[home.len()..]),
            _ => entry.cwd.clone(),
        };
        let age_width = display_width(&age) + 2;
        put_text(
            b,
            rect.x + label_width,
            rect.y,
            rect.width.saturating_sub(label_width + age_width),
            &cwd,
            if position == picker.selected {
                style
            } else {
                dim
            },
        );
        put_right_text(b, rect, rect.y, &format!("{age} "), style);
        row_hits.push((rect, position));
    }
    if closed_rows == 0 {
        let message = if picker.loading {
            " loading…"
        } else if picker.unsupported {
            " restart the herdr server to see closed spaces"
        } else if picker.entries.is_empty() {
            " no closed spaces yet"
        } else {
            " no matching spaces"
        };
        put_text(b, list.x, list.y, list.width, message, dim);
    }

    let actions_y = list.bottom();
    put_text(b, inner.x, actions_y, inner.width, &divider, divider_style);
    for (offset, position) in (closed_rows..rows.len()).enumerate() {
        let rect = Rect::new(inner.x, actions_y + 1 + offset as u16, inner.width, 1);
        let style = row_style(position);
        b.set_style(rect, style);
        let label = match rows[position] {
            ClientSpacePickerRow::NewSpace => " new space",
            ClientSpacePickerRow::OpenFolder => " open folder…",
            ClientSpacePickerRow::Closed(_) => continue,
        };
        put_text(b, rect.x, rect.y, rect.width, label, style);
        row_hits.push((rect, position));
    }
    if let Some((text, color)) = status {
        put_text(
            b,
            inner.x,
            inner.bottom().saturating_sub(1),
            inner.width,
            &text,
            Style::default().fg(color).bg(p.panel_bg),
        );
    }
    Some(OverlayRender {
        area: popup,
        worktree_search: search,
        worktree_rows: row_hits,
        cursor: cursor.filter(|_| !picker.opening),
        ..OverlayRender::default()
    })
}
