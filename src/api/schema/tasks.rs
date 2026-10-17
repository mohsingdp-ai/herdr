use serde::{Deserialize, Serialize};

use crate::cli::projection::{cli_params, optional_text, Field, Input};

fn workspace() -> Input<Option<String>> {
    super::tabs::workspace_option("ID").configure(|arg| arg.conflicts_with("pane"))
}

fn pane() -> Input<Option<String>> {
    Input::value("pane", "PANE_ID", |slot, value| {
        *slot = Some(crate::cli::normalize_pane_id(value));
        Ok(())
    })
}

fn task_id() -> Input<String> {
    Input::positional("id", 0, |slot, value| {
        *slot = value.to_owned();
        Ok(())
    })
    .value_name("ID")
}

fn texts(name: &'static str, value_name: &'static str) -> Input<Vec<String>> {
    Input::value(name, value_name, |slot, value| {
        slot.push(value.to_owned());
        Ok(())
    })
}

fn indexes(name: &'static str) -> Input<Vec<u32>> {
    Input::value(name, "N", |slot, value| {
        slot.push(
            value.parse().map_err(|_| {
                format!("an acceptance criterion index must be a number, got {value}")
            })?,
        );
        Ok(())
    })
}

cli_params! {
    /// The space whose tasks to use: a workspace, a pane's workspace, or
    /// (with neither) the focused pane's workspace.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct TaskTarget => TaskTargetProjection {
        /// The workspace whose tasks to use.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None).input(workspace().order(0)),
        /// A pane whose workspace's tasks to use, when no workspace is given. Defaults to the focused pane.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => Field::new(None).input(pane().order(1)),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct TaskGetParams => TaskGetProjection {
        /// The workspace whose tasks to use.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None).input(workspace().order(1)),
        /// A pane whose workspace's tasks to use, when no workspace is given. Defaults to the focused pane.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => Field::new(None).input(pane().order(2)),
        /// Task ID, such as `TASK-12`.
        pub id: String => Field::new(String::new()).input(task_id().order(0)),
    }
}

cli_params! {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct TaskCreateParams => TaskCreateProjection {
        /// The workspace whose tasks to use.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None).input(workspace().order(1)),
        /// A pane whose workspace's tasks to use, when no workspace is given. Defaults to the focused pane.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => Field::new(None).input(pane().order(2)),
        pub title: String => Field::new(String::new()).input(
            Input::positional("title", 0, |slot, value| {
                *slot = value.to_owned();
                Ok(())
            })
            .value_name("TITLE")
            .verbatim()
            .order(0),
        ),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String> => Field::new(None).input(optional_text("description", "TEXT").order(3)),
        /// One of the project's statuses. Defaults to the project's default status.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<String> => Field::new(None).input(optional_text("status", "STATUS").order(4)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub priority: Option<String> => Field::new(None).input(optional_text("priority", "PRIORITY").order(5)),
        #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
        pub task_type: Option<String> => Field::new(None).input(optional_text("type", "TYPE").order(6)),
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub assignees: Vec<String> => Field::new(Vec::new()).input(texts("assignee", "NAME").order(7)),
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub labels: Vec<String> => Field::new(Vec::new()).input(texts("label", "LABEL").order(8)),
        /// Acceptance criteria, in order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub acceptance_criteria: Vec<String> => Field::new(Vec::new()).input(texts("ac", "TEXT").order(9)),
        /// IDs of tasks this one depends on.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub dependencies: Vec<String> => Field::new(Vec::new()).input(texts("dep", "ID").order(10)),
    }
}

cli_params! {
    /// Changes one task. Only the given fields change.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
    pub struct TaskUpdateParams => TaskUpdateProjection {
        /// The workspace whose tasks to use.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace_id: Option<String> => Field::new(None).input(workspace().order(1)),
        /// A pane whose workspace's tasks to use, when no workspace is given. Defaults to the focused pane.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub pane_id: Option<String> => Field::new(None).input(pane().order(2)),
        /// Task ID, such as `TASK-12`.
        pub id: String => Field::new(String::new()).input(task_id().order(0)),
        /// The task's `fingerprint` as last read. When given and the task
        /// changed since, nothing is written and the error is `task_conflict`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub fingerprint: Option<String> => Field::new(None).input(optional_text("expect", "FINGERPRINT").order(3)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<String> => Field::new(None).input(optional_text("status", "STATUS").order(4)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub title: Option<String> => Field::new(None).input(optional_text("title", "TEXT").order(5)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String> => Field::new(None).input(optional_text("description", "TEXT").order(6)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub priority: Option<String> => Field::new(None).input(optional_text("priority", "PRIORITY").order(7)),
        #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
        pub task_type: Option<String> => Field::new(None).input(optional_text("type", "TYPE").order(8)),
        /// Replaces every assignee; an empty list clears them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub assignees: Option<Vec<String>> => Field::new(None)
            .input(Input::value("assignee", "NAME", |slot: &mut Option<Vec<String>>, value| {
                slot.get_or_insert_with(Vec::new).push(value.to_owned());
                Ok(())
            }).order(9))
            .input(Input::flag("no-assignee", |slot, _| {
                *slot = Some(Vec::new());
                Ok(())
            }).order(10)),
        /// Acceptance criteria to tick, by 1-based index.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub check_ac: Vec<u32> => Field::new(Vec::new()).input(indexes("check-ac").order(11)),
        /// Acceptance criteria to untick, by 1-based index.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub uncheck_ac: Vec<u32> => Field::new(Vec::new()).input(indexes("uncheck-ac").order(12)),
        /// A comment to append.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub comment: Option<String> => Field::new(None).input(optional_text("comment", "TEXT").order(13)),
        /// References (URLs or files) to add, such as the task's pull request.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub add_references: Vec<String> => Field::new(Vec::new()).input(texts("add-ref", "URL").order(15)),
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub comment_author: Option<String> => Field::new(None).input(optional_text("comment-author", "NAME").order(14)),
    }
}

/// A space's Backlog.md project: its statuses (the board's columns, in
/// order) and every task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskBoardInfo {
    /// The folder holding the Backlog.md project.
    pub root: String,
    pub statuses: Vec<String>,
    pub tasks: Vec<TaskSummary>,
    /// The project's `task.stamp` when this was read.
    #[serde(default)]
    pub stamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// IDs of tasks this one depends on.
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// The task this one is a subtask of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    /// URLs or files linked to the task, such as its pull request.
    #[serde(default)]
    pub references: Vec<String>,
    /// The start of the description, for a card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_preview: Option<String>,
    #[serde(default)]
    pub acceptance_criteria_checked: u32,
    #[serde(default)]
    pub acceptance_criteria_total: u32,
    /// Changes whenever the task's file changes; pass it back to `task.update`
    /// to refuse writing over a change made since.
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub summary: TaskSummary,
    /// The task file, relative to `root`.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<TaskCheckItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation_plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation_notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_summary: Option<String>,
    #[serde(default)]
    pub comments: Vec<TaskComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskCheckItem {
    /// 1-based, as `check_ac` and `uncheck_ac` take it.
    pub index: u32,
    pub text: String,
    pub checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskComment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub body: String,
}
