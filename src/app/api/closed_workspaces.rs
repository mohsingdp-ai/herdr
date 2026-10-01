//! `workspace.closed.list` / `workspace.reopen`: bring back a closed space.

use crate::api::schema::{ClosedWorkspaceInfo, ResponseResult, WorkspaceReopenParams};
use crate::app::{App, Mode};
use crate::persist::ClosedWorkspace;

use super::responses::{encode_error, encode_success};

impl App {
    /// Snapshot workspaces about to be closed so they can be reopened.
    /// Must run before the close drops their terminals.
    pub(super) fn remember_closed_workspaces(&mut self, indices: &[usize]) {
        let closed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        for &index in indices {
            let Some(workspace) = self.state.workspaces.get(index) else {
                continue;
            };
            let snapshot = crate::persist::capture_workspace(
                workspace,
                &self.state.terminals,
                &self.terminal_runtimes,
            );
            let label = self.workspace_info(index).label;
            // Closing the same space again replaces its older entry.
            self.closed_workspaces.retain(|closed| {
                closed.label != label || closed.workspace.identity_cwd != snapshot.identity_cwd
            });
            self.closed_workspaces.push(ClosedWorkspace {
                label,
                closed_at,
                workspace: snapshot,
            });
        }
        let overflow = self
            .closed_workspaces
            .len()
            .saturating_sub(crate::persist::CLOSED_WORKSPACE_LIMIT);
        self.closed_workspaces.drain(..overflow);
        self.save_closed_workspaces();
    }

    fn save_closed_workspaces(&self) {
        let Some(path) = &self.closed_workspaces_path else {
            return;
        };
        if let Err(err) = crate::persist::save_closed_workspaces(path, &self.closed_workspaces) {
            tracing::warn!(err = %err, "failed to save closed spaces");
        }
    }

    pub(super) fn handle_workspace_closed_list(&mut self, id: String) -> String {
        let workspaces = self
            .closed_workspaces
            .iter()
            .rev()
            .map(|closed| ClosedWorkspaceInfo {
                closed_id: closed.closed_id(),
                label: closed.label.clone(),
                cwd: closed.workspace.identity_cwd.display().to_string(),
                tab_count: closed.workspace.tabs.len(),
                closed_at: closed.closed_at,
            })
            .collect();
        encode_success(id, ResponseResult::ClosedWorkspaceList { workspaces })
    }

