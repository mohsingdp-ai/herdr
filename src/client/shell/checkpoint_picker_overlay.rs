use super::*;

/// Most rows shown at once; the list scrolls to the selection.
const VISIBLE_CHECKPOINTS: usize = 12;

pub(super) fn render_checkpoint_picker_overlay(
    b: &mut Buffer,
    picker: &ClientCheckpointPickerOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let rows = picker.rows();
    let list_height = rows.len().clamp(1, VISIBLE_CHECKPOINTS) as u16;
    let status = if picker.restoring {
        Some((" going back…".to_owned(), p.accent))
    } else if let Some(error) = &picker.error {
        Some((format!(" {error}"), p.red))
    } else {
        picker
            .notice
            .as_ref()
            .map(|notice| (format!(" {notice}"), p.green))
    };
    // title, divider, list, divider, two lines on what Enter does, status
    let height = 2 + list_height + 3 + u16::from(status.is_some()) + 2;
    let popup = popup(b.area, 88, height)?;
    let inner = panel(b, popup, p.accent, p.panel_bg)?;
    let base = Style::default().fg(p.text).bg(p.panel_bg);
    let dim = Style::default().fg(p.overlay0).bg(p.panel_bg);
    let green = Style::default().fg(p.green).bg(p.panel_bg);
    let divider = "─".repeat(inner.width as usize);
    let divider_style = Style::default().fg(p.surface1).bg(p.panel_bg);

    let folder = picker
        .folder
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    let title = if folder.is_empty() {
        " rewind files".to_owned()
    } else {
        format!(" rewind files · {folder}")
    };
    put_text(
        b,
        inner.x,
        inner.y,
        inner.width,
        &title,
        base.add_modifier(Modifier::BOLD),
    );
    // "changes | all prompts (a)": click or press a to switch.
    let tabs_x = inner.x + display_width(&title) + 3;
    let changes_style = if picker.show_all {
        dim
    } else {
        base.fg(p.accent).add_modifier(Modifier::BOLD)
    };
    let all_style = if picker.show_all {
        base.fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        dim
    };
    let x = put_segment(b, tabs_x, inner.y, inner.right(), "changes", changes_style);
    let x = put_segment(b, x, inner.y, inner.right(), "  ", dim);
    put_segment(b, x, inner.y, inner.right(), "all prompts (a)", all_style);
    let tabs = Rect::new(tabs_x, inner.y, x.saturating_sub(tabs_x) + 15, 1);
    put_right_text(b, inner, inner.y, "↵ go back · esc ", dim);
    put_text(
        b,
        inner.x,
        inner.y + 1,
        inner.width,
        &divider,
        divider_style,
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
    let files_text = |count: usize| match count {
        0 => "no changes".to_owned(),
        1 => "1 file".to_owned(),
        count => format!("{count} files"),
    };
    let start = picker
        .selected
        .min(rows.len().saturating_sub(1))
        .saturating_sub(VISIBLE_CHECKPOINTS - 1);
    let mut row_hits = vec![(tabs, CHECKPOINT_TOGGLE_HIT)];
    for (offset, (position, row)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(VISIBLE_CHECKPOINTS)
        .enumerate()
    {
        let entry = &picker.entries[ClientCheckpointPickerOverlay::entry_index(*row)];
        let rect = Rect::new(list.x, list.y + offset as u16, list.width, 1);
        let selected = position == picker.selected;
        let quiet = entry.changed_files.is_empty();
        let style = match (selected, row) {
            (true, _) => Style::default().fg(contrast(p)).bg(p.accent),
            (false, ClientCheckpointRow::UndoRewind(_)) => green,
            (false, _) if quiet => dim,
            _ => base,
        };
        b.set_style(rect, style);
        let (left, right) = match row {
            ClientCheckpointRow::UndoRewind(_) => (
                format!(" ↶ undo last rewind ({})", clock(entry.created_at)),
                format!("{} ", files_text(entry.changed_files.len())),
            ),
            ClientCheckpointRow::Checkpoint(_) => {
                let age = super::super::space_picker::closed_age(entry.created_at, now);
                let files = files_text(entry.changed_files.len());
                (
                    format!(" {}  {}", clock(entry.created_at), entry.label),
                    match &entry.agent {
                        Some(agent) => format!("{agent} · {files}  {age:>3} "),
                        None => format!("{files}  {age:>3} "),
                    },
                )
            }
        };
        let left_width = rect.width.saturating_sub(display_width(&right) + 1);
        put_text(
            b,
            rect.x,
            rect.y,
            left_width,
            &fit(&left, left_width),
            style,
        );
        put_right_text(b, rect, rect.y, &right, style);
        row_hits.push((rect, position));
    }
    if rows.is_empty() {
        let message = if picker.loading {
            " loading…"
        } else if picker.unsupported {
            " restart the herdr server to use checkpoints"
        } else if picker.error.is_some() {
            ""
        } else if picker.entries.is_empty() {
            " no checkpoints yet · saved before each prompt when experimental.checkpoints is on"
        } else {
            " no prompt has changed files yet · press a to see all prompts"
        };
        put_text(b, list.x, list.y, list.width, message, dim);
    }

    // What Enter does to the highlighted row, in plain words.
    let explain_y = list.bottom();
    put_text(b, inner.x, explain_y, inner.width, &divider, divider_style);
    let (first, second, style) = match rows.get(picker.selected) {
        Some(ClientCheckpointRow::UndoRewind(index)) => (
            format!(
                " ↵ undoes your last rewind: files go back to how they were at {}",
                clock(picker.entries[*index].created_at)
            ),
            String::new(),
            green,
        ),
        Some(ClientCheckpointRow::Checkpoint(index)) => {
            let entry = &picker.entries[*index];
            // Everything newer than the target is undone, the target prompt included.
            let undone = picker.entries[..=*index]
                .iter()
                .filter(|entry| !ClientCheckpointPickerOverlay::is_undo(entry))
                .count();
            let mut files: Vec<&str> = picker.entries[..=*index]
                .iter()
                .flat_map(|entry| entry.changed_files.iter())
                .map(|path| path.rsplit('/').next().unwrap_or(path))
                .collect();
            files.sort_unstable();
            files.dedup();
            let shown = 5;
            let restores = match files.len() {
                0 => " · no files change".to_owned(),
                count if count > shown => {
                    format!(
                        " · restores {}, +{}",
                        files[..shown].join(", "),
                        count - shown
                    )
                }
                _ => format!(" · restores {}", files.join(", ")),
            };
            (
                format!(
                    " ↵ puts your files back to {}, right before {}",
                    clock(entry.created_at),
                    entry.label
                ),
                format!(
                    "   undoes {undone} prompt{}{restores}",
                    if undone == 1 { "" } else { "s" }
                ),
                base,
            )
        }
        None => (String::new(), String::new(), dim),
    };
    put_text(
        b,
        inner.x,
        explain_y + 1,
        inner.width,
        &fit(&first, inner.width),
        style,
    );
    put_text(
        b,
        inner.x,
        explain_y + 2,
        inner.width,
        &fit(&second, inner.width),
        dim,
    );
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

/// Cuts `text` to `width` columns, ending in `…` (and keeping a closing quote
/// when the text ends in one).
fn fit(text: &str, width: u16) -> String {
    let width = width as usize;
    if display_width(text) as usize <= width || width < 3 {
        return text.to_owned();
    }
    let tail = if text.ends_with('"') { "…\"" } else { "…" };
    let mut out = String::new();
    for character in text.chars() {
        if display_width(&out) as usize + 1 + tail.chars().count() > width {
            break;
        }
        out.push(character);
    }
    out + tail
}
