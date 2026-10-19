use serde::{Deserialize, Serialize};

/// The space whose notes to use: a workspace, a pane's workspace, or (with
/// neither) the focused pane's workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct NoteTarget {
    /// The workspace whose notes to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// A pane whose workspace's notes to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

/// Creates or replaces one note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct NoteWriteParams {
    /// The workspace whose notes to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// A pane whose workspace's notes to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The note, relative to the vault root, with `/` separators, ending in `.md`.
    pub path: String,
    pub content: String,
    /// The note's `fingerprint` as last read. Without it the note is created
    /// and the error is `note_exists` when it already exists; with it the
    /// note is replaced only when unchanged since, else `note_conflict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct NoteRenameParams {
    /// The workspace whose notes to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// A pane whose workspace's notes to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub from: String,
    pub to: String,
}

/// Moves one note into the vault's `.trash` folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct NoteDeleteParams {
    /// The workspace whose notes to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// A pane whose workspace's notes to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteFile {
    /// Relative to the vault root, with `/` separators.
    pub path: String,
    pub content: String,
    /// The sha256 of the file's bytes; pass it back to `note.write` to
    /// refuse writing over a change made since.
    pub fingerprint: String,
    /// Last modified, in milliseconds since the Unix epoch.
    pub modified_ms: u64,
}

/// A project's notes: every Markdown file under its folder outside the
/// repository, shared by the repository's worktrees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoteVault {
    /// The vault folder, absolute. It may not exist yet.
    pub root: String,
    pub project: String,
    /// The vault's `note.stamp` when this was read.
    pub stamp: String,
    /// Sorted by path.
    pub notes: Vec<NoteFile>,
    /// Some notes were left out: too many, or too large.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub truncated: bool,
}
