use super::*;

/// Most checkpoints shown at once; the list scrolls to the selection.
const VISIBLE_CHECKPOINTS: usize = 12;

pub(super) fn render_checkpoint_picker_overlay(
    b: &mut Buffer,
    picker: &ClientCheckpointPickerOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let list_height = picker.entries.len().clamp(1, VISIBLE_CHECKPOINTS) as u16;
    let status = if picker.restoring {
        Some((" rewinding…".to_owned(), p.accent))
    } else if let Some(error) = &picker.error {
        Some((format!(" {error}"), p.red))
    } else {
        picker
            .notice
            .as_ref()
            .map(|notice| (format!(" {notice}"), p.green))
    };
    // title, divider, list, status
    let height = 2 + list_height + u16::from(status.is_some()) + 2;
    let popup = popup(b.area, 72, height)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    let base = Style::default().fg(p.text).bg(p.panel_bg);
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    let folder = picker
        .folder
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    let title = if folder.is_empty() {
        " rewind".to_owned()
    } else {
        format!(" rewind · {folder}")
    };
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        &title,
        base.add_modifier(Modifier::BOLD),
    );
    put_right_text(b, inner, inner.y, "↵ rewind  esc close ", dim);
    put_text(
        b,
        inner.x,
        inner.y + 1,
        inner.width,
        &"─".repeat(inner.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );

    let list = Rect::new(inner.x, inner.y + 2, inner.width, list_height);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let start = picker
        .selected
        .min(picker.entries.len().saturating_sub(1))
        .saturating_sub(VISIBLE_CHECKPOINTS - 1);
    let mut row_hits = Vec::new();
    for (offset, (position, entry)) in picker
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .take(VISIBLE_CHECKPOINTS)
        .enumerate()
    {
        let rect = Rect::new(list.x, list.y + offset as u16, list.width, 1);
        let style = if position == picker.selected {
            Style::default().fg(contrast(p)).bg(p.accent)
        } else {
            base
        };
        b.set_style(rect, style);
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {}", entry.label),
            style,
        );
        let age = super::super::space_picker::closed_age(entry.created_at, now);
        put_right_text(b, rect, rect.y, &format!("{age} "), style);
        row_hits.push((rect, position));
    }
    if picker.entries.is_empty() {
        let message = if picker.loading {
            " loading…"
        } else if picker.unsupported {
            " restart the herdr server to use checkpoints"
        } else if picker.error.is_some() {
            ""
        } else {
            " no checkpoints yet · saved before each prompt when experimental.checkpoints is on"
        };
        put_text(b, list.x, list.y, list.width, message, dim);
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
        worktree_rows: row_hits,
        ..OverlayRender::default()
    })
}
