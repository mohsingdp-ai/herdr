use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct CheckpointListParams {
    /// A pane in the folder whose checkpoints to list. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CheckpointRestoreParams {
    /// A pane in the folder to rewind. Defaults to the focused pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub checkpoint_id: String,
    /// Rewind even while an agent is working in the folder.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub force: bool,
}

/// A hidden git snapshot of a folder, taken before an agent worked on a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CheckpointInfo {
    pub checkpoint_id: String,
    /// Unix seconds.
    pub created_at: u64,
    pub label: String,
}
