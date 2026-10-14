use std::path::PathBuf;

use crate::api::schema::{Method, Request, ResponseResult};
use crate::app::App;
use crate::source_control::{self, GitError};

use super::responses::{encode_error, encode_success};

type GitWork = Box<dyn FnOnce(PathBuf) -> Result<ResponseResult, GitError> + Send>;

/// The `git.*` methods, which run git off the main loop.
pub(crate) fn is_git_method(method: &Method) -> bool {
    matches!(
        method,
        Method::GitStatus(_)
            | Method::GitDiff(_)
            | Method::GitLog(_)
            | Method::GitShow(_)
            | Method::GitStage(_)
            | Method::GitUnstage(_)
            | Method::GitDiscard(_)
            | Method::GitCommit(_)
            | Method::GitUncommit(_)
            | Method::GitPush(_)
            | Method::GitPull(_)
    )
}

fn status(status: crate::api::schema::GitStatusInfo) -> ResponseResult {
    ResponseResult::GitStatus { status }
}

impl App {
    /// Answers a `git.*` request on a worker thread: git can take a while
    /// (hooks, the network) and must never stall the server loop.
    pub(crate) fn handle_deferred_git_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let id = request.id;
        let (workspace_id, pane_id, work): (Option<String>, Option<String>, GitWork) = match request
            .method
        {
            Method::GitStatus(target) => (
                target.workspace_id,
                target.pane_id,
                Box::new(|cwd| source_control::status(&cwd).map(status)),
            ),
            Method::GitDiff(params) => (
                params.workspace_id,
                params.pane_id,
                Box::new(move |cwd| {
                    source_control::diff(
                        &cwd,
                        &params.path,
                        params.orig_path.as_deref(),
                        params.commit.as_deref(),
                    )
                    .map(|diff| ResponseResult::GitDiff { diff })
                }),
            ),
            Method::GitLog(params) => (
                params.workspace_id,
                params.pane_id,
                Box::new(move |cwd| {
                    source_control::log(&cwd, params.limit)
                        .map(|(commits, has_more)| ResponseResult::GitLog { commits, has_more })
                }),
            ),
            Method::GitShow(params) => (
                params.workspace_id,
                params.pane_id,
                Box::new(move |cwd| {
                    source_control::show(&cwd, &params.commit).map(|files| {
                        ResponseResult::GitCommitFiles {
                            commit: params.commit,
                            files,
                        }
                    })
                }),
            ),
            Method::GitStage(params) => (
                params.workspace_id,
                params.pane_id,
                Box::new(move |cwd| {
                    let (root, hunk) = (params.root.as_deref(), params.hunk.as_deref());
                    source_control::stage(&cwd, root, &params.paths, hunk).map(status)
                }),
            ),
            Method::GitUnstage(params) => (
                params.workspace_id,
                params.pane_id,
                Box::new(move |cwd| {
                    let (root, hunk) = (params.root.as_deref(), params.hunk.as_deref());
                    source_control::unstage(&cwd, root, &params.paths, hunk).map(status)
                }),
            ),
            Method::GitDiscard(params) => (
                params.workspace_id,
                params.pane_id,
                Box::new(move |cwd| {
                    source_control::discard(&cwd, params.root.as_deref(), &params.paths).map(status)
                }),
            ),
            Method::GitCommit(params) => (
                params.workspace_id,
                params.pane_id,
                Box::new(move |cwd| {
                    let root = params.root.as_deref();
                    source_control::commit(&cwd, root, &params.message, params.all).map(status)
                }),
            ),
            Method::GitUncommit(params) => (
                params.workspace_id,
                params.pane_id,
                Box::new(move |cwd| {
                    source_control::uncommit(&cwd, params.root.as_deref(), &params.commit)
                        .map(status)
                }),
            ),
            Method::GitPush(target) => (
                target.workspace_id,
                target.pane_id,
                Box::new(move |cwd| source_control::push(&cwd, target.root.as_deref()).map(status)),
            ),
            Method::GitPull(target) => (
                target.workspace_id,
                target.pane_id,
                Box::new(move |cwd| source_control::pull(&cwd, target.root.as_deref()).map(status)),
            ),
            _ => {
                let _ = respond_to.send(encode_error(id, "invalid_request", "not a git method"));
                return;
            }
        };
        let cwd = match self.git_folder(workspace_id.as_deref(), pane_id.as_deref()) {
            Ok(cwd) => cwd,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };
        let worker_respond_to = respond_to.clone();
        let worker_id = id.clone();
        let spawned = crate::thread_spawn::spawn_named("herdr-git-api", move || {
            let response = match work(cwd) {
                Ok(result) => encode_success(worker_id, result),
                Err(err) => encode_error(worker_id, err.code(), err.message()),
            };
            let _ = worker_respond_to.send(response);
        });
        if let Err(err) = spawned {
            let _ = respond_to.send(encode_error(id, "spawn_failed", err.to_string()));
        }
    }

    /// A workspace's folder is the one its sidebar git status comes from, so
    /// the git view and the sidebar always describe the same checkout.
    fn git_folder(
        &self,
        workspace_id: Option<&str>,
        pane_id: Option<&str>,
    ) -> Result<PathBuf, (&'static str, String)> {
        if let Some(workspace_id) = workspace_id {
            let workspace = self
                .parse_workspace_id(workspace_id)
                .and_then(|index| self.state.workspaces.get(index))
                .ok_or_else(|| {
                    (
                        "workspace_not_found",
                        format!("workspace {workspace_id} not found"),
                    )
                })?;
            return workspace
                .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                .ok_or_else(|| ("not_a_repository", "workspace has no folder".to_owned()));
        }
        self.resolve_optional_pane(pane_id)
            .and_then(|(ws_idx, pane)| self.launch_cwd_for_pane_in_workspace(ws_idx, pane))
            .ok_or_else(|| ("pane_not_found", "pane not found".to_owned()))
    }
}
