//! Sidebar ↺ picker: reopen a recently closed space, start a new one, or open
//! a folder as a space.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientSpacePickerRow {
    Closed(usize),
    NewSpace,
    OpenFolder,
}

#[derive(Debug, Default)]
pub(super) struct ClientSpacePickerOverlay {
    pub(super) entries: Vec<crate::api::schema::ClosedWorkspaceInfo>,
    pub(super) loading: bool,
    pub(super) query: TextEditor,
    /// Position in `rows()`.
    pub(super) selected: usize,
    pub(super) error: Option<String>,
    pub(super) opening: bool,
    /// The server predates closed-space history.
    pub(super) unsupported: bool,
}

impl ClientSpacePickerOverlay {
    /// Closed spaces matching the search, then the fixed actions.
    pub(super) fn rows(&self) -> Vec<ClientSpacePickerRow> {
        let query = self.query.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty()
                    || format!("{} {}", entry.label, entry.cwd)
                        .to_lowercase()
                        .contains(&query)
            })
            .map(|(index, _)| ClientSpacePickerRow::Closed(index))
            .chain([
                ClientSpacePickerRow::NewSpace,
                ClientSpacePickerRow::OpenFolder,
            ])
            .collect()
    }
}

/// "now", "5m", "3h", "2d", "4w".
pub(super) fn closed_age(closed_at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(closed_at);
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        86_400..=604_799 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

impl ClientShellState {
    pub(super) fn open_space_picker(&mut self, outcome: &mut ClientShellInput) {
        self.overlay = Some(ClientShellOverlay::SpacePicker(ClientSpacePickerOverlay {
            loading: true,
            ..Default::default()
        }));
        outcome.repaint = true;
        let listed = self.push_endpoint_method_with_kind(
            crate::api::schema::Method::WorkspaceClosedList(Default::default()),
            PendingEndpointKind::SpacePickerList,
            outcome,
        );
        if !listed {
            // Older server: new space / open folder still work.
            if let Some(ClientShellOverlay::SpacePicker(picker)) = self.overlay.as_mut() {
                picker.loading = false;
                picker.unsupported = true;
            }
        }
    }

    pub(super) fn insert_space_picker_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::SpacePicker(picker)) = self.overlay.as_mut() else {
            return false;
        };
        if !picker.opening && picker.query.insert(text) {
            picker.selected = 0;
        }
        true
    }

    pub(super) fn route_space_picker_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::SpacePicker(picker)) = self.overlay.as_mut() else {
            return false;
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let control = modifiers == crossterm::event::KeyModifiers::CONTROL;
        outcome.repaint = true;
        match code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.activate_space_picker_row(None, outcome),
            KeyCode::Up => self.move_space_picker_selection(-1),
            KeyCode::Down => self.move_space_picker_selection(1),
            KeyCode::Char('p') if control => self.move_space_picker_selection(-1),
            KeyCode::Char('n') if control => self.move_space_picker_selection(1),
            // Typing edits the search; a changed search restarts at the top.
            _ if !picker.opening && picker.query.handle_key(key) == Some(true) => {
                picker.selected = 0;
            }
            _ => {}
        }
        true
    }

    pub(super) fn move_space_picker_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::SpacePicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        let last = picker.rows().len().saturating_sub(1) as isize;
        picker.selected = (picker.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Activates `position` (a click) or the highlighted row (Enter).
    pub(super) fn activate_space_picker_row(
        &mut self,
        position: Option<usize>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::SpacePicker(picker)) = self.overlay.as_mut() else {
            return;
        };
        if picker.opening {
            return;
        }
        let position = position.unwrap_or(picker.selected);
        let Some(row) = picker.rows().get(position).copied() else {
            return;
        };
        picker.selected = position;
        outcome.repaint = true;
        match row {
            ClientSpacePickerRow::Closed(index) => {
                let Some(closed_id) = picker.entries.get(index).map(|e| e.closed_id.clone()) else {
                    return;
                };
                picker.opening = true;
                picker.error = None;
                let sent = self.push_endpoint_method_with_kind(
                    crate::api::schema::Method::WorkspaceReopen(
                        crate::api::schema::WorkspaceReopenParams {
                            closed_id: Some(closed_id),
                            focus: true,
                        },
                    ),
                    PendingEndpointKind::SpacePickerReopen,
                    outcome,
                );
                if !sent {
                    if let Some(ClientShellOverlay::SpacePicker(picker)) = self.overlay.as_mut() {
                        picker.opening = false;
                    }
                }
            }
            ClientSpacePickerRow::NewSpace => {
                self.overlay = None;
                self.record_binding(
                    crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewWorkspace),
                    outcome,
                );
            }
            // A local desktop dialog only makes sense for a local server.
            ClientSpacePickerRow::OpenFolder if self.active_endpoint_id.is_local() => {
                self.overlay = None;
                outcome.actions.push(ClientShellAction::PickFolder);
            }
            ClientSpacePickerRow::OpenFolder => self.open_folder_overlay(),
        }
    }

    /// The desktop folder picker answered: open the folder as a space, or ask
    /// for a typed path when there is no picker.
    pub(crate) fn receive_folder_pick(
        &mut self,
        pick: crate::platform::FolderPick,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput {
            repaint: true,
            ..Default::default()
        };
        match pick {
            crate::platform::FolderPick::Picked(path) => self.push_endpoint_method(
                crate::api::schema::Method::WorkspaceCreate(
                    crate::api::schema::WorkspaceCreateParams {
                        source_workspace_id: None,
                        cwd: Some(path.to_string_lossy().into_owned()),
                        focus: true,
                        label: None,
                        env: Default::default(),
                    },
                ),
                &mut outcome,
            ),
            crate::platform::FolderPick::Unavailable => self.open_folder_overlay(),
            crate::platform::FolderPick::Cancelled => {}
        }
        outcome
    }

    fn open_folder_overlay(&mut self) {
        // Start from the current space's folder, ready to edit.
        let cwd = self.snapshot.as_deref().and_then(|snapshot| {
            let workspace_id = self.workspace_action_id()?;
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
                .map(|workspace| workspace.new_workspace_cwd.clone())
        });
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "open folder",
            input: TextEditor::new(&cwd.unwrap_or_default(), false),
            target: ClientRenameTarget::OpenFolder,
        }));
    }

    /// Folder typed into "open folder"; `~` means the home directory when the
    /// space runs on this machine.
    pub(super) fn open_folder_path(&self, typed: &str) -> String {
        let typed = typed.trim();
        match typed.strip_prefix('~') {
            Some(rest)
                if self.active_endpoint_id.is_local()
                    && (rest.is_empty() || rest.starts_with('/')) =>
            {
                std::env::var("HOME").map_or_else(|_| typed.to_owned(), |home| home + rest)
            }
            _ => typed.to_owned(),
        }
    }

    pub(super) fn handle_space_picker_result(
        &mut self,
        kind: PendingEndpointKind,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> bool {
        let Some(ClientShellOverlay::SpacePicker(picker)) = self.overlay.as_mut() else {
            return false;
        };
        match (kind, result) {
            (
                PendingEndpointKind::SpacePickerList,
                Ok(crate::api::schema::ResponseResult::ClosedWorkspaceList { workspaces }),
            ) => {
                picker.entries = workspaces;
                picker.loading = false;
                picker.selected = 0;
            }
            (PendingEndpointKind::SpacePickerReopen, Ok(_)) => self.overlay = None,
            (_, Err(error)) => {
                picker.loading = false;
                picker.opening = false;
                picker.error = Some(error.message);
            }
            (_, Ok(_)) => {
                picker.loading = false;
                picker.opening = false;
                picker.error = Some("unexpected response from the server".to_owned());
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::closed_age;

    #[test]
    fn closed_age_uses_the_largest_whole_unit() {
        assert_eq!(closed_age(1_000, 1_030), "now");
        assert_eq!(closed_age(1_000, 1_000 + 5 * 60), "5m");
        assert_eq!(closed_age(1_000, 1_000 + 3 * 3_600 + 59), "3h");
        assert_eq!(closed_age(1_000, 1_000 + 2 * 86_400), "2d");
        assert_eq!(closed_age(1_000, 1_000 + 15 * 86_400), "2w");
        // A server clock ahead of this one must not panic.
        assert_eq!(closed_age(2_000, 1_000), "now");
    }
}
