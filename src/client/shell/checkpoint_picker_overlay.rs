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
    // title, divider, list, divider, changed files, status
    let height = 2 + list_height + 2 + u16::from(status.is_some()) + 2;
    let popup = popup(b.area, 84, height)?;
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
    let local_now = crate::platform::local_datetime();
    // Wall-clock time of a checkpoint, from the local time now and its age.
    let clock = |created_at: u64| {
        let ago = time::Duration::seconds(now.saturating_sub(created_at) as i64);
        local_now
            .and_then(|local| local.checked_sub(ago))
            .map_or_else(String::new, |at| {
                format!("{:02}:{:02}", at.hour(), at.minute())
            })
    };
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
        let age = super::super::space_picker::closed_age(entry.created_at, now);
        let files = match entry.changed_files.len() {
            1 => "1 file".to_owned(),
            count => format!("{count} files"),
        };
        let right = match &entry.agent {
            Some(agent) => format!("{agent} · {files}  {age:>3} "),
            None => format!("{files}  {age:>3} "),
        };
        let right_width = display_width(&right);
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width.saturating_sub(right_width + 1),
            &format!(" {}  {}", clock(entry.created_at), entry.label),
            style,
        );
        put_right_text(b, rect, rect.y, &right, style);
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
    // The highlighted checkpoint's changed files.
    let changed_y = list.bottom();
    put_text(
        b,
        inner.x,
        changed_y,
        inner.width,
        &"─".repeat(inner.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );
    if let Some(entry) = picker.entries.get(picker.selected) {
        let shown = 6;
        let names: Vec<&str> = entry
            .changed_files
            .iter()
            .take(shown)
            .map(|path| path.rsplit('/').next().unwrap_or(path))
            .collect();
        let more = entry.changed_files.len().saturating_sub(shown);
        let text = if names.is_empty() {
            " no file changes after this point".to_owned()
        } else if more > 0 {
            format!(" changed: {}, +{more}", names.join(", "))
        } else {
            format!(" changed: {}", names.join(", "))
        };
        put_text(b, inner.x, changed_y + 1, inner.width, &text, dim);
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
