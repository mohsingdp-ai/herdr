use serde::{Deserialize, Serialize};

/// The folder a git request acts on: a workspace's folder, a pane's folder,
/// or (with neither) the focused pane's folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct GitTarget {
    /// The workspace whose folder to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The pane whose folder to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The repository root the caller is showing: the change is refused when
    /// the folder now resolves to a different repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitDiffParams {
    /// The workspace whose folder to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The pane whose folder to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Path relative to the repository root.
    pub path: String,
    /// The path before a rename, so both sides of the rename are diffed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
    /// Diff the file as changed by this commit. Without it, diff the working tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct GitLogParams {
    /// The workspace whose folder to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The pane whose folder to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Most commits to return, newest first. Defaults to 100; at most 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitShowParams {
    /// The workspace whose folder to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The pane whose folder to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitPathsParams {
    /// The workspace whose folder to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The pane whose folder to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The repository root the caller is showing: the change is refused when
    /// the folder now resolves to a different repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Paths relative to the repository root. Include a rename's old path too.
    pub paths: Vec<String>,
    /// Only this hunk of the single path, by its `@@ … @@` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunk: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitDiscardParams {
    /// The workspace whose folder to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The pane whose folder to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The repository root the caller is showing: the change is refused when
    /// the folder now resolves to a different repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Paths relative to the repository root.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitCommitParams {
    /// The workspace whose folder to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The pane whose folder to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The repository root the caller is showing: the change is refused when
    /// the folder now resolves to a different repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub message: String,
    /// Stage every change first, like `git commit -a` plus new files.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitUncommitParams {
    /// The workspace whose folder to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The pane whose folder to use, when no workspace is given. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The repository root the caller is showing: the change is refused when
    /// the folder now resolves to a different repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Must still be HEAD and not pushed anywhere.
    pub commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitFileChange {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
    pub kind: GitChangeKind,
    /// The index differs from HEAD.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub staged: bool,
    /// The working tree differs from the index (always true for untracked files).
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub unstaged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct GitStatusInfo {
    /// The repository root.
    pub root: String,
    /// The checked-out branch; none when HEAD is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The HEAD commit; none before the first commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default)]
    pub ahead: u32,
    #[serde(default)]
    pub behind: u32,
    #[serde(default)]
    pub files: Vec<GitFileChange>,
    /// More files changed than were listed.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub truncated: bool,
    /// The latest commit, push or pull started through `git.*`, while it runs
    /// and after it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<GitOperationInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitOperationKind {
    Commit,
    Push,
    Pull,
    #[serde(other)]
    Unknown,
}

/// A commit, push or pull running in the background: `git.commit`,
/// `git.push` and `git.pull` answer as soon as it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitOperationInfo {
    /// Unique per server run.
    pub id: u64,
    pub kind: GitOperationKind,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub running: bool,
    /// Why it failed, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One `@@` hunk. `lines` are raw unified-diff lines: the first character is
/// ' ', '+', '-' or '\' (the "no newline at end of file" marker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitHunk {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct GitFileDiff {
    pub path: String,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub binary: bool,
    /// The diff was cut short; very long lines are cut too.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub truncated: bool,
    /// Working tree against the index, or the commit against its first parent.
    #[serde(default)]
    pub hunks: Vec<GitHunk>,
    /// Index against HEAD. Empty for a commit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged_hunks: Vec<GitHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitCommitInfo {
    pub commit: String,
    pub short_commit: String,
    pub author: String,
    /// Author time, Unix seconds.
    pub time: u64,
    pub subject: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub merge: bool,
    /// Reachable from a remote-tracking branch.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub pushed: bool,
    /// Against the first parent.
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub insertions: u32,
    #[serde(default)]
    pub deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitCommitFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
    pub kind: GitChangeKind,
    #[serde(default)]
    pub insertions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub binary: bool,
}
