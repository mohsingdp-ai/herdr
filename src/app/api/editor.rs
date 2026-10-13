use std::path::PathBuf;

use crate::api::schema::{PaneTarget, PaneToggleEditorParams, ResponseResult};
use crate::app::actions::PaneZoomCommand;
use crate::app::App;
use crate::layout::PaneId;
use ratatui::layout::Direction;

use super::responses::{encode_error, encode_success};
use crate::app::creation::launch_cwd_for_terminal;

const EDITOR_PANE_LABEL: &str = "editor";

impl App {
    /// Flips an agent pane and its editor. Each agent gets an editor pane in its
    /// own tab, running `$EDITOR .` in the agent's folder, zoomed while shown.
    /// Editors are matched to agents by folder, so agents sharing a folder share
    /// one editor. Restore brings an editor pane back as an idle shell that keeps
    /// its "editor" label; that slot is replaced by a fresh editor on next use.
    pub(super) fn handle_pane_toggle_editor(
        &mut self,
        id: String,
        params: PaneToggleEditorParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.resolve_optional_pane(params.pane_id.as_deref()) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let editor_argv = match crate::platform::project_editor_argv() {
            Ok(argv) => argv,
            Err(err) => return encode_error(id, "editor_unavailable", err.to_string()),
        };
        let Some(tab_idx) = self.state.workspaces[ws_idx].find_tab_index_for_pane(pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let panes = self.state.workspaces[ws_idx].tabs[tab_idx]
            .layout
            .pane_ids();
        // Some(true): a running editor. Some(false): a restored editor slot,
        // i.e. an idle shell still labeled "editor", safe to replace.
        let editor_state = |app: &Self, pane: PaneId| -> Option<bool> {
            let terminal = app.state.workspaces[ws_idx].tabs[tab_idx]
                .terminal_id(pane)
                .and_then(|terminal_id| app.state.terminals.get(terminal_id))?;
            if terminal.launch_argv.as_ref() == Some(&editor_argv) {
                return Some(true);
            }
            let restored_slot = terminal.launch_argv.is_none()
                && terminal.manual_label.as_deref() == Some(EDITOR_PANE_LABEL)
                && app
                    .state
                    .runtime_for_pane_in_workspace(&app.terminal_runtimes, ws_idx, pane)
                    .and_then(|runtime| runtime.child_pid())
                    .is_some_and(crate::detect::pane_shell_is_idle);
            restored_slot.then_some(false)
        };
        let is_editor = |app: &Self, pane: PaneId| editor_state(app, pane).is_some();
        let folder = self.pane_folder(ws_idx, tab_idx, pane_id);
        let same_folder = |app: &Self, pane: PaneId| {
            folder.is_some() && app.pane_folder(ws_idx, tab_idx, pane) == folder
        };

        let target = if is_editor(self, pane_id) {
            // Back to the agent: the one focused before if it shares this folder,
            // else any pane in this folder, else any non-editor pane.
            let previous = self
                .state
                .previous_pane_focus
                .as_ref()
                .filter(|focus| focus.workspace_id == self.state.workspaces[ws_idx].id)
                .map(|focus| focus.pane_id)
                .filter(|pane| panes.contains(pane) && !is_editor(self, *pane));
            previous
                .filter(|&pane| same_folder(self, pane))
                .or_else(|| {
                    panes
                        .iter()
                        .copied()
                        .find(|&pane| !is_editor(self, pane) && same_folder(self, pane))
                })
                .or(previous)
                .or_else(|| panes.iter().copied().find(|&pane| !is_editor(self, pane)))
        } else {
            let in_folder = |live| {
                panes
                    .iter()
                    .copied()
                    .find(|&pane| editor_state(self, pane) == Some(live) && same_folder(self, pane))
            };
            if let Some(editor) = in_folder(true) {
                Some(editor)
            } else {
                if let Some(slot) = in_folder(false) {
                    let slot_id = self.public_pane_id(ws_idx, slot).unwrap_or_default();
                    if let Err(response) =
                        self.close_pane(id.clone(), &PaneTarget { pane_id: slot_id })
                    {
                        return response;
                    }
                }
                match self.split_editor_pane(ws_idx, pane_id, folder, &editor_argv) {
                    Ok(editor) => Some(editor),
                    Err(err) => return encode_error(id, "editor_open_failed", err.to_string()),
                }
            }
        };
        let Some(target) = target else {
            // A lone editor with no agent left in its tab: nothing to go back to.
            return encode_error(id, "pane_not_found", "no agent pane to return to");
        };

        self.state
            .apply_pane_zoom(ws_idx, target, PaneZoomCommand::On);
        self.state.mode = crate::app::Mode::Terminal;
        self.schedule_session_save();
        self.emit_layout_updated_event(ws_idx, tab_idx);
        match self.pane_info(ws_idx, target) {
            Some(pane) => encode_success(id, ResponseResult::PaneInfo { pane }),
            None => encode_error(id, "pane_not_found", "pane disappeared"),
        }
    }

    fn pane_folder(&self, ws_idx: usize, tab_idx: usize, pane_id: PaneId) -> Option<PathBuf> {
        let terminal_id = self.state.workspaces[ws_idx].tabs[tab_idx].terminal_id(pane_id)?;
        let cwd =
            launch_cwd_for_terminal(terminal_id, &self.state.terminals, &self.terminal_runtimes)?;
        Some(crate::worktree::canonical_or_original(&cwd))
    }

    fn split_editor_pane(
        &mut self,
        ws_idx: usize,
        agent_pane: PaneId,
        folder: Option<PathBuf>,
        argv: &[String],
    ) -> std::io::Result<PaneId> {
        let (rows, cols) = self.state.estimate_pane_size();
        let scrollback_limit_bytes = self.state.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let (_, new_pane) = self.state.workspaces[ws_idx]
            .split_pane_argv_command(
                agent_pane,
                Direction::Horizontal,
                rows.max(4),
                cols.max(10),
                folder,
                argv,
                Vec::new(),
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                false,
            )
            .ok_or_else(|| std::io::Error::other("agent pane disappeared"))??;
        let mut terminal = new_pane.terminal;
        terminal.set_manual_label(EDITOR_PANE_LABEL.to_string());
        let terminal_id = terminal.id.clone();
        self.terminal_runtimes
            .insert(terminal_id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state.terminals.insert(terminal_id, terminal);
        if let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) {
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::PaneCreated,
                data: crate::api::schema::EventData::PaneCreated { pane },
            });
        }
        Ok(new_pane.pane_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::shutdown_test_runtimes;
    use super::*;
    use crate::{config::Config, workspace::Workspace};

    #[tokio::test]
    async fn toggle_editor_flips_agent_and_its_editor_without_duplicates() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let workspace = Workspace::test_new("editor");
        let agent = workspace.tabs[0].root_pane;
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0].terminal_id(agent).cloned().unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = std::env::temp_dir();
        let toggle = |app: &mut App| {
            app.handle_pane_toggle_editor("req".into(), PaneToggleEditorParams::default());
            app.state.workspaces[0].focused_pane_id().unwrap()
        };

        let editor = toggle(&mut app);
        assert_ne!(editor, agent, "opens an editor pane");
        assert!(
            app.state.workspaces[0].tabs[0].zoomed,
            "editor fills the tab"
        );
        assert_eq!(toggle(&mut app), agent, "returns to the agent");
        assert_eq!(toggle(&mut app), editor, "reuses the same editor");
        assert_eq!(app.state.workspaces[0].tabs[0].layout.pane_count(), 2);
        shutdown_test_runtimes(&mut app);
    }
}
