use std::path::PathBuf;

use crate::api::schema::{AgentStatus, CheckpointInfo, Method, Request, ResponseResult};
use crate::app::App;
use crate::layout::PaneId;

use super::responses::{encode_error, encode_success};

/// An agent starting on a new prompt: idle, done or unknown becoming working.
/// Blocked → working continues the same prompt.
fn starts_prompt(previous: AgentStatus, current: AgentStatus) -> bool {
    current == AgentStatus::Working
        && !matches!(previous, AgentStatus::Working | AgentStatus::Blocked)
}

impl App {
    /// Before an agent works on a new prompt, snapshots its folder in the
    /// background. Blocked → working continues the same prompt, so it is skipped.
    pub(super) fn checkpoint_before_prompt(
        &self,
        ws_idx: usize,
        pane_id: PaneId,
        previous: AgentStatus,
        current: AgentStatus,
        agent: Option<&str>,
    ) {
        if !self.checkpoints_enabled || !starts_prompt(previous, current) {
            return;
        }
        let Some(cwd) = self.launch_cwd_for_pane_in_workspace(ws_idx, pane_id) else {
            return;
        };
        let label = format!("before {}", agent.unwrap_or("agent"));
        let spawned = crate::thread_spawn::spawn_named("herdr-checkpoint", move || {
            // Folders outside git just have no checkpoints.
            if let Err(err) = crate::checkpoint::snapshot(&cwd, &label) {
                tracing::debug!(err = %err, cwd = %cwd.display(), "checkpoint skipped");
            }
        });
        if let Err(err) = spawned {
            tracing::warn!(err = %err, "failed to start checkpoint snapshot");
        }
    }

    /// `checkpoint.list` and `checkpoint.restore` run git off the main loop and
    /// answer `respond_to` when done.
    pub(crate) fn handle_deferred_checkpoint_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let id = request.id;
        let (pane_id, work): (_, Box<dyn FnOnce(PathBuf) -> String + Send>) = match request.method {
            Method::CheckpointList(params) => {
                let id = id.clone();
                (
                    params.pane_id,
                    Box::new(move |cwd| match crate::checkpoint::list(&cwd) {
                        Ok((folder, checkpoints)) => encode_success(
                            id,
                            ResponseResult::CheckpointList {
                                folder: folder.display().to_string(),
                                checkpoints: checkpoints
                                    .into_iter()
                                    .map(|checkpoint| CheckpointInfo {
                                        checkpoint_id: checkpoint.id,
                                        created_at: checkpoint.created_at,
                                        label: checkpoint.label,
                                    })
                                    .collect(),
                            },
                        ),
                        Err(err) => encode_error(id, "not_a_repository", err.to_string()),
                    }),
                )
            }
            Method::CheckpointRestore(params) => {
                let id = id.clone();
                let busy = self.working_agent_folders();
                (
                    params.pane_id,
                    Box::new(move |cwd| {
                        match crate::checkpoint::restore(
                            &cwd,
                            &params.checkpoint_id,
                            &busy,
                            params.force,
                        ) {
                            Ok(undo_checkpoint_id) => encode_success(
                                id,
                                ResponseResult::CheckpointRestored {
                                    checkpoint_id: params.checkpoint_id,
                                    undo_checkpoint_id,
                                },
                            ),
                            Err(crate::checkpoint::RestoreError::NotFound) => {
                                encode_error(id, "checkpoint_not_found", "checkpoint not found")
                            }
                            Err(crate::checkpoint::RestoreError::Busy) => encode_error(
                                id,
                                "agent_working",
                                "an agent is working in this folder; stop it first",
                            ),
                            Err(crate::checkpoint::RestoreError::Git(err)) => {
                                encode_error(id, "restore_failed", err.to_string())
                            }
                        }
                    }),
                )
            }
            _ => return false,
        };
        let cwd = self
            .resolve_optional_pane(pane_id.as_deref())
            .and_then(|(ws_idx, pane)| self.launch_cwd_for_pane_in_workspace(ws_idx, pane));
        let Some(cwd) = cwd else {
            let _ = respond_to.send(encode_error(id, "pane_not_found", "pane not found"));
            return true;
        };
        let worker_respond_to = respond_to.clone();
        let spawned = crate::thread_spawn::spawn_named("herdr-checkpoint-api", move || {
            let _ = worker_respond_to.send(work(cwd));
        });
        if let Err(err) = spawned {
            let _ = respond_to.send(encode_error(id, "spawn_failed", err.to_string()));
        }
        true
    }

    fn working_agent_folders(&self) -> Vec<PathBuf> {
        let mut folders = Vec::new();
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            for pane in workspace.tabs.iter().flat_map(|tab| tab.layout.pane_ids()) {
                let working = self
                    .pane_info(ws_idx, pane)
                    .is_some_and(|info| info.agent_status == AgentStatus::Working);
                if let Some(cwd) = working
                    .then(|| self.launch_cwd_for_pane_in_workspace(ws_idx, pane))
                    .flatten()
                {
                    folders.push(cwd);
                }
            }
        }
        folders
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_new_prompt_starts_a_checkpoint() {
        use AgentStatus::*;
        assert!(starts_prompt(Idle, Working));
        assert!(starts_prompt(Done, Working));
        assert!(starts_prompt(Unknown, Working));
        assert!(
            !starts_prompt(Blocked, Working),
            "approving a prompt mid-turn"
        );
        assert!(!starts_prompt(Working, Working));
        assert!(!starts_prompt(Working, Idle));
    }
}
