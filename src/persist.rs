//! Session persistence — save/restore workspaces, layouts, and working directories.
//!
//! Stored at `~/.config/herdr/session.json`.
//! Optional pane screen history is stored separately at `session-history.json`.
//! Installed plugins are persisted separately at `plugins.json`.
//! Recently closed workspaces are kept in `closed-spaces.json`.

mod closed;
mod io;
pub mod plugin_registry;
mod restore;
mod snapshot;
mod writer;

pub(crate) use self::closed::{
    closed_workspaces_path, load_closed_workspaces, save_closed_workspaces,
    single_workspace_session, ClosedWorkspace, CLOSED_WORKSPACE_LIMIT,
};
pub use self::io::{clear_history, load, load_history};
pub use self::restore::restore;
#[cfg(unix)]
pub use self::restore::{handoff_pane_aliases, restore_handoff};
pub(crate) use self::snapshot::capture_workspace;
pub use self::snapshot::{
    capture, capture_history, DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot,
    SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
};
pub(crate) use self::writer::SessionWriter;
