use super::*;
use ratatui::{
    text::Line,
    widgets::{Paragraph, Widget},
};

fn workspace_selection_background(palette: &Palette) -> ratatui::style::Color {
    if palette.selection_bg == ratatui::style::Color::Reset {
        palette.active_row_bg
    } else {
        palette.selection_bg
    }
}

pub(in crate::client::shell) fn workspace_active_background(
    palette: &Palette,
    navigating: bool,
) -> ratatui::style::Color {
    // The fallback cursor shares the active-row color; only fill the cursor while navigating.
    if navigating && palette.selection_bg == ratatui::style::Color::Reset {
        palette.sidebar_bg
    } else {
        palette.active_row_bg
    }
}

pub(in crate::client::shell) fn collapsed_sidebar_sections(
    area: Rect,
) -> (Rect, Option<u16>, Rect) {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.is_empty() {
        return (Rect::default(), None, Rect::default());
    }
    if content.height < 7 {
        return (content, None, Rect::default());
    }
    let workspace_height = content.height.div_ceil(2);
    let divider_y = content.y + workspace_height;
    let detail_height = content.height.saturating_sub(workspace_height + 1);
    (
        Rect::new(content.x, content.y, content.width, workspace_height),
        Some(divider_y),
        Rect::new(content.x, divider_y + 1, content.width, detail_height),
    )
}

pub(crate) fn render_collapsed_sidebar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    selected_workspace_id: Option<&str>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    let selection_background = workspace_selection_background(palette);
    let active_background = workspace_active_background(palette, selected_workspace_id.is_some());
    render_sidebar_background(buffer, area, palette);
    let (workspace_area, divider_y, detail_area) = collapsed_sidebar_sections(area);
    for (index, workspace) in snapshot
        .workspaces
        .iter()
        .take(workspace_area.height as usize)
        .enumerate()
    {
        let rect = Rect::new(
            workspace_area.x,
            workspace_area.y + index as u16,
            workspace_area.width,
            1,
        );
        let selected = selected_workspace_id == Some(workspace.workspace_id.as_str());
        if selected {
            buffer.set_style(rect, Style::default().bg(selection_background));
        } else if workspace.focused {
            buffer.set_style(rect, Style::default().bg(active_background));
        }
        let number_style = if selected {
            Style::default()
                .fg(palette.overlay1)
                .bg(selection_background)
        } else if workspace.focused {
            Style::default().fg(palette.text).bg(active_background)
        } else {
            Style::default().fg(palette.overlay0)
        };
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width.min(2),
            &format!("{:<2}", index + 1),
            number_style,
        );
        let status = workspace.agent_status;
        put_text(
            buffer,
            rect.x.saturating_add(2),
            rect.y,
            rect.width.saturating_sub(2),
            status_icon(status, config.status_indicators),
            Style::default().fg(status_color(status, palette)),
        );
        hits.workspaces.push(WorkspaceHit {
            rect,
            endpoint_id: ClientEndpointId::Local,
            workspace_id: workspace.workspace_id.clone(),
            indented: false,
            group_toggle: None,
        });
    }

    if let Some(divider_y) = divider_y {
        put_text(
            buffer,
            workspace_area.x,
            divider_y,
            workspace_area.width,
            &"─".repeat(workspace_area.width as usize),
            Style::default().fg(palette.surface_dim),
        );
    }

    let detail_content = Rect::new(
        detail_area.x,
        detail_area.y,
        detail_area.width,
        detail_area.height.saturating_sub(1),
    );
    for (index, pane_id) in super::ordered_agent_pane_ids(snapshot, config.agent_panel_sort)
        .into_iter()
        .take(detail_content.height as usize)
        .enumerate()
    {
        let Some(agent) = snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == pane_id)
        else {
            continue;
        };
        let rect = Rect::new(
            detail_content.x,
            detail_content.y + index as u16,
            detail_content.width,
            1,
        );
        if agent.focused {
            buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
        }
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width.min(2),
            &format!("{:<2}", index + 1),
            Style::default().fg(if agent.focused {
                palette.text
            } else {
                palette.overlay0
            }),
        );
        put_text(
            buffer,
            rect.x.saturating_add(2),
            rect.y,
            rect.width.saturating_sub(2),
            status_icon(agent.agent_status, config.status_indicators),
            Style::default().fg(status_color(agent.agent_status, palette)),
        );
        hits.agents.push((rect, pane_id));
    }
    hits.sidebar_toggle = if area.is_empty() || workspace_area.width == 0 {
        Rect::default()
    } else {
        Rect::new(
            workspace_area.x + workspace_area.width / 2,
            area.bottom().saturating_sub(1),
            1,
            1,
        )
    };
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        hits.sidebar_toggle.y,
        hits.sidebar_toggle.width,
        "»",
        if super::super::global_menu::global_menu_attention(snapshot) {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.overlay0)
        },
    );
}

