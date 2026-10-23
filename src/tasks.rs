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
/// The columns a project initialized from herdr starts with: the workflow
/// where people promote proposed tasks and approve reviews, and agents do
/// the rest. Not "Draft": Backlog.md moves a task with that status into its
/// drafts folder under a new ID.
const INITIAL_STATUSES: [&str; 6] = [
    "Backlog",
    "To Do",
    "In Progress",
    "Review",
    "Blocked",
    "Done",
];
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
    /// An agent asked for a move that is a person's to make.
    Refused(String),
    /// An agent asked to put a task in Review without a pull request link.
    NeedsPullRequest(String),
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
            Self::Refused(_) => "workflow_refused",
            Self::NeedsPullRequest(_) => "review_needs_pr",
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
            Self::NeedsPullRequest(id) => format!(
                "{id} needs a pull request link before Review: add one with --add-ref <url>"
            ),
            Self::Refused(message) | Self::Rejected(message) | Self::Failed(message) => {
                message.clone()
            }
        }
    }
}

type Result<T> = std::result::Result<T, TaskError>;

pub(crate) fn list(folder: &Path) -> Result<TaskBoardInfo> {
    let project = Project::find(folder)?;
    // Files first: a task changed in between then reads as a conflict on its
    // next write, never as a fresh fingerprint over stale fields, and as a
    // new stamp, so a watching Board reads the list again.
    let stamp = project.stamp();
    let mut files = scan(&project.tasks);
    let list: ListJson = parse_json(&backlog(&project.root, &["task", "list", "--json"])?)?;
    let tasks = list
        .tasks
        .into_iter()
        .map(|task| {
            let file = files.remove(&task.id.to_ascii_uppercase());
            let mut summary = task.into_summary(file);
            summary.pane_id = linked_pane(&project.root, &summary.id);
            summary
        })
        .collect();
    Ok(TaskBoardInfo {
        root: project.root.display().to_string(),
        statuses: project.statuses(),
        tasks,
        stamp,
    })
}

/// A cheap check for changes: no `backlog` call, only file names, sizes
/// and times, so a Board can ask every few seconds.
pub(crate) fn stamp(folder: &Path) -> Result<String> {
    Ok(Project::find(folder)?.stamp())
}

/// Remembers that the agent in `pane_id` created or changed task `id`, so
/// reads can say which pane is working on it. Kept only in memory: pane IDs
/// do not outlive the server either.
pub(crate) fn link_pane(folder: &Path, id: &str, pane_id: &str) {
    let Ok(project) = Project::find(folder) else {
        return;
    };
    agent_panes(|panes| {
        panes.insert((project.root, id.to_ascii_uppercase()), pane_id.to_owned());
    });
}

fn linked_pane(root: &Path, id: &str) -> Option<String> {
    agent_panes(|panes| {
        panes
            .get(&(root.to_path_buf(), id.to_ascii_uppercase()))
            .cloned()
    })
}

fn agent_panes<T>(work: impl FnOnce(&mut HashMap<(PathBuf, String), String>) -> T) -> T {
    static PANES: Mutex<Option<HashMap<(PathBuf, String), String>>> = Mutex::new(None);
    work(
        PANES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert_with(HashMap::new),
    )
}

pub(crate) fn get(folder: &Path, id: &str) -> Result<TaskDetail> {
    let project = Project::find(folder)?;
    project.detail(id)
}

