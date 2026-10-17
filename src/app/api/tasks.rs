use std::path::PathBuf;

use crate::api::schema::{EventData, EventEnvelope, EventKind, Method, Request, ResponseResult};
use crate::app::App;
use crate::tasks::{self, TaskError};

use super::responses::{encode_error, encode_success};

type TaskWork = Box<dyn FnOnce(PathBuf) -> Result<ResponseResult, TaskError> + Send>;

/// The `task.*` methods, which run Backlog.md off the main loop.
pub(crate) fn is_task_method(method: &Method) -> bool {
    matches!(
        method,
        Method::TaskList(_)
            | Method::TaskGet(_)
            | Method::TaskCreate(_)
            | Method::TaskUpdate(_)
            | Method::TaskInit(_)
            | Method::TaskStamp(_)
            | Method::TaskArchive(_)
            | Method::TaskAddWorkflow(_)
    )
}

fn detail(task: crate::api::schema::TaskDetail) -> ResponseResult {
    ResponseResult::TaskDetail { task }
}

fn board(board: crate::api::schema::TaskBoardInfo) -> ResponseResult {
    ResponseResult::TaskList { board }
}

impl App {
    /// Answers a `task.*` request on a worker thread: each `backlog` call
    /// takes about half a second and must never stall the server loop.
    pub(crate) fn handle_deferred_task_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let id = request.id;
        let writes = matches!(
            request.method,
            Method::TaskCreate(_)
                | Method::TaskUpdate(_)
                | Method::TaskInit(_)
                | Method::TaskArchive(_)
                | Method::TaskAddWorkflow(_)
        );
        // An agent's request (its pane runs a detected agent) follows the
        // workflow's agent rules; the Board and a person's shell do not.
        let agent = self.caller_agent(&request.method);
        let (workspace_id, pane_id, work): (Option<String>, Option<String>, TaskWork) =
            match request.method {
                Method::TaskList(target) => (
                    target.workspace_id,
                    target.pane_id,
                    Box::new(|folder| tasks::list(&folder).map(board)),
                ),
                Method::TaskInit(target) => (
                    target.workspace_id,
                    target.pane_id,
                    Box::new(|folder| tasks::init(&folder).map(board)),
                ),
                Method::TaskStamp(target) => (
                    target.workspace_id,
                    target.pane_id,
                    Box::new(|folder| {
                        tasks::stamp(&folder).map(|stamp| ResponseResult::TaskStamp { stamp })
                    }),
                ),
                Method::TaskGet(params) => (
                    params.workspace_id,
                    params.pane_id,
                    Box::new(move |folder| tasks::get(&folder, &params.id).map(detail)),
                ),
                Method::TaskCreate(params) => (
                    params.workspace_id.clone(),
                    params.pane_id.clone(),
                    Box::new(move |folder| {
                        tasks::create(&folder, &params, agent.as_deref()).map(detail)
                    }),
                ),
                Method::TaskUpdate(params) => (
                    params.workspace_id.clone(),
                    params.pane_id.clone(),
                    Box::new(move |folder| {
                        tasks::update(&folder, &params, agent.as_deref()).map(detail)
                    }),
                ),
                Method::TaskArchive(params) => (
                    params.workspace_id,
                    params.pane_id,
                    Box::new(move |folder| tasks::archive(&folder, &params.id).map(board)),
                ),
                Method::TaskAddWorkflow(target) => (
                    target.workspace_id,
                    target.pane_id,
                    Box::new(|folder| tasks::add_workflow(&folder).map(board)),
                ),
                _ => {
                    let _ =
                        respond_to.send(encode_error(id, "invalid_request", "not a task method"));
                    return;
                }
            };
        let (folder, workspace_index) =
            match self.task_folder(workspace_id.as_deref(), pane_id.as_deref()) {
                Ok(found) => found,
                Err((code, message)) => {
                    let _ = respond_to.send(encode_error(id, code, message));
                    return;
                }
            };
        // Writes tell subscribers which space's tasks changed.
        let changed = writes.then(|| {
            (
                self.event_hub.clone(),
                self.public_workspace_id(workspace_index),
            )
        });
        let worker_respond_to = respond_to.clone();
        let worker_id = id.clone();
        let spawned = crate::thread_spawn::spawn_named("herdr-task-api", move || {
            let response = match work(folder) {
                Ok(result) => {
                    if let Some((hub, workspace_id)) = changed {
                        let task_id = match &result {
                            ResponseResult::TaskDetail { task } => Some(task.summary.id.clone()),
                            _ => None,
                        };
                        // Straight to the hub from this thread: no plugin hooks run for it.
                        hub.push(EventEnvelope {
                            event: EventKind::TasksChanged,
                            data: EventData::TasksChanged {
                                workspace_id,
                                task_id,
                            },
                        });
                    }
                    encode_success(worker_id, result)
                }
                Err(err) => encode_error(worker_id, err.code(), err.message()),
            };
            let _ = worker_respond_to.send(response);
        });
        if let Err(err) = spawned {
            let _ = respond_to.send(encode_error(id, "spawn_failed", err.to_string()));
        }
    }

    /// The agent running in the pane a request names, if any. Only a pane
    /// named by the request counts, never the focused one.
    fn caller_agent(&self, method: &Method) -> Option<String> {
        let pane_id = match method {
            Method::TaskCreate(params) => params.pane_id.as_deref(),
            Method::TaskUpdate(params) => params.pane_id.as_deref(),
            _ => None,
        }?;
        let (workspace_index, pane) = self.resolve_optional_pane(Some(pane_id))?;
        self.pane_metadata(workspace_index, pane)?.agent
    }

    /// Tasks belong to a space: a pane's tasks are its workspace's, so an
    /// agent and the Board always see the same project.
    fn task_folder(
        &self,
        workspace_id: Option<&str>,
        pane_id: Option<&str>,
    ) -> Result<(PathBuf, usize), (&'static str, String)> {
        let workspace_index = match workspace_id {
            Some(workspace_id) => self.parse_workspace_id(workspace_id).ok_or_else(|| {
                (
                    "workspace_not_found",
                    format!("workspace {workspace_id} not found"),
                )
            })?,
            None => {
                self.resolve_optional_pane(pane_id)
                    .ok_or_else(|| ("pane_not_found", "pane not found".to_owned()))?
                    .0
            }
        };
        let workspace = self
            .state
            .workspaces
            .get(workspace_index)
            .ok_or_else(|| ("workspace_not_found", "workspace not found".to_owned()))?;
        workspace
            .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
            .map(|folder| (folder, workspace_index))
            .ok_or_else(|| {
                (
                    "backlog_not_initialized",
                    "workspace has no folder".to_owned(),
                )
            })
    }
}