pub(crate) fn render_sidebar(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    state: &mut ShellRenderState<'_>,
    hits: &mut ShellHitMap,
) {
    let palette = &config.palette;
    render_sidebar_background(buffer, area, palette);
    hits.sidebar_divider = if area.is_empty() {
        Rect::default()
    } else {
        Rect::new(area.right().saturating_sub(1), area.y, 1, area.height)
    };
    // Grouped agents sit under their space in one list, like Zed's thread
    // sidebar; priority order (or a filtered agent view) keeps its own panel.
    let nested = config.agents.nested
        && snapshot.agent_view_label.is_none()
        && config.agent_panel_sort == crate::config::AgentPanelSortConfig::Spaces;
    let (workspace_area, detail_area) = if nested {
        let (top, bottom) = crate::ui::expanded_sidebar_sections(area, 1.0);
        (top.union(bottom), Rect::default())
    } else {
        crate::ui::expanded_sidebar_sections(area, state.sidebar_section_split)
    };
    hits.sidebar_section_divider = if nested {
        Rect::default()
    } else {
        crate::ui::sidebar_section_divider_rect(area, state.sidebar_section_split)
    };
    put_text(
        buffer,
        workspace_area.x,
        workspace_area.y,
        workspace_area.width,
        " spaces",
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    if nested && workspace_area.height > 0 {
        // Switches the compact list and the expanded tree.
        let view = if state.space_tree { "tree" } else { "list" };
        let view_width = display_width(view).min(workspace_area.width);
        let view_rect = Rect::new(
            workspace_area.right().saturating_sub(view_width),
            workspace_area.y,
            view_width,
            1,
        );
        if config.mouse_capture {
            hits.space_view_toggle = view_rect;
        }
        put_text(
            buffer,
            view_rect.x,
            view_rect.y,
            view_rect.width,
            view,
            Style::default()
                .fg(palette.overlay0)
                .add_modifier(Modifier::BOLD),
        );
    }
    // Nested spaces draw as a branch tree: worktrees hang off their repo's
    // branch line and status is the icon alone. The list keeps it compact (a
    // lone agent rides on its space's line); the tree gives every agent a line
    // and lets the repo's ⌃ fold the whole group.
    let compact = nested && !state.space_tree;
    let mut agents = if nested {
        crate::client::shell::agent_sidebar::agent_rows(snapshot, config, None)
    } else {
        Vec::new()
    };
    if nested {
        for row in agents.iter_mut().flat_map(|agent| agent.rows.iter_mut()) {
            row.retain(|token| !matches!(token.kind, crate::ui::ResolvedTokenKind::StateText(_)));
        }
    }
    let agents = agents;
    // ponytail: keyed by workspace id, so a folded space can reopen folded
    // under a reused id after restart; key by something stabler if that bites.
    let agents_key =
        |workspace: &ClientShellWorkspace| format!("agents:{}", workspace.workspace_id);
    let agent_count = |workspace: &ClientShellWorkspace| {
        agents
            .iter()
            .filter(|agent| agent.workspace_id == workspace.workspace_id)
            .count()
    };
    let inline_agent = |workspace: &ClientShellWorkspace| {
        (compact && agent_count(workspace) == 1)
            .then(|| {
                agents
                    .iter()
                    .find(|agent| agent.workspace_id == workspace.workspace_id)
            })
            .flatten()
    };
    // Only agents with lines of their own can be folded away.
    let has_agents =
        |workspace: &ClientShellWorkspace| agent_count(workspace) > usize::from(compact);
    // A repo checkout folds with its worktree group.
    let tree_group_folded = |workspace: &ClientShellWorkspace| {
        nested
            && workspace.worktree.as_ref().is_some_and(|worktree| {
                !worktree.is_linked_worktree && state.collapsed_groups.contains(&worktree.key)
            })
    };
    // A folded space hides its agents; worktrees under a project have no ⌃ of
    // their own, so theirs always show.
    let space_agents = |workspace: &ClientShellWorkspace, indented: bool| {
        agents
            .iter()
            .filter(|agent| agent.workspace_id == workspace.workspace_id)
            .filter(|_| indented || !state.collapsed_groups.contains(&agents_key(workspace)))
            .filter(|_| !tree_group_folded(workspace))
            .filter(|_| inline_agent(workspace).is_none())
            .collect::<Vec<_>>()
    };
    let agent_height =
        |agent: &crate::client::shell::agent_sidebar::AgentRow| agent.rows.len().max(1) as u16;
    let space_rows = |workspace: &ClientShellWorkspace, entry: &WorkspaceEntry| {
        let mut rows = workspace_rows(
            workspace,
            displayed_workspace_status(snapshot, workspace, state.collapsed_groups),
            entry.indented,
            &config.spaces,
        );
        // The branch moves onto the name line (see below).
        if nested {
            rows.truncate(1);
        }
        rows
    };

    let entries = workspace_entries(snapshot, state.collapsed_groups);
    let body = Rect::new(
        workspace_area.x,
        workspace_area.y.saturating_add(WORKSPACE_HEADER_ROWS),
        workspace_area.width,
        workspace_area
            .height
            .saturating_sub(WORKSPACE_HEADER_ROWS + 1),
    );
    hits.workspace_body = body;
    // Tree: a repo with worktrees gets its branch on a line of its own.
    let branch_line = |position: usize| {
        nested
            && entries.get(position).is_some_and(|entry| !entry.indented)
            && entries
                .get(position + 1)
                .is_some_and(|entry| entry.indented)
    };
    let row_heights = entries
        .iter()
        .enumerate()
        .map(|(position, entry)| {
            snapshot
                .workspaces
                .get(entry.index)
                .map(|workspace| {
                    let space = space_rows(workspace, entry)
                        .len()
                        .max(1)
                        .min(u16::MAX as usize) as u16;
                    space_agents(workspace, entry.indented)
                        .into_iter()
                        .fold(space, |height, agent| {
                            height.saturating_add(agent_height(agent))
                        })
                        .saturating_add(u16::from(branch_line(position)))
                })
                .unwrap_or(1)
        })
        .collect::<Vec<_>>();
    // Nested: a divider line between spaces and, in the list, a blank line
    // before each grouped worktree.
    let space_gap = if nested { 1 } else { config.spaces.row_gap };
    let gaps = entries
        .iter()
        .enumerate()
        .map(|(index, _)| {
            entries
                .get(index + 1)
                .map_or(0, |next| u16::from(!next.indented) * space_gap)
        })
        .collect::<Vec<_>>();
    let mut metrics = super::scroll::list_scroll_metrics(
        &row_heights,
        &gaps,
        body.height,
        *state.workspace_scroll,
    );
    if !body.is_empty() && std::mem::take(state.reveal_focused_workspace) {
        if let Some(target) = entries
            .iter()
            .position(|entry| snapshot.workspaces[entry.index].focused)
        {
            *state.workspace_scroll = super::scroll::list_scroll_start_to_reveal(
                &row_heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
                target,
            );
            metrics = super::scroll::list_scroll_metrics(
                &row_heights,
                &gaps,
                body.height,
                *state.workspace_scroll,
            );
        }
    }
    hits.workspace_max_scroll = metrics.max_offset_from_bottom;
    hits.workspace_scroll_metrics = Some(metrics);
    *state.workspace_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for (entry_position, entry) in entries.iter().enumerate().skip(*state.workspace_scroll) {
        let Some(workspace) = snapshot.workspaces.get(entry.index) else {
            continue;
        };
        let status = displayed_workspace_status(snapshot, workspace, state.collapsed_groups);
        let rows = space_rows(workspace, entry);
        let row_height = (rows.len().max(1).min(u16::MAX as usize) as u16).min(body.height);
        if y.saturating_add(row_height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, row_height);
        let selected = state.selected_workspace_id.is_some_and(|target| {
            target.matches(state.active_endpoint_id, &workspace.workspace_id)
        });
        let dragged = state.dragged_workspace_id == Some(workspace.workspace_id.as_str());
        if selected {
            buffer.set_style(rect, Style::default().bg(palette.selection_bg));
        } else if dragged {
            buffer.set_style(rect, Style::default().bg(palette.surface1));
        } else if workspace.focused {
            buffer.set_style(rect, Style::default().bg(palette.active_row_bg));
        }
        // Nested: the branch sits at the right end of the name line, then the
        // show/hide-agents chevron, then any group toggle. Worktree children
        // already show their branch as the name.
        // Nested, the repo's ⌃ folds its whole group instead: always in the
        // tree, and in the list only once folded, so it can be unfolded.
        let group_key = parent_group_key(snapshot, entry.index).filter(|_| !entry.indented);
        let tree_group_key = group_key
            .clone()
            .filter(|key| nested && (!compact || state.collapsed_groups.contains(key)));
        let show_group_toggle = group_key.is_some() && !nested;
        let mut right = rect
            .right()
            .saturating_sub(if show_group_toggle { 2 } else { 1 });
        // Only project rows fold; worktrees under them have no ⌃.
        let agents_toggle = (nested
            && rect.height > 0
            && !entry.indented
            && (has_agents(workspace) || tree_group_key.is_some()))
        .then(|| {
            let key = tree_group_key.unwrap_or_else(|| agents_key(workspace));
            let toggle = Rect::new(right.saturating_sub(1).max(rect.x), rect.y, 1, 1);
            put_text(
                buffer,
                toggle.x,
                toggle.y,
                1,
                if state.collapsed_groups.contains(&key) {
                    "⌄"
                } else {
                    "⌃"
                },
                // Same colour as the space name.
                if workspace.focused || dragged {
                    Style::default()
                        .fg(palette.text)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.subtext0)
                },
            );
            right = toggle.x.saturating_sub(1).max(rect.x);
            (toggle, key)
        });
        let has_tree_children = branch_line(entry_position);
        let branch = workspace
            .branch
            .as_deref()
            .filter(|_| nested && rect.height > 0 && !has_tree_children);
        let mut right_segments = branch.map(|branch| {
            // A grouped worktree is usually named after its branch; say it once.
            let branch = if entry.indented && (!workspace.custom_label || workspace.label == branch)
            {
                ""
            } else {
                branch
            };
            // Ahead/behind arrows follow the branch, as on the dropped git row.
            let (ahead, behind) = workspace.git_ahead_behind.unwrap_or((0, 0));
            vec![
                (
                    branch.to_string(),
                    if workspace.focused {
                        palette.mauve
                    } else {
                        palette.overlay0
                    },
                ),
                (
                    if ahead > 0 {
                        format!(" ↑{ahead}")
                    } else {
                        String::new()
                    },
                    palette.green,
                ),
                (
                    if behind > 0 {
                        format!(" ↓{behind}")
                    } else {
                        String::new()
                    },
                    palette.red,
                ),
                (git_changes_badge(workspace), palette.yellow),
            ]
        });
        let segments_width = |segments: &[(String, ratatui::style::Color)]| {
            segments.iter().fold(0u16, |width, (text, _)| {
                width.saturating_add(display_width(text))
            })
        };
        // Columns before the name: the dot's margin, or a tree branch.
        let lead = if entry.indented { 6u16 } else { 1 };
        let name_width = rows.first().map_or(0, |row| {
            crate::ui::resolved_token_spans(
                row,
                (
                    status_icon(status, config.status_indicators),
                    Style::default(),
                ),
                Style::default(),
                Style::default(),
                Style::default(),
                Style::default(),
                palette,
                usize::MAX,
            )
            .iter()
            .fold(0u16, |width, span| {
                width.saturating_add(display_width(&span.content))
            })
        });
        let space_label = rows.first().and_then(|row| {
            row.iter().find_map(|token| match &token.kind {
                crate::ui::ResolvedTokenKind::Workspace(text) => Some(text.clone()),
                _ => None,
            })
        });
        // The name and dot come first: a branch that would squeeze them is
        // dropped, leaving only its ahead/behind arrows.
        if let Some(segments) = right_segments
            .as_mut()
            .filter(|segments| !segments[0].0.is_empty() && !rows.is_empty())
        {
            // The leading columns, then render_workspace_rows' 2-column margin.
            let needed = lead.saturating_add(name_width).saturating_add(2);
            let name_end = right
                .saturating_sub(segments_width(segments))
                .saturating_add(1);
            if name_end.saturating_sub(rect.x) < needed {
                segments[0].0.clear();
            }
        }
        let right_segments =
            right_segments.filter(|segments| segments.iter().any(|(text, _)| !text.is_empty()));
        let name_rect = right_segments.as_ref().map_or(rect, |segments| {
            let x = right.saturating_sub(segments_width(segments)).max(rect.x);
            segments.iter().fold(x, |cursor, (text, color)| {
                let end = put_segment(
                    buffer,
                    cursor,
                    rect.y,
                    right,
                    text,
                    Style::default().fg(*color),
                );
                if text.starts_with(GIT_CHANGES_PREFIX) && end > cursor && config.mouse_capture {
                    hits.space_git_badges.push((
                        Rect::new(cursor, rect.y, end - cursor, 1),
                        workspace.workspace_id.clone(),
                    ));
                }
                end
            });
            // render_workspace_rows stops 2 columns short of its area: 1 column gap.
            Rect::new(rect.x, rect.y, (x + 1).saturating_sub(rect.x), rect.height)
        });
        let name_rect = match (&right_segments, &agents_toggle) {
            (None, Some(_)) => Rect::new(
                rect.x,
                rect.y,
                (right + 1).saturating_sub(rect.x),
                rect.height,
            ),
            _ => name_rect,
        };
        if config.mouse_capture {
            hits.space_agent_toggles.extend(agents_toggle);
        }
        render_workspace_rows(
            buffer,
            name_rect,
            status,
            config.status_indicators,
            entry,
            rows,
            workspace.focused,
            selected,
            state.selected_workspace_id.is_some(),
            dragged,
            palette,
        );
        if let Some(agent) = inline_agent(workspace) {
            if agent.focused {
                buffer.set_style(
                    Rect::new(body.x, rect.y, content_width, 1),
                    Style::default().bg(palette.surface0),
                );
            }
            // Its name follows the space's, unless they match or it won't fit whole.
            let name = agent
                .rows
                .iter()
                .flatten()
                .find_map(|token| match &token.kind {
                    crate::ui::ResolvedTokenKind::Agent(text) => Some(text.as_str()),
                    _ => None,
                });
            if let Some(name) = name.filter(|name| space_label.as_deref() != Some(*name)) {
                let x = rect.x.saturating_add(lead).saturating_add(name_width);
                let end = name_rect.right().saturating_sub(2);
                if x.saturating_add(3).saturating_add(display_width(name)) <= end {
                    let x = put_segment(
                        buffer,
                        x,
                        rect.y,
                        end,
                        " · ",
                        Style::default().fg(palette.overlay0),
                    );
                    put_segment(
                        buffer,
                        x,
                        rect.y,
                        end,
                        name,
                        Style::default().fg(palette.subtext0),
                    );
                }
            }
        }
        // The space's agents, indented under it; they scroll with it.
        let mut agent_y = rect.bottom();
        // A dim guide runs down under the space's status dot (or a tree
        // worktree's dot); agents sit after it.
        let guide = if nested && entry.indented { 6 } else { 1 };
        let indent = guide + 1;
        for agent in space_agents(workspace, entry.indented) {
            let height = agent_height(agent);
            if agent_y.saturating_add(height) > body.bottom() {
                break;
            }
            let agent_rect = Rect::new(
                body.x.saturating_add(indent),
                agent_y,
                content_width.saturating_sub(indent),
                height,
            );
            crate::client::shell::agent_sidebar::render_agent_row(
                buffer, agent_rect, agent, config,
            );
            if agent.focused {
                // Themes may set active_row_bg to the sidebar colour, so the
                // focused agent gets a surface bar across the whole row.
                buffer.set_style(
                    Rect::new(body.x, agent_y, content_width, height),
                    Style::default().bg(palette.surface0),
                );
            }
            for row in agent_y..agent_y.saturating_add(height) {
                put_text(
                    buffer,
                    body.x.saturating_add(guide),
                    row,
                    1,
                    "│",
                    Style::default().fg(palette.overlay0),
                );
                // The tree trunk runs on past a worktree that has siblings below.
                if nested && entry.indented && !entry.last_child {
                    put_text(
                        buffer,
                        body.x.saturating_add(3),
                        row,
                        1,
                        "│",
                        Style::default().fg(palette.overlay0),
                    );
                }
            }
            hits.agents.push((agent_rect, agent.pane_id.clone()));
            agent_y = agent_y.saturating_add(height);
        }
        if has_tree_children {
            if agent_y < body.bottom() {
                let (ahead, behind) = workspace.git_ahead_behind.unwrap_or((0, 0));
                let line_end = rect.right();
                let mut x = body.x.saturating_add(3);
                x = put_segment(
                    buffer,
                    x,
                    agent_y,
                    line_end,
                    workspace.branch.as_deref().unwrap_or_default(),
                    Style::default().fg(if workspace.focused {
                        palette.mauve
                    } else {
                        palette.overlay0
                    }),
                );
                if ahead > 0 {
                    x = put_segment(
                        buffer,
                        x,
                        agent_y,
                        line_end,
                        &format!(" ↑{ahead}"),
                        Style::default().fg(palette.green),
                    );
                }
                if behind > 0 {
                    x = put_segment(
                        buffer,
                        x,
                        agent_y,
                        line_end,
                        &format!(" ↓{behind}"),
                        Style::default().fg(palette.red),
                    );
                }
                let badge = git_changes_badge(workspace);
                if !badge.is_empty() {
                    let end = put_segment(
                        buffer,
                        x,
                        agent_y,
                        line_end,
                        &badge,
                        Style::default().fg(palette.yellow),
                    );
                    if end > x && config.mouse_capture {
                        hits.space_git_badges.push((
                            Rect::new(x, agent_y, end - x, 1),
                            workspace.workspace_id.clone(),
                        ));
                    }
                }
            }
            agent_y = agent_y.saturating_add(1);
        }
        let row_height = agent_y.saturating_sub(rect.y);
        let group_toggle = show_group_toggle
            .then(|| {
                render_parent_group_toggle(
                    buffer,
                    rect,
                    snapshot,
                    entry.index,
                    state.collapsed_groups,
                    palette,
                )
            })
            .flatten();
        hits.workspaces.push(WorkspaceHit {
            rect,
            endpoint_id: ClientEndpointId::Local,
            workspace_id: workspace.workspace_id.clone(),
            indented: entry.indented,
            group_toggle,
        });
        let next = entries.get(entry_position + 1);
        let gap = next.map_or(0, |next| u16::from(!next.indented) * space_gap);
        let divider_y = y.saturating_add(row_height);
        if nested && next.is_some_and(|next| !next.indented) && divider_y < body.bottom() {
            put_text(
                buffer,
                body.x.saturating_add(1),
                divider_y,
                content_width.saturating_sub(2),
                &"─".repeat(content_width.saturating_sub(2) as usize),
                Style::default().fg(palette.surface_dim),
            );
        }
        y = y.saturating_add(row_height + gap);
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.workspace_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, palette);
    }

    if let Some(row) = state.workspace_drop_indicator_row.filter(|row| {
        *row >= workspace_area.y.saturating_add(1)
            && *row < workspace_area.bottom().saturating_sub(1)
    }) {
        put_text(
            buffer,
            body.x,
            row,
            body.width,
            &"─".repeat(body.width as usize),
            Style::default().fg(palette.accent),
        );
    }

    let footer_y = workspace_area.bottom().saturating_sub(1);
    if config.mouse_capture {
        hits.new_workspace = Rect::new(
            workspace_area.x,
            footer_y,
            5.min(workspace_area.width),
            u16::from(workspace_area.height > 0),
        );
        put_text(
            buffer,
            workspace_area.x,
            footer_y,
            workspace_area.width,
            " new",
            Style::default().fg(palette.overlay0),
        );
        let attention = super::super::global_menu::global_menu_attention(snapshot);
        let launcher_width = if attention { 8 } else { 6 }.min(workspace_area.width);
        hits.global_launcher = Rect::new(
            workspace_area.right().saturating_sub(launcher_width),
            footer_y,
            launcher_width,
            1,
        );
        if attention {
            let start_x = workspace_area.right().saturating_sub(6);
            put_text(
                buffer,
                start_x,
                footer_y,
                2,
                "● ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            );
            put_text(
                buffer,
                start_x.saturating_add(2),
                footer_y,
                4,
                "menu",
                Style::default().fg(palette.overlay0),
            );
        } else {
            put_right_text(
                buffer,
                workspace_area,
                footer_y,
                "menu",
                Style::default().fg(palette.overlay0),
            );
        }
    }

    if !nested {
        super::render_agent_panel(
            buffer,
            detail_area,
            snapshot,
            config,
            state.agent_scroll,
            hits,
        );
    }

    hits.sidebar_toggle = Rect::new(
        area.right().saturating_sub(2),
        area.bottom().saturating_sub(1),
        u16::from(area.width > 1),
        u16::from(area.height > 0),
    );
    put_text(
        buffer,
        hits.sidebar_toggle.x,
        hits.sidebar_toggle.y,
        hits.sidebar_toggle.width,
        "«",
        Style::default().fg(palette.overlay0),
    );
}