/// `agent` names the agent asking, when an agent's pane asked: its tasks
/// start in Backlog, where a person promotes them.
pub(crate) fn create(
    folder: &Path,
    params: &TaskCreateParams,
    agent: Option<&str>,
) -> Result<TaskDetail> {
    let project = Project::find(folder)?;
    let title = checked_text("title", &params.title)?;
    let mut status = params.status.clone();
    if agent.is_some() {
        if let Some(backlog) = project.status_for(Stage::Backlog) {
            if status
                .as_deref()
                .is_some_and(|wanted| stage(wanted) != Some(Stage::Backlog))
            {
                return Err(TaskError::Refused(
                    "agents create tasks in Backlog; a person promotes them to To Do".to_owned(),
                ));
            }
            status = Some(backlog);
        }
    }
    let mut args = vec!["task".to_owned(), "create".to_owned()];
    push_opt(&mut args, "-d", params.description.as_deref());
    push_opt(&mut args, "-s", status.as_deref());
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

/// `agent` names the agent asking, when an agent's pane asked: it may only
/// make an agent's status moves (see [`agent_may_move`]), and its comments
/// are signed with its name unless they name an author.
pub(crate) fn update(
    folder: &Path,
    params: &TaskUpdateParams,
    agent: Option<&str>,
) -> Result<TaskDetail> {
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
    push_all(&mut args, "--add-ref", &params.add_references);
    push_opt(
        &mut args,
        "--final-summary",
        params.final_summary.as_deref(),
    );
    if let Some(comment) = &params.comment {
        args.extend([
            "--comment".to_owned(),
            checked_text("comment", comment)?.to_owned(),
        ]);
        let signed = agent.map(|agent| format!("@{}", agent.trim_start_matches('@')));
        push_opt(
            &mut args,
            "--comment-author",
            params.comment_author.as_deref().or(signed.as_deref()),
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
        if let (Some(_), Some(to)) = (agent, params.status.as_deref()) {
            check_agent_move(&params.id, &file, to, &params.add_references)?;
        }
        backlog(&project.root, &args).map(drop)
    })?;
    project.detail(&params.id)
}

/// Moves a task to the project's archive, out of the board.
pub(crate) fn archive(folder: &Path, id: &str) -> Result<TaskBoardInfo> {
    let project = Project::find(folder)?;
    locked(&project.root, || {
        let file = project.file(id)?;
        backlog(&project.root, &["task", "archive", file.id.as_str()]).map(drop)
    })?;
    list(folder)
}

/// Adds the workflow's columns a project lacks: Backlog first, Review and
/// Blocked before its last column. Existing columns stay as they are.
pub(crate) fn add_workflow(folder: &Path) -> Result<TaskBoardInfo> {
    let project = Project::find(folder)?;
    locked(&project.root, || {
        let mut statuses = project.statuses();
        let has = |statuses: &[String], wanted: Stage| {
            statuses.iter().any(|status| stage(status) == Some(wanted))
        };
        if !has(&statuses, Stage::Backlog) {
            statuses.insert(0, "Backlog".to_owned());
        }
        for (wanted, name) in [(Stage::Review, "Review"), (Stage::Blocked, "Blocked")] {
            if !has(&statuses, wanted) {
                let before_last = statuses.len().saturating_sub(1).max(1);
                statuses.insert(before_last, name.to_owned());
            }
        }
        write_statuses(&project.config, &statuses)
    })?;
    list(folder)
}

fn write_statuses(config: &Path, statuses: &[String]) -> Result<()> {
    let text = std::fs::read_to_string(config).map_err(io_failed)?;
    let list = statuses
        .iter()
        .map(|status| format!("\"{status}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let text = set_config_line(&text, "statuses", &format!("[{list}]"));
    std::fs::write(config, text).map_err(io_failed)
}

/// Starts a Backlog.md project in `folder` with herdr's columns.
pub(crate) fn init(folder: &Path) -> Result<TaskBoardInfo> {
    if Project::find(folder).is_ok() {
        return Err(TaskError::AlreadyInitialized);
    }
    let folder = &in_main_checkout(folder);
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
    write_statuses(&project.config, &INITIAL_STATUSES.map(str::to_owned))?;
    // New tasks wait in Backlog until a person promotes them.
    let config = std::fs::read_to_string(&project.config).map_err(io_failed)?;
    let config = set_config_line(&config, "default_status", "\"Backlog\"");
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
    /// `.backlog/` folder with its own `config.yml`. A linked git worktree
    /// uses its main checkout's project, so a repository has one board.
    fn find(folder: &Path) -> Result<Self> {
        for dir in in_main_checkout(folder).ancestors() {
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

    fn stamp(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut entries: Vec<(std::ffi::OsString, u64, Option<std::time::SystemTime>)> =
            std::fs::read_dir(&self.tasks)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let metadata = entry.metadata().ok()?;
                    Some((entry.file_name(), metadata.len(), metadata.modified().ok()))
                })
                .collect();
        entries.sort();
        entries.hash(&mut hasher);
        std::fs::metadata(&self.config)
            .ok()
            .map(|metadata| (metadata.len(), metadata.modified().ok()))
            .hash(&mut hasher);
        format!("{:016x}", hasher.finish())
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

    /// The project's own name for a workflow stage, when it has one.
    fn status_for(&self, wanted: Stage) -> Option<String> {
        self.statuses()
            .into_iter()
            .find(|status| stage(status) == Some(wanted))
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
        let mut detail = view.task.into_detail(file);
        detail.summary.pane_id = linked_pane(&self.root, &detail.summary.id);
        Ok(detail)
    }
}

/// The same place in the main checkout when `folder` is inside a linked git
/// worktree, otherwise `folder` itself. Reads only git's small pointer files
/// (`.git` and `commondir`), never runs git, so a Board's stamp stays cheap.
fn in_main_checkout(folder: &Path) -> PathBuf {
    let main = || {
        let worktree = folder.ancestors().find(|dir| dir.join(".git").exists())?;
        // A main checkout's `.git` is a folder, so this read fails for it.
        let pointer = std::fs::read_to_string(worktree.join(".git")).ok()?;
        let git_dir = worktree.join(pointer.trim().strip_prefix("gitdir:")?.trim());
        // Only linked worktrees have `commondir`; submodules do not.
        let common = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
        let common = git_dir.join(common.trim()).canonicalize().ok()?;
        if common.file_name()? != ".git" {
            return None;
        }
        Some(common.parent()?.join(folder.strip_prefix(worktree).ok()?))
    };
    main().unwrap_or_else(|| folder.to_path_buf())
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
    status: String,
    dependencies: Vec<String>,
    references: Vec<String>,
    description_preview: Option<String>,
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
            let text = String::from_utf8_lossy(&bytes);
            let header = frontmatter(&text)?;
            Some((
                header.id.to_ascii_uppercase(),
                TaskFile {
                    id: header.id,
                    fingerprint,
                    status: header.status,
                    dependencies: header.dependencies,
                    references: header.references,
                    description_preview: description_preview(&text),
                },
            ))
        })
        .collect()
}

/// What the Board reads from a task file's YAML frontmatter itself.
#[derive(Debug, Default, PartialEq, Eq)]
struct Frontmatter {
    id: String,
    status: String,
    dependencies: Vec<String>,
    references: Vec<String>,
}

impl Frontmatter {
    fn list(&mut self, key: &str) -> &mut Vec<String> {
        if key == "dependencies" {
            &mut self.dependencies
        } else {
            &mut self.references
        }
    }
}

fn frontmatter(text: &str) -> Option<Frontmatter> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut header = Frontmatter::default();
    // The list a `key:` line on its own opened, filled by the `- item` lines after it.
    let mut open: Option<&str> = None;
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if let Some(key) = open {
            if let Some(item) = line.trim_start().strip_prefix("- ") {
                header.list(key).push(unquote(item));
                continue;
            }
            if line.starts_with([' ', '\t']) {
                continue;
            }
            open = None;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key {
            "id" => header.id = unquote(value),
            "status" => header.status = unquote(value),
            "dependencies" | "references" if value.is_empty() => open = Some(key),
            "dependencies" | "references" => *header.list(key) = flow_list(value),
            _ => {}
        }
    }
    (!header.id.is_empty()).then_some(header)
}

/// The workflow's steps, by the usual Backlog.md status names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Proposed, waiting for a person to promote it to To Do.
    Backlog,
    ToDo,
    InProgress,
    Review,
    Blocked,
    Done,
}