    pub(super) fn handle_workspace_reopen(
        &mut self,
        id: String,
        params: WorkspaceReopenParams,
    ) -> String {
        let position = match params.closed_id.as_deref() {
            None => self.closed_workspaces.len().checked_sub(1),
            // Newest match wins if an id was closed twice.
            Some(closed_id) => self
                .closed_workspaces
                .iter()
                .rposition(|closed| closed.closed_id() == closed_id),
        };
        let Some(position) = position else {
            return encode_error(
                id,
                "closed_workspace_not_found",
                match params.closed_id {
                    Some(closed_id) => format!("no closed workspace {closed_id}"),
                    None => "no closed workspaces to reopen".to_string(),
                },
            );
        };
        let mut closed = self.closed_workspaces.remove(position);
        // Workspace ids restart after a server restart, so the old id may be
        // live again; a fresh id avoids two workspaces sharing one.
        if closed
            .workspace
            .id
            .as_ref()
            .is_some_and(|old| self.state.workspaces.iter().any(|ws| &ws.id == old))
        {
            closed.workspace.id = None;
        }
        let ClosedWorkspace {
            label,
            closed_at,
            workspace,
        } = closed;
        let mut snapshot = crate::persist::single_workspace_session(workspace);
        let (rows, cols) = self.state.estimate_pane_size();
        let (mut workspaces, terminals, runtimes) = crate::persist::restore(
            &snapshot,
            None,
            rows,
            cols,
            self.state.pane_scrollback_limit_bytes,
            &self.state.default_shell,
            self.state.shell_mode,
            // Reopening asks for the space back, agents included.
            true,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        );
        let Some(workspace) = workspaces.pop() else {
            // Keep it listed so a later retry (e.g. once the folder exists) can work.
            if let Some(workspace) = snapshot.workspaces.pop() {
                self.closed_workspaces.insert(
                    position,
                    ClosedWorkspace {
                        label,
                        closed_at,
                        workspace,
                    },
                );
            }
            return encode_error(
                id,
                "workspace_reopen_failed",
                "could not restore the closed workspace",
            );
        };
        for (terminal_id, runtime) in runtimes {
            runtime.apply_host_terminal_theme(self.state.host_terminal_theme);
            runtime.apply_host_terminal_appearance(self.state.host_terminal_appearance);
            self.terminal_runtimes.insert(terminal_id, runtime);
        }
        self.state.terminals.extend(terminals);
        for pane_id in workspace.tabs.iter().flat_map(|tab| tab.layout.pane_ids()) {
            self.state.remove_alias_shadowed_by_new_pane(pane_id);
        }
        self.state.workspaces.push(workspace);
        let index = self.state.workspaces.len() - 1;
        if params.focus || self.state.active.is_none() {
            self.state.switch_workspace(index);
            self.state.mode = Mode::Terminal;
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.sync_pending_agent_resume_deadline(std::time::Instant::now());
        self.save_closed_workspaces();
        self.emit_workspace_open_events(index);
        match self.workspace_created_result(index) {
            Some(result) => encode_success(id, result),
            None => encode_error(
                id,
                "workspace_reopen_failed",
                "reopened workspace has no tabs",
            ),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::api::schema::{SuccessResponse, WorkspaceCloseParams};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn result(response: &str) -> ResponseResult {
        serde_json::from_str::<SuccessResponse>(response)
            .unwrap_or_else(|err| panic!("{err}: {response}"))
            .result
    }

    fn error_code(response: &str) -> String {
        let response: serde_json::Value = serde_json::from_str(response).unwrap();
        response["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn closed_workspace_reopens_under_a_fresh_id_when_its_old_id_is_live() {
        let mut app = test_app();
        let cwd = std::env::temp_dir();
        app.create_workspace_with_options(cwd.clone(), true)
            .unwrap();
        let closing = app.create_workspace_with_options(cwd, false).unwrap();
        app.state.workspaces[closing].set_custom_name("scratch".into());
        let closed_internal_id = app.state.workspaces[closing].id.clone();
        let workspace_id = app.public_workspace_id(closing);
        app.handle_workspace_close(
            "close".into(),
            WorkspaceCloseParams {
                workspace_id,
                close_group: false,
            },
        );
        assert_eq!(app.state.workspaces.len(), 1);

        let ResponseResult::ClosedWorkspaceList { workspaces } =
            result(&app.handle_workspace_closed_list("list".into()))
        else {
            panic!("expected a closed workspace list");
        };
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].label, "scratch");
        assert_eq!(workspaces[0].tab_count, 1);
        assert_eq!(workspaces[0].closed_id, closed_internal_id);

        // As after a server restart: a live workspace now holds the old id.
        app.state.workspaces[0].id = closed_internal_id.clone();
        let ResponseResult::WorkspaceCreated { workspace, .. } =
            result(&app.handle_workspace_reopen(
                "reopen".into(),
                WorkspaceReopenParams {
                    closed_id: Some(closed_internal_id.clone()),
                    focus: true,
                },
            ))
        else {
            panic!("expected the reopened workspace");
        };
        assert_eq!(workspace.label, "scratch");
        assert_eq!(app.state.workspaces.len(), 2);
        assert_ne!(app.state.workspaces[1].id, closed_internal_id);
        assert_eq!(app.state.active, Some(1));
        assert!(app.closed_workspaces.is_empty());
        app.state.assert_invariants_for_test();

        assert_eq!(
            error_code(&app.handle_workspace_reopen(
                "again".into(),
                WorkspaceReopenParams {
                    closed_id: None,
                    focus: true,
                },
            )),
            "closed_workspace_not_found"
        );
    }

    #[tokio::test]
    async fn closed_list_keeps_the_newest_and_lists_them_first() {
        let mut app = test_app();
        app.create_workspace_with_options(std::env::temp_dir(), true)
            .unwrap();
        let total = crate::persist::CLOSED_WORKSPACE_LIMIT + 2;
        for number in 0..total {
            app.state.workspaces[0].set_custom_name(format!("space {number}"));
            app.remember_closed_workspaces(&[0]);
        }
        assert_eq!(
            app.closed_workspaces.len(),
            crate::persist::CLOSED_WORKSPACE_LIMIT
        );
        let ResponseResult::ClosedWorkspaceList { workspaces } =
            result(&app.handle_workspace_closed_list("list".into()))
        else {
            panic!("expected a closed workspace list");
        };
        assert_eq!(workspaces[0].label, format!("space {}", total - 1));
        assert_eq!(
            workspaces.last().map(|closed| closed.label.as_str()),
            Some("space 2")
        );

        // Closing the same space again moves it to the top instead of repeating it.
        app.state.workspaces[0].set_custom_name("space 5".into());
        app.remember_closed_workspaces(&[0]);
        let ResponseResult::ClosedWorkspaceList { workspaces } =
            result(&app.handle_workspace_closed_list("list".into()))
        else {
            panic!("expected a closed workspace list");
        };
        assert_eq!(workspaces[0].label, "space 5");
        assert_eq!(
            workspaces
                .iter()
                .filter(|closed| closed.label == "space 5")
                .count(),
            1
        );
        assert_eq!(workspaces.len(), crate::persist::CLOSED_WORKSPACE_LIMIT);
    }
}
