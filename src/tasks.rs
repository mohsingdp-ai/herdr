//! Backlog.md tasks for one space's folder, behind the Board and the
//! `task.*` API. Every read and write goes through the `backlog` CLI; task
//! files are only read, for what its list leaves out (dependencies) and for
//! each task's fingerprint. Every function shells out and may block, so
//! callers run them off the main loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::api::schema::{
    TaskBoardInfo, TaskCheckItem, TaskComment, TaskCreateParams, TaskDetail, TaskSummary,
    TaskUpdateParams,
};

const PROGRAM: &str = "backlog";
/// Answers within the client's 60-second wait.
const TIMEOUT: Duration = Duration::from_secs(50);
const MAX_OUTPUT_BYTES: usize = 32 << 20;
/// Hex digits of a file's sha256 kept as its fingerprint.
const FINGERPRINT_LEN: usize = 16;
/// The columns a project initialized from herdr starts with.
const INITIAL_STATUSES: [&str; 4] = ["To Do", "In Progress", "Review", "Done"];
/// Backlog.md's own default, for a config without a `statuses` line.
const DEFAULT_STATUSES: [&str; 3] = ["To Do", "In Progress", "Done"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskError {
    NotInstalled,
    NotInitialized,
    AlreadyInitialized,
    NotFound(String),
    /// The task changed since the caller read it; nothing was written.
    Conflict,
    Rejected(String),
    Failed(String),
}

impl TaskError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotInstalled => "backlog_not_installed",
            Self::NotInitialized => "backlog_not_initialized",
            Self::AlreadyInitialized => "backlog_already_initialized",
            Self::NotFound(_) => "task_not_found",
            Self::Conflict => "task_conflict",
            Self::Rejected(_) => "invalid_request",
            Self::Failed(_) => "backlog_failed",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::NotInstalled => "Backlog.md is not installed (the `backlog` command)".to_owned(),
            Self::NotInitialized => "this folder has no Backlog.md project".to_owned(),
            Self::AlreadyInitialized => "this folder already has a Backlog.md project".to_owned(),
            Self::NotFound(id) => format!("task {id} not found"),
            Self::Conflict => "the task changed since it was read; reload and try again".to_owned(),
            Self::Rejected(message) | Self::Failed(message) => message.clone(),
        }
    }
}

type Result<T> = std::result::Result<T, TaskError>;

pub(crate) fn list(folder: &Path) -> Result<TaskBoardInfo> {
    let project = Project::find(folder)?;
    // Files first: a task changed in between then reads as a conflict on its
    // next write, never as a fresh fingerprint over stale fields.
    let mut files = scan(&project.tasks);
    let list: ListJson = parse_json(&backlog(&project.root, &["task", "list", "--json"])?)?;
    let tasks = list
        .tasks
        .into_iter()
        .map(|task| {
            let file = files.remove(&task.id.to_ascii_uppercase());
            task.into_summary(file)
        })
        .collect();
    Ok(TaskBoardInfo {
        root: project.root.display().to_string(),
        statuses: project.statuses(),
        tasks,
    })
}

pub(crate) fn get(folder: &Path, id: &str) -> Result<TaskDetail> {
    let project = Project::find(folder)?;
    project.detail(id)
}

pub(crate) fn create(folder: &Path, params: &TaskCreateParams) -> Result<TaskDetail> {
    let project = Project::find(folder)?;
    let title = checked_text("title", &params.title)?;
    let mut args = vec!["task".to_owned(), "create".to_owned()];
    push_opt(&mut args, "-d", params.description.as_deref());
    push_opt(&mut args, "-s", params.status.as_deref());
    push_opt(&mut args, "--priority", params.priority.as_deref());
    push_opt(&mut args, "--type", params.task_type.as_deref());
    push_all(&mut args, "-a", &params.assignees);
    push_all(&mut args, "-l", &params.labels);
    push_all(&mut args, "--ac", &params.acceptance_criteria);
    push_all(&mut args, "--dep", &params.dependencies);
    args.extend(["--".to_owned(), title.to_owned()]);
    // Locked: Backlog.md picks the next free ID, so two creates must not overlap.
    let output = locked(&project.root, || backlog(&project.root, &args))?;
    let id = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Created task "))
        .map(str::trim)
        .ok_or_else(|| TaskError::Failed(format!("unexpected backlog output: {output}")))?;
    project.detail(id)
}