pub(crate) fn workspace_entries(
    snapshot: &ClientShellSnapshot,
    collapsed_groups: &HashSet<String>,
) -> Vec<WorkspaceEntry> {
    let mut members = HashMap::<&str, Vec<usize>>::new();
    for (index, workspace) in snapshot.workspaces.iter().enumerate() {
        if let Some(worktree) = &workspace.worktree {
            members.entry(&worktree.key).or_default().push(index);
        }
    }
    let grouped = members
        .iter()
        .filter(|(_, indices)| {
            indices.iter().any(|index| {
                snapshot.workspaces[*index]
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| worktree.is_linked_worktree)
            }) && indices.iter().any(|index| {
                snapshot.workspaces[*index]
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| !worktree.is_linked_worktree)
            })
        })
        .map(|(key, _)| *key)
        .collect::<HashSet<_>>();
    let mut emitted = HashSet::<&str>::new();
    let mut entries = Vec::new();
    for (index, workspace) in snapshot.workspaces.iter().enumerate() {
        let Some(worktree) = workspace
            .worktree
            .as_ref()
            .filter(|worktree| grouped.contains(worktree.key.as_str()))
        else {
            entries.push(WorkspaceEntry {
                index,
                indented: false,
                last_child: false,
            });
            continue;
        };
        if !emitted.insert(&worktree.key) {
            continue;
        }
        let Some(group_members) = members.get(worktree.key.as_str()) else {
            continue;
        };
        for parent in group_members.iter().copied().filter(|member| {
            snapshot.workspaces[*member]
                .worktree
                .as_ref()
                .is_some_and(|worktree| !worktree.is_linked_worktree)
        }) {
            entries.push(WorkspaceEntry {
                index: parent,
                indented: false,
                last_child: false,
            });
        }
        if collapsed_groups.contains(&worktree.key) {
            if let Some(active) = group_members.iter().copied().find(|member| {
                let workspace = &snapshot.workspaces[*member];
                workspace.focused
                    && workspace
                        .worktree
                        .as_ref()
                        .is_some_and(|worktree| worktree.is_linked_worktree)
            }) {
                entries.push(WorkspaceEntry {
                    index: active,
                    indented: true,
                    last_child: true,
                });
            }
            continue;
        }
        let children = group_members
            .iter()
            .copied()
            .filter(|member| {
                snapshot.workspaces[*member]
                    .worktree
                    .as_ref()
                    .is_some_and(|worktree| worktree.is_linked_worktree)
            })
            .collect::<Vec<_>>();
        for (child_index, child) in children.iter().enumerate() {
            entries.push(WorkspaceEntry {
                index: *child,
                indented: true,
                last_child: child_index + 1 == children.len(),
            });
        }
    }
    entries
}

