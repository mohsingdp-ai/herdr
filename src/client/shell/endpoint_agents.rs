use super::render::put_text;
use super::*;

pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) {
    let rows = agent_rows(endpoints, active_endpoint_id, config);
    for (index, row) in rows.into_iter().take(area.height as usize).enumerate() {
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        if row.agent.focused {
            buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
        }
        let initial = row.machine_label.chars().next().unwrap_or('?');
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!(
                "{initial}{}",
                status_icon(row.agent.status, config.status_indicators)
            ),
            Style::default()
                .fg(if row.stale {
                    config.palette.overlay0
                } else {
                    status_color(row.agent.status, &config.palette)
                })
                .add_modifier(if row.stale {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                }),
        );
        hits.endpoint_agents
            .push((rect, row.endpoint_id, row.agent.pane_id));
    }
}

pub(super) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    if !super::agent_sidebar::render_agent_panel_header(
        buffer,
        area,
        agent_view_label,
        config,
        hits,
    ) {
        return;
    }
    let grouped = agent_view_label.is_none()
        && config.agent_panel_sort == crate::config::AgentPanelSortConfig::Spaces;
    let rows = panel_rows(endpoints, active_endpoint_id, config, grouped);
    super::agent_sidebar::render_agent_list(
        buffer,
        area,
        &rows,
        agent_view_label.map(|_| " no matching agents"),
        config,
        agent_scroll,
        hits,
        |row| super::agent_sidebar::panel_row_height(row, |row| row.agent.rows.len()),
        |buffer, rect, row, hits| {
            super::agent_sidebar::render_panel_row(
                buffer,
                rect,
                row,
                grouped,
                config,
                |buffer, inner, row| {
                    super::agent_sidebar::render_agent_row(buffer, inner, &row.agent, config);
                    if row.stale {
                        buffer.set_style(
                            rect,
                            Style::default()
                                .fg(config.palette.overlay0)
                                .add_modifier(Modifier::DIM),
                        );
                    }
                    hits.endpoint_agents.push((
                        rect,
                        row.endpoint_id.clone(),
                        row.agent.pane_id.clone(),
                    ));
                },
            )
        },
    );
}

/// Agent rows with a heading over each space's agents when `grouped`. With
/// more than one machine, headings name the machine too.
fn panel_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    grouped: bool,
) -> Vec<super::agent_sidebar::PanelRow<EndpointAgentRow>> {
    let multiple_machines = endpoints
        .iter()
        .filter(|endpoint| endpoint.snapshot.is_some())
        .count()
        > 1;
    super::agent_sidebar::with_group_headings(
        agent_rows(endpoints, active_endpoint_id, config),
        |row| {
            if !grouped {
                return None;
            }
            let endpoint = endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == row.endpoint_id)?;
            let workspace = endpoint
                .snapshot
                .as_deref()?
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == row.agent.workspace_id)?;
            let label = if multiple_machines {
                format!("{} · {}", row.machine_label, workspace.label)
            } else {
                workspace.label.clone()
            };
            Some((
                (row.endpoint_id.clone(), row.agent.workspace_id.clone()),
                label,
                workspace.focused && &row.endpoint_id == active_endpoint_id,
            ))
        },
    )
}

impl ClientShellState {
    pub(super) fn reveal_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        body_height: u16,
    ) {
        if body_height == 0 {
            return;
        }
        let grouped = self.config.agent_panel_sort == crate::config::AgentPanelSortConfig::Spaces
            && self
                .endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
                .and_then(|endpoint| endpoint.snapshot.as_deref())
                .is_none_or(|snapshot| snapshot.agent_view_label.is_none());
        let rows = panel_rows(
            &self.endpoints,
            &self.active_endpoint_id,
            &self.config,
            grouped,
        );
        let Some(target) = rows.iter().position(|row| {
            matches!(
                row,
                super::agent_sidebar::PanelRow::Agent(row)
                    if &row.endpoint_id == endpoint_id && row.agent.pane_id == pane_id
            )
        }) else {
            return;
        };
        let heights = rows
            .iter()
            .map(|row| {
                super::agent_sidebar::panel_row_height(row, |row| row.agent.rows.len())
                    .max(1)
                    .min(u16::MAX as usize) as u16
            })
            .collect::<Vec<_>>();
        let mut gaps = vec![self.config.agents.row_gap; rows.len()];
        if let Some(last) = gaps.last_mut() {
            *last = 0;
        }
        self.agent_scroll = super::scroll::list_scroll_start_to_reveal(
            &heights,
            &gaps,
            body_height,
            self.agent_scroll,
            target,
        );
    }
}

struct EndpointAgentRow {
    endpoint_id: ClientEndpointId,
    machine_label: String,
    stale: bool,
    agent: super::agent_sidebar::AgentRow,
}

fn agent_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
) -> Vec<EndpointAgentRow> {
    let mut rendered_rows = endpoints
        .iter()
        .filter_map(|endpoint| {
            endpoint.snapshot.as_deref().map(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .filter_map(|agent| {
                        super::agent_sidebar::agent_row(
                            snapshot,
                            &agent.pane_id,
                            config,
                            Some(&endpoint.label),
                        )
                    })
                    .map(|agent| ((endpoint.endpoint_id.clone(), agent.pane_id.clone()), agent))
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect::<HashMap<_, _>>();

    super::aggregate_navigation::aggregate_agent_rows(
        endpoints,
        active_endpoint_id,
        config.agent_panel_sort,
    )
    .into_iter()
    .filter_map(|row| {
        let key = (row.endpoint.endpoint_id.clone(), row.agent.pane_id.clone());
        let mut agent = rendered_rows.remove(&key)?;
        agent.focused &= row.endpoint.endpoint_id == active_endpoint_id;
        Some(EndpointAgentRow {
            endpoint_id: row.endpoint.endpoint_id.clone(),
            machine_label: row.endpoint.label.to_owned(),
            stale: row.endpoint.stale(),
            agent,
        })
    })
    .collect()
}