pub(crate) fn update(folder: &Path, params: &TaskUpdateParams) -> Result<TaskDetail> {
    let project = Project::find(folder)?;
    let mut args = vec!["task".to_owned(), "edit".to_owned(), params.id.clone()];
    push_opt(&mut args, "-s", params.status.as_deref());
    if let Some(title) = &params.title {
        args.extend(["-t".to_owned(), checked_text("title", title)?.to_owned()]);
    }
    push_opt(&mut args, "-d", params.description.as_deref());
    push_opt(&mut args, "--priority", params.priority.as_deref());
    push_opt(&mut args, "--type", params.task_type.as_deref());
    match params.assignees.as_deref() {
        Some([]) => args.extend(["-a".to_owned(), String::new()]),
        Some(assignees) => push_all(&mut args, "-a", assignees),
        None => {}
    }
    for index in &params.check_ac {
        args.extend(["--check-ac".to_owned(), index.to_string()]);
    }
    for index in &params.uncheck_ac {
        args.extend(["--uncheck-ac".to_owned(), index.to_string()]);
    }
    if let Some(comment) = &params.comment {
        args.extend([
            "--comment".to_owned(),
            checked_text("comment", comment)?.to_owned(),
        ]);
        push_opt(
            &mut args,
            "--comment-author",
            params.comment_author.as_deref(),
        );
    }
    if args.len() == 3 {
        return Err(TaskError::Rejected("nothing to change".to_owned()));
    }
    locked(&project.root, || {
        let file = project.file(&params.id)?;
        if params
            .fingerprint
            .as_ref()
            .is_some_and(|expected| *expected != file.fingerprint)
        {
            return Err(TaskError::Conflict);
        }
        backlog(&project.root, &args).map(drop)
    })?;
    project.detail(&params.id)
}

/// Starts a Backlog.md project in `folder` with herdr's columns.
pub(crate) fn init(folder: &Path) -> Result<TaskBoardInfo> {
    if Project::find(folder).is_ok() {
        return Err(TaskError::AlreadyInitialized);
    }
    let name = folder.file_name().map_or_else(
        || "tasks".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut args = vec![
        "init",
        name.as_str(),
        "--defaults",
        // Never write AGENTS.md or CLAUDE.md into someone's project unasked.
        "--integration-mode",
        "none",
    ];
    // Outside git, init would stop to ask whether to create a repository.
    if !folder.ancestors().any(|dir| dir.join(".git").exists()) {
        args.push("--no-git");
    }
    locked(folder, || backlog(folder, &args))?;
    let project = Project::find(folder)?;
    let config = std::fs::read_to_string(&project.config).map_err(io_failed)?;
    let statuses = INITIAL_STATUSES
        .iter()
        .map(|status| format!("\"{status}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let config = set_config_line(&config, "statuses", &format!("[{statuses}]"));
    std::fs::write(&project.config, config).map_err(io_failed)?;
    list(folder)
}

struct Project {
    root: PathBuf,
    tasks: PathBuf,
    config: PathBuf,
}

impl Project {
    /// The nearest Backlog.md project at or above `folder`, as `backlog`
    /// itself finds it: a root `backlog.config.yml`, or a `backlog/` or
    /// `.backlog/` folder with its own `config.yml`.
    fn find(folder: &Path) -> Result<Self> {
        for dir in folder.ancestors() {
            let root_config = dir.join("backlog.config.yml");
            if root_config.is_file() {
                let text = std::fs::read_to_string(&root_config).unwrap_or_default();
                let backlog_dir = config_value(&text, "backlog_directory")
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "backlog".to_owned());
                return Ok(Self {
                    root: dir.to_path_buf(),
                    tasks: dir.join(backlog_dir).join("tasks"),
                    config: root_config,
                });
            }
            for name in ["backlog", ".backlog"] {
                let config = dir.join(name).join("config.yml");
                if config.is_file() {
                    return Ok(Self {
                        root: dir.to_path_buf(),
                        tasks: dir.join(name).join("tasks"),
                        config,
                    });
                }
            }
        }
        Err(TaskError::NotInitialized)
    }

    fn statuses(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.config).unwrap_or_default();
        let statuses = config_value(&text, "statuses")
            .map(|value| flow_list(&value))
            .unwrap_or_default();
        if statuses.is_empty() {
            DEFAULT_STATUSES.map(str::to_owned).to_vec()
        } else {
            statuses
        }
    }

    fn file(&self, id: &str) -> Result<TaskFile> {
        scan(&self.tasks)
            .remove(&id.trim().to_ascii_uppercase())
            .ok_or_else(|| TaskError::NotFound(id.to_owned()))
    }

    fn detail(&self, id: &str) -> Result<TaskDetail> {
        // File first, for the same reason as in `list`.
        let file = self.file(id)?;
        let view: ViewJson = parse_json(&backlog(
            &self.root,
            &["task", "view", file.id.as_str(), "--json"],
        )?)?;
        Ok(view.task.into_detail(file))
    }
}

/// Runs `work` holding the write lock of the project at `root`: one `backlog`
/// write per project at a time, while other projects are not held up.
fn locked<T>(root: &Path, work: impl FnOnce() -> Result<T>) -> Result<T> {
    static LOCKS: Mutex<Option<HashMap<PathBuf, Arc<Mutex<()>>>>> = Mutex::new(None);
    let project = Arc::clone(
        LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert_with(HashMap::new)
            .entry(root.to_path_buf())
            .or_default(),
    );
    let _guard = project
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    work()
}

struct TaskFile {
    id: String,
    fingerprint: String,
    dependencies: Vec<String>,
}

/// Every task file in `dir` by upper-case ID.
fn scan(dir: &Path) -> HashMap<String, TaskFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|bytes| {
            let digest = Sha256::digest(&bytes);
            let mut fingerprint = crate::checksum::to_lower_hex(&digest);
            fingerprint.truncate(FINGERPRINT_LEN);
            let (id, dependencies) = frontmatter(&String::from_utf8_lossy(&bytes))?;
            Some((
                id.to_ascii_uppercase(),
                TaskFile {
                    id,
                    fingerprint,
                    dependencies,
                },
            ))
        })
        .collect()
}

