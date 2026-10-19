use crate::api::schema::{Method, Request, ResponseResult};
use crate::app::App;
use crate::notes::{NoteError, Vault};

use super::responses::{encode_error, encode_success};

type NoteWork = Box<dyn FnOnce(Vault) -> Result<ResponseResult, NoteError> + Send>;

/// The `note.*` methods, which read and write files off the main loop.
pub(crate) fn is_note_method(method: &Method) -> bool {
    matches!(
        method,
        Method::NoteList(_)
            | Method::NoteStamp(_)
            | Method::NoteWrite(_)
            | Method::NoteRename(_)
            | Method::NoteDelete(_)
    )
}

fn vault(vault: crate::api::schema::NoteVault) -> ResponseResult {
    ResponseResult::NoteList { vault }
}

impl App {
    /// Answers a `note.*` request on a worker thread: a vault can hold
    /// thousands of files, and disk work must never stall the server loop.
    pub(crate) fn handle_deferred_note_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) {
        let id = request.id;
        let (workspace_id, pane_id, work): (Option<String>, Option<String>, NoteWork) =
            match request.method {
                Method::NoteList(target) => (
                    target.workspace_id,
                    target.pane_id,
                    Box::new(|notes| Ok(vault(notes.list()))),
                ),
                Method::NoteStamp(target) => (
                    target.workspace_id,
                    target.pane_id,
                    Box::new(|notes| {
                        Ok(ResponseResult::NoteStamp {
                            stamp: notes.stamp(),
                        })
                    }),
                ),
                Method::NoteWrite(params) => (
                    params.workspace_id.clone(),
                    params.pane_id.clone(),
                    Box::new(move |notes| {
                        let note = notes.write(
                            &params.path,
                            &params.content,
                            params.base_fingerprint.as_deref(),
                        )?;
                        Ok(ResponseResult::NoteSaved {
                            note,
                            stamp: notes.stamp(),
                        })
                    }),
                ),
                Method::NoteRename(params) => (
                    params.workspace_id.clone(),
                    params.pane_id.clone(),
                    Box::new(move |notes| notes.rename(&params.from, &params.to).map(vault)),
                ),
                Method::NoteDelete(params) => (
                    params.workspace_id.clone(),
                    params.pane_id.clone(),
                    Box::new(move |notes| notes.delete(&params.path).map(vault)),
                ),
                _ => {
                    let _ =
                        respond_to.send(encode_error(id, "invalid_request", "not a note method"));
                    return;
                }
            };
        let folder = match self.space_folder(
            workspace_id.as_deref(),
            pane_id.as_deref(),
            "note_no_folder",
        ) {
            Ok((folder, _)) => folder,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(id, code, message));
                return;
            }
        };
        let worker_respond_to = respond_to.clone();
        let worker_id = id.clone();
        let spawned = crate::thread_spawn::spawn_named("herdr-note-api", move || {
            let response = match work(Vault::for_folder(&folder)) {
                Ok(result) => encode_success(worker_id, result),
                Err(err) => encode_error(worker_id, err.code(), err.message()),
            };
            let _ = worker_respond_to.send(response);
        });
        if let Err(err) = spawned {
            let _ = respond_to.send(encode_error(id, "spawn_failed", err.to_string()));
        }
    }
}