fn stage(status: &str) -> Option<Stage> {
    Some(match status.trim().to_ascii_lowercase().as_str() {
        "backlog" => Stage::Backlog,
        "to do" | "todo" => Stage::ToDo,
        "in progress" | "doing" => Stage::InProgress,
        "review" | "in review" => Stage::Review,
        "blocked" => Stage::Blocked,
        "done" => Stage::Done,
        _ => return None,
    })
}

/// The moves that are an agent's: picking work up, handing it back, sending
/// it to Review, and stopping on a problem. Promoting from Backlog, approving,
/// sending work back from Review and unblocking are a person's. Statuses
/// outside the workflow's names are not ruled on.
fn agent_may_move(from: Stage, to: Stage) -> bool {
    use Stage::*;
    from == to
        || matches!(
            (from, to),
            (ToDo, InProgress)
                | (InProgress, ToDo)
                | (InProgress, Review)
                | (ToDo | InProgress, Blocked)
        )
}

fn check_agent_move(id: &str, file: &TaskFile, to: &str, adding: &[String]) -> Result<()> {
    let (Some(from), Some(target)) = (stage(&file.status), stage(to)) else {
        return Ok(());
    };
    if !agent_may_move(from, target) {
        let why = match (from, target) {
            (Stage::Backlog, _) => "only a person promotes a task out of Backlog",
            (_, Stage::Done) => "only a person approves a task into Done",
            (Stage::Review, _) => "a person approves or sends back a task in Review",
            (Stage::Blocked, _) => "only a person unblocks a task",
            _ => "agents move tasks To Do → In Progress → Review, or to Blocked",
        };
        return Err(TaskError::Refused(format!(
            "{id}: {} → {to} is not an agent's move ({why})",
            file.status
        )));
    }
    if target == Stage::Review
        && from != Stage::Review
        && !file
            .references
            .iter()
            .chain(adding)
            .any(|reference| is_pull_request(reference))
    {
        return Err(TaskError::NeedsPullRequest(id.to_owned()));
    }
    Ok(())
}