fn parent_group_key(snapshot: &ClientShellSnapshot, index: usize) -> Option<String> {
    let workspace = snapshot.workspaces.get(index)?;
    let worktree = workspace.worktree.as_ref()?;
    if worktree.is_linked_worktree {
        return None;
    }
    snapshot
        .workspaces
        .iter()
        .any(|candidate| {
            candidate.worktree.as_ref().is_some_and(|candidate| {
                candidate.key == worktree.key && candidate.is_linked_worktree
            })
        })
        .then(|| worktree.key.clone())
}

pub(in crate::client::shell) fn workspace_close_is_group(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
) -> bool {
    let Some(worktree) = workspace
        .worktree
        .as_ref()
        .filter(|worktree| !worktree.is_linked_worktree)
    else {
        return false;
    };
    let mut has_child = false;
    for member in &snapshot.workspaces {
        if member.workspace_id == workspace.workspace_id {
            continue;
        }
        if let Some(candidate) = member
            .worktree
            .as_ref()
            .filter(|candidate| candidate.key == worktree.key)
        {
            if !candidate.is_linked_worktree {
                return false;
            }
            has_child = true;
        }
    }
    has_child
}

pub(in crate::client::shell) fn render_parent_group_toggle(
    buffer: &mut Buffer,
    workspace_rect: Rect,
    snapshot: &ClientShellSnapshot,
    workspace_index: usize,
    collapsed_groups: &HashSet<String>,
    palette: &Palette,
) -> Option<(Rect, String)> {
    let key = parent_group_key(snapshot, workspace_index)?;
    let toggle = Rect::new(
        workspace_rect.right().saturating_sub(1),
        workspace_rect.y,
        1,
        1,
    );
    put_text(
        buffer,
        toggle.x,
        toggle.y,
        toggle.width,
        if collapsed_groups.contains(&key) {
            "▸"
        } else {
            "▾"
        },
        Style::default().fg(palette.accent),
    );
    Some((toggle, key))
}

