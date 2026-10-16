use clap::Command;

use crate::api::schema::{
    Method, Request, TaskCreateProjection, TaskGetProjection, TaskTargetProjection,
    TaskUpdateProjection,
};

use super::projection::{self, Leaf, ParseError};

const LEAVES: [Leaf; 5] = [
    Leaf {
        name: "list",
        about: "List the space's Backlog.md tasks",
        id: "cli:task:list",
        arguments: TaskTargetProjection::arguments,
        method: |matches| {
            let mut target = TaskTargetProjection::from_matches(matches)?;
            default_to_caller(&target.workspace_id, &mut target.pane_id);
            Ok(Method::TaskList(target))
        },
    },
    Leaf {
        name: "get",
        about: "Show one task",
        id: "cli:task:get",
        arguments: TaskGetProjection::arguments,
        method: |matches| {
            let mut params = TaskGetProjection::from_matches(matches)?;
            default_to_caller(&params.workspace_id, &mut params.pane_id);
            Ok(Method::TaskGet(params))
        },
    },
    Leaf {
        name: "create",
        about: "Create a task",
        id: "cli:task:create",
        arguments: TaskCreateProjection::arguments,
        method: |matches| {
            let mut params = TaskCreateProjection::from_matches(matches)?;
            default_to_caller(&params.workspace_id, &mut params.pane_id);
            Ok(Method::TaskCreate(params))
        },
    },
    Leaf {
        name: "update",
        about: "Change a task; --expect refuses it if the task changed since",
        id: "cli:task:update",
        arguments: TaskUpdateProjection::arguments,
        method: |matches| {
            let mut params = TaskUpdateProjection::from_matches(matches)?;
            default_to_caller(&params.workspace_id, &mut params.pane_id);
            Ok(Method::TaskUpdate(params))
        },
    },
    Leaf {
        name: "init",
        about: "Start a Backlog.md project in the space's folder",
        id: "cli:task:init",
        arguments: TaskTargetProjection::arguments,
        method: |matches| {
            let mut target = TaskTargetProjection::from_matches(matches)?;
            default_to_caller(&target.workspace_id, &mut target.pane_id);
            Ok(Method::TaskInit(target))
        },
    },
];

/// An agent's `herdr task` acts on its own space, not the focused one.
fn default_to_caller(workspace_id: &Option<String>, pane_id: &mut Option<String>) {
    if workspace_id.is_none() && pane_id.is_none() {
        *pane_id = super::target::caller_pane_id().map(|id| super::normalize_pane_id(&id));
    }
}

pub(crate) fn command() -> Command {
    Command::new("task")
        .about("Backlog.md tasks of a space")
        .subcommand_required(true)
        .subcommands(LEAVES.iter().map(Leaf::command))
}

pub(crate) fn request(args: &[String]) -> Result<Request, ParseError> {
    projection::group_request(command(), &LEAVES, args)
}

pub(super) fn run_task_command(args: &[String]) -> std::io::Result<i32> {
    super::run_json_request(request(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> serde_json::Value {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        crate::cli::target::with_test_client(crate::api::client::ApiClient::local(), || {
            serde_json::to_value(request(&args).unwrap()).unwrap()
        })
    }

    #[test]
    fn task_commands_build_their_requests() {
        assert_eq!(
            parse(&["list", "--workspace", "2"]),
            serde_json::json!({"id":"cli:task:list","method":"task.list","params":{"workspace_id":"2"}})
        );
        assert_eq!(
            parse(&[
                "create",
                "--fix it",
                "--priority",
                "high",
                "--ac",
                "a",
                "--ac",
                "b",
                "--dep",
                "TASK-1",
                "--workspace",
                "1"
            ]),
            serde_json::json!({"id":"cli:task:create","method":"task.create","params":{
                "workspace_id":"1","title":"--fix it","priority":"high",
                "acceptance_criteria":["a","b"],"dependencies":["TASK-1"]}})
        );
        assert_eq!(
            parse(&[
                "update",
                "TASK-3",
                "--expect",
                "abc",
                "--status",
                "Done",
                "--check-ac",
                "2",
                "--no-assignee",
                "--workspace",
                "1"
            ]),
            serde_json::json!({"id":"cli:task:update","method":"task.update","params":{
                "workspace_id":"1","id":"TASK-3","fingerprint":"abc","status":"Done",
                "assignees":[],"check_ac":[2]}})
        );
        let args: Vec<String> = ["update", "TASK-3", "--check-ac", "x"]
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect();
        assert!(request(&args).is_err());
        command().debug_assert();
    }
}