/// A task file's `id` and `dependencies`, from its YAML frontmatter.
fn frontmatter(text: &str) -> Option<(String, Vec<String>)> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut id = None;
    let mut dependencies = Vec::new();
    let mut in_dependencies = false;
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if in_dependencies {
            if let Some(item) = line.trim_start().strip_prefix("- ") {
                dependencies.push(unquote(item));
                continue;
            }
            if line.starts_with([' ', '\t']) {
                continue;
            }
            in_dependencies = false;
        }
        if let Some(value) = line.strip_prefix("id:") {
            id = Some(unquote(value));
        } else if let Some(value) = line.strip_prefix("dependencies:") {
            let value = value.trim();
            if value.is_empty() {
                in_dependencies = true;
            } else {
                dependencies = flow_list(value);
            }
        }
    }
    id.filter(|id| !id.is_empty()).map(|id| (id, dependencies))
}

/// The value of a top-level `key: value` line.
fn config_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix(':')?;
        Some(unquote(value))
    })
}

fn set_config_line(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key}: {value}");
    let prefix = format!("{key}:");
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|existing| {
            if existing.starts_with(&prefix) {
                found = true;
                line.clone()
            } else {
                existing.to_owned()
            }
        })
        .collect();
    if !found {
        lines.push(line);
    }
    lines.join("\n") + "\n"
}

/// The items of a one-line YAML list such as `["To Do", 'Done', Review]`.
/// ponytail: items holding a comma are split; Backlog.md never writes those.
fn flow_list(value: &str) -> Vec<String> {
    let value = value.trim();
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .map(unquote)
        .filter(|item| !item.is_empty())
        .collect()
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner.to_owned();
        }
    }
    value.to_owned()
}

fn checked_text<'a>(label: &str, text: &'a str) -> Result<&'a str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(TaskError::Rejected(format!("{label} must not be empty")));
    }
    Ok(trimmed)
}

fn push_opt(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        args.extend([flag.to_owned(), value.to_owned()]);
    }
}

fn push_all(args: &mut Vec<String>, flag: &str, values: &[String]) {
    for value in values {
        args.extend([flag.to_owned(), value.clone()]);
    }
}

fn io_failed(err: std::io::Error) -> TaskError {
    TaskError::Failed(err.to_string())
}

/// Runs `backlog <args>` in `root`, returning what it printed.
fn backlog<S: AsRef<str>>(root: &Path, args: &[S]) -> Result<String> {
    if !crate::integration::command_available(PROGRAM) {
        return Err(TaskError::NotInstalled);
    }
    let args: Vec<String> = args.iter().map(|arg| arg.as_ref().to_owned()).collect();
    let mut command = crate::plugin_command::command_for_argv_in_dir(PROGRAM, &args, root);
    let (status, stdout, _, stderr) =
        crate::source_control::run_raw(&mut command, None, TIMEOUT, MAX_OUTPUT_BYTES)
            .map_err(|err| TaskError::Failed(err.message()))?;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    if status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&stderr);
    let message = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .map_or_else(|| format!("backlog failed ({status})"), str::to_owned);
    // A refused value (an unknown status, a bad index) is the caller's to fix.
    Err(TaskError::Rejected(message))
}

