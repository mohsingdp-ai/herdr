//! Rewind picker: a folder's checkpoints (hidden git snapshots taken before an
//! agent's prompt). Choosing one puts the folder's files back to it.

use super::*;

#[derive(Debug, Default)]
pub(super) struct ClientCheckpointPickerOverlay {
    /// A pane in the folder being rewound.
    pub(super) pane_id: String,
    pub(super) folder: String,
    pub(super) entries: Vec<crate::api::schema::CheckpointInfo>,
    pub(super) loading: bool,
    pub(super) selected: usize,
    pub(super) error: Option<String>,
    pub(super) notice: Option<String>,
    pub(super) restoring: bool,
    /// The server predates checkpoints.
    pub(super) unsupported: bool,
}

impl ClientShellState {
    pub(super) fn open_checkpoint_picker(
        &mut self,
        pane_id: String,
        outcome: &mut ClientShellInput,
    ) {
        self.overlay = Some(ClientShellOverlay::CheckpointPicker(
            ClientCheckpointPickerOverlay {
                pane_id: pane_id.clone(),
                loading: true,
                ..Default::default()
            },
        ));
        outcome.repaint = true;
        self.request_checkpoint_list(pane_id, outcome);
    }

    fn request_checkpoint_list(&mut self, pane_id: String, outcome: &mut ClientShellInput) {
        let listed = self.push_endpoint_method_with_kind(
            crate::api::schema::Method::CheckpointList(crate::api::schema::CheckpointListParams {
                pane_id: Some(pane_id),
            }),
            PendingEndpointKind::CheckpointList,
            outcome,
        );
        if !listed {
            if let Some(ClientShellOverlay::CheckpointPicker(picker)) = self.overlay.as_mut() {
                picker.loading = false;
                picker.unsupported = true;
            }
        }
    }

    pub(super) fn route_checkpoint_picker_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::CheckpointPicker(_))) {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let control = modifiers == crossterm::event::KeyModifiers::CONTROL;
        outcome.repaint = true;
        match code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.activate_checkpoint_picker_row(None, outcome),
            KeyCode::Up | KeyCode::Char('k') => self.move_checkpoint_picker_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_checkpoint_picker_selection(1),
            KeyCode::Char('p') if control => self.move_checkpoint_picker_selection(-1),
            KeyCode::Char('n') if control => self.move_checkpoint_picker_selection(1),
            _ => {}
        }
        true
    }

    pub(super) fn move_checkpoint_picker_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::CheckpointPicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        let last = picker.entries.len().saturating_sub(1) as isize;
        picker.selected = (picker.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Rewinds to `position` (a click) or the highlighted checkpoint (Enter).
    pub(super) fn activate_checkpoint_picker_row(
        &mut self,
        position: Option<usize>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::CheckpointPicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        if picker.restoring || picker.loading {
            return;
        }
        let position = position.unwrap_or(picker.selected);
        let Some(checkpoint_id) = picker
            .entries
            .get(position)
            .map(|entry| entry.checkpoint_id.clone())
        else {
            return;
        };
        picker.selected = position;
        picker.restoring = true;
        picker.error = None;
        picker.notice = None;
        let pane_id = picker.pane_id.clone();
        outcome.repaint = true;
        let sent = self.push_endpoint_method_with_kind(
            crate::api::schema::Method::CheckpointRestore(
                crate::api::schema::CheckpointRestoreParams {
                    pane_id: Some(pane_id),
                    checkpoint_id,
                    force: false,
                },
            ),
            PendingEndpointKind::CheckpointRestore,
            outcome,
        );
        if !sent {
            if let Some(ClientShellOverlay::CheckpointPicker(picker)) = self.overlay.as_mut() {
                picker.restoring = false;
            }
        }
    }

    /// Returns whether to repaint and any follow-up endpoint actions.
    pub(super) fn handle_checkpoint_picker_result(
        &mut self,
        kind: PendingEndpointKind,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let Some(ClientShellOverlay::CheckpointPicker(picker)) = self.overlay.as_mut() else {
            return (false, Vec::new());
        };
        match (kind, result) {
            (
                PendingEndpointKind::CheckpointList,
                Ok(crate::api::schema::ResponseResult::CheckpointList {
                    folder,
                    checkpoints,
                }),
            ) => {
                picker.folder = folder;
                picker.entries = checkpoints;
                picker.loading = false;
                picker.selected = 0;
            }
            (PendingEndpointKind::CheckpointRestore, Ok(_)) => {
                // Stay open on the refreshed list, whose newest entry undoes this.
                picker.restoring = false;
                picker.loading = true;
                picker.notice = Some("rewound · the top entry undoes it".to_owned());
                let pane_id = picker.pane_id.clone();
                let mut outcome = ClientShellInput::default();
                self.request_checkpoint_list(pane_id, &mut outcome);
                return (true, outcome.actions);
            }
            (_, Err(error)) => {
                picker.loading = false;
                picker.restoring = false;
                picker.error = Some(error.message);
            }
            (_, Ok(_)) => {
                picker.loading = false;
                picker.restoring = false;
                picker.error = Some("unexpected response from the server".to_owned());
            }
        }
        (true, Vec::new())
    }

    /// The pane a space's rewind or git action targets: its agent, else its
    /// focused or first pane.
    pub(super) fn workspace_folder_pane(&self, workspace_id: &str) -> Option<String> {
        let snapshot = self.snapshot.as_deref()?;
        snapshot
            .agents
            .iter()
            .find(|agent| agent.workspace_id == workspace_id)
            .map(|agent| agent.pane_id.clone())
            .or_else(|| {
                let panes = || {
                    snapshot
                        .panes
                        .iter()
                        .filter(|pane| pane.workspace_id == workspace_id)
                };
                panes()
                    .find(|pane| pane.focused)
                    .or_else(|| panes().next())
                    .map(|pane| pane.pane_id.clone())
            })
    }
}