pub(in crate::client::shell) fn displayed_workspace_status(
    snapshot: &ClientShellSnapshot,
    workspace: &ClientShellWorkspace,
    collapsed_groups: &HashSet<String>,
) -> crate::api::schema::AgentStatus {
    let Some(worktree) = workspace
        .worktree
        .as_ref()
        .filter(|worktree| !worktree.is_linked_worktree)
    else {
        return workspace.agent_status;
    };
    if !collapsed_groups.contains(&worktree.key) {
        return workspace.agent_status;
    }
    snapshot
        .workspaces
        .iter()
        .filter(|candidate| {
            candidate.worktree.as_ref().is_some_and(|member| {
                member.key == worktree.key
                    && (member.is_linked_worktree
                        || candidate.workspace_id == workspace.workspace_id)
            })
        })
        .map(|candidate| candidate.agent_status)
        .max_by_key(|status| status_priority(*status))
        .unwrap_or(workspace.agent_status)
}

pub(in crate::client::shell) fn workspace_rows(
    workspace: &ClientShellWorkspace,
    status: crate::api::schema::AgentStatus,
    indented: bool,
    config: &SpacesSidebarConfig,
) -> Vec<Vec<crate::ui::ResolvedToken>> {
    let label = if indented && !workspace.custom_label {
        workspace
            .branch
            .as_deref()
            .and_then(|branch| branch.strip_prefix("worktree/").or(Some(branch)))
            .unwrap_or(&workspace.label)
    } else {
        &workspace.label
    };
    let token_values = workspace.tokens.iter().cloned().collect::<HashMap<_, _>>();
    crate::ui::sidebar_space_rows(
        config,
        crate::ui::SpaceTokenContext {
            workspace: label,
            branch: workspace.branch.as_deref(),
            state_text: status_text(status),
            ahead_behind: workspace.git_ahead_behind,
            tokens: &token_values,
            suppress_git_details: indented,
        },
    )
}