fn parse_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text)
        .map_err(|err| TaskError::Failed(format!("could not read backlog output: {err}")))
}

#[derive(Deserialize)]
struct ListJson {
    tasks: Vec<TaskJson>,
}

#[derive(Deserialize)]
struct ViewJson {
    task: TaskJson,
}

/// One task as `backlog task list --json` and `task view --json` print it.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct TaskJson {
    id: String,
    title: String,
    status: String,
    priority: Option<String>,
    #[serde(rename = "type")]
    task_type: Option<String>,
    assignees: Vec<String>,
    labels: Vec<String>,
    acceptance_criteria_completed: u32,
    acceptance_criteria_count: u32,
    // `task view` only.
    path: Option<String>,
    description: Option<String>,
    dependencies: Option<Vec<String>>,
    acceptance_criteria: Vec<CheckItemJson>,
    implementation_plan: Option<String>,
    implementation_notes: Option<String>,
    final_summary: Option<String>,
    comments: Vec<CommentJson>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CheckItemJson {
    index: u32,
    text: String,
    checked: bool,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct CommentJson {
    author: Option<String>,
    created_at: Option<String>,
    body: String,
}

impl TaskJson {
    fn into_summary(self, file: Option<TaskFile>) -> TaskSummary {
        let (fingerprint, file_dependencies) = file
            .map(|file| (file.fingerprint, file.dependencies))
            .unwrap_or_default();
        TaskSummary {
            id: self.id,
            title: self.title,
            status: self.status,
            priority: self.priority,
            task_type: self.task_type,
            assignees: self.assignees,
            labels: self.labels,
            dependencies: self.dependencies.unwrap_or(file_dependencies),
            acceptance_criteria_checked: self.acceptance_criteria_completed,
            acceptance_criteria_total: self.acceptance_criteria_count,
            fingerprint,
        }
    }

    fn into_detail(mut self, file: TaskFile) -> TaskDetail {
        let path = self.path.take().unwrap_or_default();
        let description = self.description.take();
        let acceptance_criteria = std::mem::take(&mut self.acceptance_criteria)
            .into_iter()
            .map(|item| TaskCheckItem {
                index: item.index,
                text: item.text,
                checked: item.checked,
            })
            .collect();
        let implementation_plan = self.implementation_plan.take();
        let implementation_notes = self.implementation_notes.take();
        let final_summary = self.final_summary.take();
        let comments = std::mem::take(&mut self.comments)
            .into_iter()
            .map(|comment| TaskComment {
                author: comment.author,
                created_at: comment.created_at,
                body: comment.body,
            })
            .collect();
        TaskDetail {
            summary: self.into_summary(Some(file)),
            path,
            description,
            acceptance_criteria,
            implementation_plan,
            implementation_notes,
            final_summary,
            comments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_reads_id_and_both_dependency_styles() {
        let block = "---\nid: TASK-2\ntitle: Two\ndependencies:\n  - TASK-1\n  - 'task-3'\nordinal: 2\n---\nbody\n";
        assert_eq!(
            frontmatter(block),
            Some(("TASK-2".into(), vec!["TASK-1".into(), "task-3".into()]))
        );
        let flow = "---\r\nid: 'T-9'\r\ndependencies: [T-1, \"T-2\"]\r\n---\r\n";
        assert_eq!(
            frontmatter(flow),
            Some(("T-9".into(), vec!["T-1".into(), "T-2".into()]))
        );
        assert_eq!(
            frontmatter("---\nid: X-1\ndependencies: []\n---\n"),
            Some(("X-1".into(), vec![]))
        );
        assert_eq!(frontmatter("no frontmatter"), None);
    }

    #[test]
    fn config_statuses_and_rewrite() {
        let config = "project_name: \"demo\"\nstatuses: [\"To Do\", 'In Progress', Done]\n";
        assert_eq!(
            config_value(config, "statuses").map(|value| flow_list(&value)),
            Some(vec!["To Do".into(), "In Progress".into(), "Done".into()])
        );
        let rewritten = set_config_line(config, "statuses", "[\"A\", \"B\"]");
        assert_eq!(
            rewritten,
            "project_name: \"demo\"\nstatuses: [\"A\", \"B\"]\n"
        );
        assert_eq!(
            config_value(&rewritten, "project_name").as_deref(),
            Some("demo")
        );
    }

    /// The real CLI, in throwaway folders. Skipped when `backlog` is missing.
    mod backlog_cli {
        use super::super::*;

        fn available() -> bool {
            let found = crate::integration::command_available(PROGRAM);
            if !found {
                eprintln!("skipping: backlog is not installed");
            }
            found
        }

        fn folder(name: &str) -> PathBuf {
            let dir =
                std::env::temp_dir().join(format!("herdr-tasks-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn create_params(title: &str) -> TaskCreateParams {
            TaskCreateParams {
                title: title.into(),
                ..TaskCreateParams::default()
            }
        }

        #[test]
        fn init_create_update_and_conflict() {
            if !available() {
                return;
            }
            let dir = folder("flow");
            assert_eq!(list(&dir), Err(TaskError::NotInitialized));

            let board = init(&dir).unwrap();
            assert_eq!(board.statuses, INITIAL_STATUSES.map(str::to_owned).to_vec());
            assert!(board.tasks.is_empty());
            assert!(!dir.join("AGENTS.md").exists());
            assert_eq!(init(&dir), Err(TaskError::AlreadyInitialized));

            let first = create(
                &dir,
                &TaskCreateParams {
                    title: "--looks like a flag".into(),
                    priority: Some("high".into()),
                    acceptance_criteria: vec!["works".into()],
                    ..TaskCreateParams::default()
                },
            )
            .unwrap();
            assert_eq!(first.summary.title, "--looks like a flag");
            assert_eq!(first.summary.status, "To Do");
            let second = create(
                &dir,
                &TaskCreateParams {
                    dependencies: vec![first.summary.id.clone()],
                    ..create_params("Second")
                },
            )
            .unwrap();

            // The list carries dependencies read from the task files.
            let board = list(&dir).unwrap();
            let listed = board
                .tasks
                .iter()
                .find(|task| task.id == second.summary.id)
                .unwrap();
            assert_eq!(listed.dependencies, vec![first.summary.id.clone()]);
            assert_eq!(listed.fingerprint, second.summary.fingerprint);

            // A move with the current fingerprint lands and changes it.
            let moved = update(
                &dir,
                &TaskUpdateParams {
                    id: first.summary.id.clone(),
                    fingerprint: Some(first.summary.fingerprint.clone()),
                    status: Some("Review".into()),
                    check_ac: vec![1],
                    comment: Some("done here".into()),
                    comment_author: Some("@herdr".into()),
                    ..TaskUpdateParams::default()
                },
            )
            .unwrap();
            assert_eq!(moved.summary.status, "Review");
            assert_eq!(moved.summary.acceptance_criteria_checked, 1);
            assert_eq!(moved.comments.len(), 1);
            assert_ne!(moved.summary.fingerprint, first.summary.fingerprint);

            // The stale fingerprint is refused and nothing is written.
            let stale = update(
                &dir,
                &TaskUpdateParams {
                    id: first.summary.id.clone(),
                    fingerprint: Some(first.summary.fingerprint.clone()),
                    status: Some("Done".into()),
                    ..TaskUpdateParams::default()
                },
            );
            assert_eq!(stale, Err(TaskError::Conflict));
            assert_eq!(
                get(&dir, &first.summary.id).unwrap().summary.status,
                "Review"
            );

            let bad_status = update(
                &dir,
                &TaskUpdateParams {
                    id: first.summary.id.clone(),
                    status: Some("Nope".into()),
                    ..TaskUpdateParams::default()
                },
            );
            assert!(matches!(bad_status, Err(TaskError::Rejected(_))));
            assert!(matches!(get(&dir, "TASK-999"), Err(TaskError::NotFound(_))));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn two_spaces_stay_isolated() {
            if !available() {
                return;
            }
            let (one, two) = (folder("one"), folder("two"));
            init(&one).unwrap();
            init(&two).unwrap();
            create(&one, &create_params("Only in one")).unwrap();
            assert_eq!(list(&one).unwrap().tasks.len(), 1);
            assert!(list(&two).unwrap().tasks.is_empty());
            // A subfolder belongs to the project above it.
            let nested = one.join("src/deep");
            std::fs::create_dir_all(&nested).unwrap();
            assert_eq!(list(&nested).unwrap().tasks.len(), 1);
            let _ = std::fs::remove_dir_all(&one);
            let _ = std::fs::remove_dir_all(&two);
        }
    }
}