/// A pull or merge request link on GitHub, GitLab or Bitbucket.
pub(crate) fn is_pull_request(reference: &str) -> bool {
    ["/pull/", "/merge_requests/", "/pull-requests/"]
        .iter()
        .any(|marker| reference.contains(marker))
}

/// Most characters of a description kept for a card.
const PREVIEW_CHARS: usize = 160;

/// The first lines of a task file's description, joined, for a card.
fn description_preview(text: &str) -> Option<String> {
    let start = text.find("<!-- SECTION:DESCRIPTION:BEGIN -->")?;
    let body = &text[start..];
    let body = body[body.find('\n')? + 1..]
        .split("<!-- SECTION:DESCRIPTION:END -->")
        .next()?;
    let mut preview = String::new();
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !preview.is_empty() {
            preview.push(' ');
        }
        preview.push_str(line);
        if preview.chars().count() >= PREVIEW_CHARS {
            break;
        }
    }
    (!preview.is_empty()).then(|| preview.chars().take(PREVIEW_CHARS).collect())
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
    parent_task_id: Option<String>,
    milestone: Option<String>,
    references: Vec<String>,
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
        let (fingerprint, file_dependencies, description_preview) = file
            .map(|file| {
                (
                    file.fingerprint,
                    file.dependencies,
                    file.description_preview,
                )
            })
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
            parent_id: self.parent_task_id,
            milestone: self.milestone,
            references: self.references,
            description_preview,
            acceptance_criteria_checked: self.acceptance_criteria_completed,
            acceptance_criteria_total: self.acceptance_criteria_count,
            pane_id: None,
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
    fn linked_worktrees_use_the_main_checkout() {
        let base = std::env::temp_dir().join(format!("herdr-tasks-wt-{}", std::process::id()));
        let main = base.join("main");
        let link = main.join(".git/worktrees/wt");
        let worktree = base.join("wt");
        std::fs::create_dir_all(&link).unwrap();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(link.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", link.display()),
        )
        .unwrap();
        let main = main.canonicalize().unwrap();
        assert_eq!(in_main_checkout(&worktree), main);
        assert_eq!(in_main_checkout(&worktree.join("src")), main.join("src"));
        assert_eq!(in_main_checkout(&main), main);
        // A submodule's gitdir has no `commondir`: it stays where it is.
        std::fs::remove_file(link.join("commondir")).unwrap();
        assert_eq!(in_main_checkout(&worktree), worktree);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_tasks_agent_pane_is_found_from_any_folder_of_its_project() {
        let root = std::env::temp_dir().join(format!("herdr-tasks-pane-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("backlog.config.yml"), "").unwrap();
        assert_eq!(linked_pane(&root, "TASK-7"), None);
        link_pane(&root.join("src"), "task-7", "ws_1:p2");
        assert_eq!(linked_pane(&root, "TASK-7").as_deref(), Some("ws_1:p2"));
        // A later agent takes it over.
        link_pane(&root, "TASK-7", "ws_1:p3");
        assert_eq!(linked_pane(&root, "task-7").as_deref(), Some("ws_1:p3"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frontmatter_reads_id_and_both_dependency_styles() {
        let block = "---\nid: TASK-2\ntitle: Two\ndependencies:\n  - TASK-1\n  - 'task-3'\nordinal: 2\n---\nbody\n";
        let header = frontmatter(block).unwrap();
        assert_eq!(header.id, "TASK-2");
        assert_eq!(header.dependencies, ["TASK-1", "task-3"]);
        let flow = "---\r\nid: 'T-9'\r\nstatus: In Progress\r\ndependencies: [T-1, \"T-2\"]\r\nreferences:\r\n  - 'https://github.com/o/r/pull/3'\r\n---\r\n";
        let header = frontmatter(flow).unwrap();
        assert_eq!(header.id, "T-9");
        assert_eq!(header.status, "In Progress");
        assert_eq!(header.dependencies, ["T-1", "T-2"]);
        assert_eq!(header.references, ["https://github.com/o/r/pull/3"]);
        let empty = frontmatter("---\nid: X-1\ndependencies: []\n---\n").unwrap();
        assert!(empty.dependencies.is_empty());
        assert_eq!(frontmatter("no frontmatter"), None);
    }

    #[test]
    fn agents_make_only_their_own_moves_and_review_needs_a_pull_request() {
        use Stage::*;
        for (from, to) in [
            (ToDo, InProgress),
            (InProgress, Review),
            (InProgress, Blocked),
            (InProgress, ToDo),
        ] {
            assert!(agent_may_move(from, to), "{from:?} → {to:?}");
        }
        for (from, to) in [
            (Backlog, ToDo),
            (Review, Done),
            (Review, InProgress),
            (Blocked, ToDo),
            (ToDo, Done),
        ] {
            assert!(!agent_may_move(from, to), "{from:?} → {to:?}");
        }
        let file = |status: &str, references: Vec<String>| TaskFile {
            id: "T-1".into(),
            fingerprint: String::new(),
            status: status.into(),
            dependencies: Vec::new(),
            references,
            description_preview: None,
        };
        assert_eq!(
            check_agent_move("T-1", &file("In Progress", Vec::new()), "Review", &[]),
            Err(TaskError::NeedsPullRequest("T-1".into()))
        );
        let pr = "https://github.com/o/r/pull/3".to_owned();
        assert!(
            check_agent_move("T-1", &file("In Progress", vec![pr.clone()]), "Review", &[]).is_ok()
        );
        assert!(check_agent_move("T-1", &file("In Progress", Vec::new()), "review", &[pr]).is_ok());
        assert!(matches!(
            check_agent_move("T-1", &file("Backlog", Vec::new()), "To Do", &[]),
            Err(TaskError::Refused(message)) if message.contains("promotes")
        ));
        // Custom statuses are not ruled on.
        assert!(check_agent_move("T-1", &file("Triage", Vec::new()), "Shipped", &[]).is_ok());
    }

    #[test]
    fn description_preview_joins_the_first_lines() {
        let file = "---\nid: T-1\n---\n\n## Description\n\n<!-- SECTION:DESCRIPTION:BEGIN -->\nFirst line.\n\nSecond line.\n<!-- SECTION:DESCRIPTION:END -->\n";
        assert_eq!(
            description_preview(file).as_deref(),
            Some("First line. Second line.")
        );
        assert_eq!(description_preview("---\nid: T-1\n---\n"), None);
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
                None,
            )
            .unwrap();
            assert_eq!(first.summary.title, "--looks like a flag");
            assert_eq!(first.summary.status, "Backlog", "new tasks wait in Backlog");
            let second = create(
                &dir,
                &TaskCreateParams {
                    dependencies: vec![first.summary.id.clone()],
                    ..create_params("Second")
                },
                None,
            )
            .unwrap();

            let child = create(
                &dir,
                &TaskCreateParams {
                    description: Some("Child work".into()),
                    ..create_params("Child")
                },
                None,
            )
            .unwrap();
            // Subtasks come from Backlog.md's own --parent; created here through the CLI.
            backlog(
                &dir,
                &["task", "create", "Sub", "-p", first.summary.id.as_str()],
            )
            .unwrap();

            // The list carries dependencies, parents and previews read from the task files.
            let board = list(&dir).unwrap();
            let sub = board.tasks.iter().find(|task| task.title == "Sub").unwrap();
            assert_eq!(sub.parent_id.as_deref(), Some(first.summary.id.as_str()));
            let child_listed = board
                .tasks
                .iter()
                .find(|task| task.id == child.summary.id)
                .unwrap();
            assert_eq!(
                child_listed.description_preview.as_deref(),
                Some("Child work")
            );
            assert_eq!(
                stamp(&dir).unwrap(),
                board.stamp,
                "unchanged since the list"
            );
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
                    add_references: vec!["https://github.com/o/r/pull/7".into()],
                    ..TaskUpdateParams::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(moved.summary.status, "Review");
            assert_eq!(moved.summary.acceptance_criteria_checked, 1);
            assert_eq!(moved.comments.len(), 1);
            assert_eq!(
                moved.summary.references,
                vec!["https://github.com/o/r/pull/7".to_owned()]
            );
            assert_ne!(moved.summary.fingerprint, first.summary.fingerprint);
            assert_ne!(
                stamp(&dir).unwrap(),
                board.stamp,
                "a write changes the stamp"
            );

            // The stale fingerprint is refused and nothing is written.
            let stale = update(
                &dir,
                &TaskUpdateParams {
                    id: first.summary.id.clone(),
                    fingerprint: Some(first.summary.fingerprint.clone()),
                    status: Some("Done".into()),
                    ..TaskUpdateParams::default()
                },
                None,
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
                None,
            );
            assert!(matches!(bad_status, Err(TaskError::Rejected(_))));
            assert!(matches!(get(&dir, "TASK-999"), Err(TaskError::NotFound(_))));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn agents_follow_the_workflow_and_people_promote_and_approve() {
            if !available() {
                return;
            }
            let dir = folder("workflow");
            init(&dir).unwrap();
            let agent = Some("claude");
            let update_as = |agent: Option<&str>, id: &str, edit: TaskUpdateParams| {
                update(
                    &dir,
                    &TaskUpdateParams {
                        id: id.into(),
                        ..edit
                    },
                    agent,
                )
            };
            let status = |status: &str| TaskUpdateParams {
                status: Some(status.into()),
                ..TaskUpdateParams::default()
            };

            let task = create(&dir, &create_params("Agent idea"), agent).unwrap();
            let id = task.summary.id.clone();
            assert_eq!(task.summary.status, "Backlog");
            assert!(matches!(
                create(
                    &dir,
                    &TaskCreateParams {
                        status: Some("To Do".into()),
                        ..create_params("Sneaky")
                    },
                    agent
                ),
                Err(TaskError::Refused(_))
            ));
            assert!(matches!(
                update_as(agent, &id, status("To Do")),
                Err(TaskError::Refused(_))
            ));
            // A person promotes it; the agent picks it up.
            update_as(None, &id, status("To Do")).unwrap();
            update_as(agent, &id, status("In Progress")).unwrap();
            assert_eq!(
                update_as(agent, &id, status("Review")),
                Err(TaskError::NeedsPullRequest(id.clone()))
            );
            let reviewed = update_as(
                agent,
                &id,
                TaskUpdateParams {
                    add_references: vec!["https://github.com/o/r/pull/9".into()],
                    final_summary: Some("Adds the thing".into()),
                    comment: Some("Ready".into()),
                    ..status("Review")
                },
            )
            .unwrap();
            assert_eq!(reviewed.summary.status, "Review");
            assert_eq!(reviewed.final_summary.as_deref(), Some("Adds the thing"));
            assert_eq!(reviewed.comments[0].author.as_deref(), Some("@claude"));
            assert!(matches!(
                update_as(agent, &id, status("Done")),
                Err(TaskError::Refused(_))
            ));
            // A person approves; a proposal nobody wants is archived off the board.
            update_as(None, &id, status("Done")).unwrap();
            let unwanted = create(&dir, &create_params("Not needed"), agent).unwrap();
            let board = archive(&dir, &unwanted.summary.id).unwrap();
            assert!(board
                .tasks
                .iter()
                .all(|task| task.id != unwanted.summary.id));
            assert!(board.tasks.iter().any(|task| task.id == id));

            // A project set up outside herdr gets the missing columns.
            let plain = folder("plain");
            backlog(
                &plain,
                &[
                    "init",
                    "plain",
                    "--defaults",
                    "--no-git",
                    "--integration-mode",
                    "none",
                ],
            )
            .unwrap();
            assert_eq!(
                add_workflow(&plain).unwrap().statuses,
                [
                    "Backlog",
                    "To Do",
                    "In Progress",
                    "Review",
                    "Blocked",
                    "Done"
                ]
            );
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&plain);
        }

        #[test]
        fn two_spaces_stay_isolated() {
            if !available() {
                return;
            }
            let (one, two) = (folder("one"), folder("two"));
            init(&one).unwrap();
            init(&two).unwrap();
            create(&one, &create_params("Only in one"), None).unwrap();
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