pub(in crate::client::shell) fn render_workspace_rows(
    buffer: &mut Buffer,
    area: Rect,
    status: crate::api::schema::AgentStatus,
    indicators: crate::config::StatusIndicatorStyle,
    entry: &WorkspaceEntry,
    rows: Vec<Vec<crate::ui::ResolvedToken>>,
    focused: bool,
    selected: bool,
    navigating: bool,
    dragged: bool,
    palette: &Palette,
) {
    for (row_index, row) in rows.iter().enumerate() {
        let y = area.y + row_index as u16;
        if y >= area.bottom() {
            break;
        }
        let mut x = area.x;
        if entry.indented {
            let prefix = if row_index == 0 {
                if entry.last_child {
                    "   └─ "
                } else {
                    "   ├─ "
                }
            } else if entry.last_child {
                "        "
            } else {
                "   │    "
            };
            x = put_segment(
                buffer,
                x,
                y,
                area.right(),
                prefix,
                Style::default().fg(palette.overlay0),
            );
        } else if row_index == 0 {
            x = x.saturating_add(1);
        } else {
            x = x.saturating_add(3);
        }
        let highlighted = focused || dragged;
        let workspace_style = Style::default()
            .fg(if highlighted {
                palette.text
            } else {
                palette.subtext0
            })
            .add_modifier(if highlighted {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let secondary_style = Style::default().fg(if focused {
            palette.mauve
        } else {
            palette.overlay0
        });
        let spans = crate::ui::resolved_token_spans(
            row,
            (
                status_icon(status, indicators),
                Style::default().fg(status_color(status, palette)),
            ),
            Style::default().fg(status_color(status, palette)),
            workspace_style,
            secondary_style,
            Style::default().fg(palette.overlay1),
            palette,
            area.right().saturating_sub(2).saturating_sub(x) as usize,
        );
        Paragraph::new(Line::from(spans)).render(
            Rect::new(x, y, area.right().saturating_sub(2).saturating_sub(x), 1),
            buffer,
        );
    }

    let background = if selected {
        Some(workspace_selection_background(palette))
    } else if dragged {
        Some(palette.surface1)
    } else if focused {
        Some(workspace_active_background(palette, navigating))
    } else {
        None
    };
    if let Some(background) = background {
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].set_bg(background);
            }
        }
    }
}

const GIT_CHANGES_PREFIX: &str = " ±";

/// " ±N" for a space with N uncommitted files; empty when clean or unknown.
fn git_changes_badge(workspace: &ClientShellWorkspace) -> String {
    match workspace.git_changes {
        Some(count) if count > 0 => format!("{GIT_CHANGES_PREFIX}{count}"),
        _ => String::new(),
    }
}
