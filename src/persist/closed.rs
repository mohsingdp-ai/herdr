//! Recently closed workspaces, kept in `closed-spaces.json` beside `session.json`
//! so a closed space can be reopened with its tabs, layout and agent sessions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::snapshot::{SessionSnapshot, WorkspaceSnapshot, SNAPSHOT_VERSION};

/// How many closed workspaces to remember; the oldest drops off first.
pub(crate) const CLOSED_WORKSPACE_LIMIT: usize = 10;

#[derive(Serialize, Deserialize)]
pub(crate) struct ClosedWorkspace {
    pub(crate) label: String,
    /// Unix seconds.
    pub(crate) closed_at: u64,
    pub(crate) workspace: WorkspaceSnapshot,
}

/// Newest last.
#[derive(Deserialize)]
struct ClosedWorkspaceFile {
    version: u32,
    workspaces: Vec<ClosedWorkspace>,
}

#[derive(Serialize)]
struct ClosedWorkspaceFileRef<'a> {
    version: u32,
    workspaces: &'a [ClosedWorkspace],
}

impl ClosedWorkspace {
    /// Stable handle for `workspace.reopen`: the workspace's own id.
    pub(crate) fn closed_id(&self) -> String {
        self.workspace
            .id
            .clone()
            .unwrap_or_else(|| format!("closed-{}", self.closed_at))
    }
}

/// Wraps one workspace so the normal session restore can rebuild it.
pub(crate) fn single_workspace_session(workspace: WorkspaceSnapshot) -> SessionSnapshot {
    SessionSnapshot {
        version: SNAPSHOT_VERSION,
        workspaces: vec![workspace],
        active: Some(0),
        selected: 0,
        sidebar_width: None,
        sidebar_section_split: None,
        collapsed_space_keys: Default::default(),
    }
}

pub(crate) fn closed_workspaces_path() -> PathBuf {
    crate::session::data_dir().join("closed-spaces.json")
}

pub(crate) fn load_closed_workspaces(path: &Path) -> Vec<ClosedWorkspace> {
    let Ok(json) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<ClosedWorkspaceFile>(&json) {
        // An older or newer layout format could restore wrongly; start over.
        Ok(file) if file.version == SNAPSHOT_VERSION => file.workspaces,
        Ok(_) => Vec::new(),
        Err(err) => {
            warn!(path = %path.display(), err = %err, "ignoring unreadable closed spaces file");
            Vec::new()
        }
    }
}

pub(crate) fn save_closed_workspaces(
    path: &Path,
    workspaces: &[ClosedWorkspace],
) -> std::io::Result<()> {
    if workspaces.is_empty() {
        return super::io::clear_path(path);
    }
    super::io::save_json_to_path(
        path,
        &ClosedWorkspaceFileRef {
            version: SNAPSHOT_VERSION,
            workspaces,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(label: &str) -> ClosedWorkspace {
        let snapshot = super::super::snapshot::capture(
            &[crate::workspace::Workspace::test_new(label)],
            &Default::default(),
            &crate::terminal::TerminalRuntimeRegistry::new(),
            Some(0),
            0,
        );
        ClosedWorkspace {
            label: label.into(),
            closed_at: 7,
            workspace: snapshot
                .workspaces
                .into_iter()
                .next()
                .expect("one workspace"),
        }
    }

    #[test]
    fn closed_workspaces_round_trip_and_clear_when_empty() {
        let path = std::env::temp_dir().join(format!(
            "herdr-closed-spaces-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        save_closed_workspaces(&path, &[closed("one"), closed("two")]).unwrap();
        let loaded = load_closed_workspaces(&path);
        assert_eq!(
            loaded.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );

        // Another snapshot format must not be restored.
        let json = std::fs::read_to_string(&path).unwrap().replacen(
            &format!("\"version\": {SNAPSHOT_VERSION}"),
            "\"version\": 999",
            1,
        );
        std::fs::write(&path, json).unwrap();
        assert!(load_closed_workspaces(&path).is_empty());

        save_closed_workspaces(&path, &[]).unwrap();
        assert!(!path.exists());
    }
}
