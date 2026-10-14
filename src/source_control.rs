//! Source control for one folder: its working-tree changes, history and the
//! git commands behind the git view. Reads and local changes (stage, discard,
//! undo) answer when done; a commit, push or pull starts a background
//! operation and answers at once, so a slow hook or network never holds up
//! the caller. Every function shells out to git and may block, so callers run
//! them off the main loop.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::api::schema::{
    GitChangeKind, GitCommitFile, GitCommitInfo, GitFileChange, GitFileDiff, GitHunk,
    GitOperationInfo, GitOperationKind, GitStatusInfo,
};

/// Most files listed by `status`; the rest only set `truncated`.
const MAX_FILES: usize = 5_000;
const MAX_STATUS_BYTES: usize = 4 << 20;
/// The sidebar count reads less: a few thousand entries.
const MAX_COUNT_BYTES: usize = 256 * 1024;
const CHANGE_COUNT_MAX_AGE: Duration = Duration::from_secs(3);
/// Most diff text returned for one file; very long lines are cut too.
const MAX_DIFF_BYTES: usize = 1 << 20;
const MAX_LINE_CHARS: usize = 1_000;
/// Most of an untracked file shown as its diff.
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024;
const MAX_BODY_CHARS: usize = 4_000;
pub(crate) const DEFAULT_LOG_LIMIT: u32 = 100;
const MAX_LOG_LIMIT: u32 = 500;
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Path bytes per git command line (Windows caps a whole command line at
/// 32,767 characters), so long path lists run in batches.
const MAX_PATH_ARG_BYTES: usize = 8_000;
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Stage, discard and undo answer within the client's 60-second wait.
const CHANGE_TIMEOUT: Duration = Duration::from_secs(55);
/// Commit hooks and the network, in background operations.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a stopped git gets to remove its lock files before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// After git exits, how long its output pipes may stay open: something it
/// started (an ssh control master) can keep stderr open indefinitely.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// Bumped after every change made through this module.
static CHANGE_EPOCH: AtomicU64 = AtomicU64::new(0);
static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitError {
    NotARepository,
    /// The hunk is no longer in the file's diff; the caller should refresh.
    HunkNotFound,
    /// The folder now resolves to another repository than the caller shows.
    FolderChanged,
    Rejected(String),
    Failed(String),
}

impl GitError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotARepository => "not_a_repository",
            Self::HunkNotFound => "hunk_not_found",
            Self::FolderChanged => "folder_changed",
            Self::Rejected(_) => "invalid_request",
            Self::Failed(_) => "git_failed",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::NotARepository => "not a git repository".to_owned(),
            Self::HunkNotFound => "that change moved; refresh and try again".to_owned(),
            Self::FolderChanged => {
                "this space now points at another repository; refresh and try again".to_owned()
            }
            Self::Rejected(message) | Self::Failed(message) => message.clone(),
        }
    }
}

type Result<T> = std::result::Result<T, GitError>;

pub(crate) fn status(cwd: &Path) -> Result<GitStatusInfo> {
    let root = toplevel(cwd)?;
    status_at(&root)
}

/// How many files `git status` lists as changed in `folder`'s repository,
/// untracked ones included. A huge count is cut at what fits in a small read.
pub(crate) fn changed_file_count(folder: &Path) -> Result<usize> {
    let output = run(
        git(folder).args(["status", "--porcelain=v2", "-z", "--untracked-files=all"]),
        None,
        READ_TIMEOUT,
        MAX_COUNT_BYTES,
    )?;
    Ok(parse_status(&output.stdout, output.truncated).files.len())
}

/// The change count for the sidebar: re-read at most every
/// [`CHANGE_COUNT_MAX_AGE`] per folder, and right after any change made here.
/// Edits never move HEAD, so this cannot hang off the branch fingerprint.
pub(crate) fn recent_changed_file_count(folder: &Path) -> Option<usize> {
    /// When it was read, the epoch then, how long reading took, and the count.
    type Counts = HashMap<PathBuf, (Instant, u64, Duration, Option<usize>)>;
    static COUNTS: Mutex<Option<Counts>> = Mutex::new(None);
    let epoch = CHANGE_EPOCH.load(Ordering::Acquire);
    let cached = lock(&COUNTS)
        .as_ref()
        .and_then(|counts| counts.get(folder).copied());
    if let Some((at, seen, took, count)) = cached {
        // A slow status (a huge checkout) is read less often: at most ~5% busy.
        let max_age = CHANGE_COUNT_MAX_AGE.max(took * 20);
        if seen == epoch && at.elapsed() < max_age {
            return count;
        }
    }
    let started = Instant::now();
    let count = changed_file_count(folder).ok();
    lock(&COUNTS).get_or_insert_with(HashMap::new).insert(
        folder.to_path_buf(),
        (Instant::now(), epoch, started.elapsed(), count),
    );
    count
}

pub(crate) fn diff(
    cwd: &Path,
    path: &str,
    orig_path: Option<&str>,
    commit: Option<&str>,
) -> Result<GitFileDiff> {
    let root = toplevel(cwd)?;
    let paths = checked_paths(path, orig_path)?;
    let mut diff = GitFileDiff {
        path: path.to_owned(),
        ..GitFileDiff::default()
    };
    if let Some(commit) = commit {
        let commit = checked_commit(commit)?;
        let output = run(
            diff_command(
                &root,
                &[
                    "show",
                    "--format=",
                    "--diff-merges=first-parent",
                    "-M",
                    commit,
                ],
            )
            .arg("--")
            .args(&paths),
            None,
            READ_TIMEOUT,
            MAX_DIFF_BYTES,
        )?;
        let parsed = parse_hunks(&output.stdout);
        diff.binary = parsed.binary;
        diff.truncated = output.truncated;
        diff.hunks = api_hunks(&parsed.hunks, &mut diff.truncated);
        return Ok(diff);
    }
    if is_untracked(&root, path)? {
        return untracked_diff(&root, path);
    }
    let staged = run(
        diff_command(&root, &["diff", "--cached", "-M"])
            .arg("--")
            .args(&paths),
        None,
        READ_TIMEOUT,
        MAX_DIFF_BYTES,
    )?;
    let unstaged = run(
        diff_command(&root, &["diff"]).arg("--").arg(path),
        None,
        READ_TIMEOUT,
        MAX_DIFF_BYTES,
    )?;
    diff.truncated = staged.truncated || unstaged.truncated;
    let (staged, unstaged) = (parse_hunks(&staged.stdout), parse_hunks(&unstaged.stdout));
    diff.binary = staged.binary || unstaged.binary;
    diff.hunks = api_hunks(&unstaged.hunks, &mut diff.truncated);
    diff.staged_hunks = api_hunks(&staged.hunks, &mut diff.truncated);
    Ok(diff)
}

/// Newest first, at most `limit` commits, and whether older ones exist.
pub(crate) fn log(cwd: &Path, limit: Option<u32>) -> Result<(Vec<GitCommitInfo>, bool)> {
    let root = toplevel(cwd)?;
    let limit = limit.unwrap_or(DEFAULT_LOG_LIMIT).clamp(1, MAX_LOG_LIMIT) as usize;
    if !has_head(&root)? {
        return Ok((Vec::new(), false));
    }
    let count = (limit + 1).to_string();
    let output = run(
        git(&root).args([
            "-c",
            "log.showSignature=false",
            "log",
            "-n",
            &count,
            "--format=%x1e%H%x1f%h%x1f%an%x1f%at%x1f%P%x1f%s%x1f%b%x1f",
            "--numstat",
            "--diff-merges=first-parent",
            "HEAD",
            "--",
        ]),
        None,
        READ_TIMEOUT,
        usize::MAX,
    )?;
    let unpushed = unpushed_commits(&root, &count)?;
    let mut commits = parse_log(&String::from_utf8_lossy(&output.stdout), &unpushed);
    let more = commits.len() > limit;
    commits.truncate(limit);
    Ok((commits, more))
}

/// The files a commit changed against its first parent.
pub(crate) fn show(cwd: &Path, commit: &str) -> Result<Vec<GitCommitFile>> {
    let root = toplevel(cwd)?;
    let commit = checked_commit(commit)?;
    let command = |stat: &str| {
        let mut command = git(&root);
        command.args([
            "-c",
            "log.showSignature=false",
            "show",
            "--format=",
            "--diff-merges=first-parent",
            "-M",
            stat,
            "-z",
            commit,
            "--",
        ]);
        command
    };
    let numstat = run(&mut command("--numstat"), None, READ_TIMEOUT, usize::MAX)?;
    let names = run(
        &mut command("--name-status"),
        None,
        READ_TIMEOUT,
        usize::MAX,
    )?;
    Ok(parse_commit_files(&numstat.stdout, &names.stdout))
}

// ---- local changes ----

/// Stages whole paths, or one hunk of a single path.
pub(crate) fn stage(
    cwd: &Path,
    expected_root: Option<&str>,
    paths: &[String],
    hunk: Option<&str>,
) -> Result<GitStatusInfo> {
    let paths = checked_path_list(paths)?;
    change(cwd, expected_root, |root| match hunk {
        Some(header) if !is_untracked(root, single_path(&paths)?)? => {
            apply_hunk(root, single_path(&paths)?, header, false)
        }
        // A new file has no index entry to apply a hunk to: stage it whole.
        _ => exec_paths(root, &["add", "-A"], &paths).map(drop),
    })
}

/// Unstages whole paths, or one staged hunk of a single path.
pub(crate) fn unstage(
    cwd: &Path,
    expected_root: Option<&str>,
    paths: &[String],
    hunk: Option<&str>,
) -> Result<GitStatusInfo> {
    let paths = checked_path_list(paths)?;
    change(cwd, expected_root, |root| {
        if let Some(header) = hunk {
            apply_hunk(root, single_path(&paths)?, header, true)
        } else if has_head(root)? {
            exec_paths(root, &["restore", "--staged"], &paths).map(drop)
        } else {
            // Before the first commit there is no HEAD to restore the index from.
            exec_paths(
                root,
                &["rm", "--cached", "-r", "-q", "--ignore-unmatch"],
                &paths,
            )
            .map(drop)
        }
    })
}

/// Puts tracked files back to their staged (or committed) content and deletes
/// untracked ones. Staged changes are kept; ignored files and nested
/// repositories are never touched.
pub(crate) fn discard(
    cwd: &Path,
    expected_root: Option<&str>,
    paths: &[String],
) -> Result<GitStatusInfo> {
    let paths = checked_path_list(paths)?;
    change(cwd, expected_root, |root| {
        let listed = exec_paths(root, &["ls-files", "-z"], &paths)?;
        let tracked: HashSet<&str> = nul_fields(&listed).collect();
        let (tracked, untracked): (Vec<String>, Vec<String>) = paths
            .iter()
            .cloned()
            .partition(|path| tracked.contains(path.as_str()));
        if !tracked.is_empty() {
            exec_paths(root, &["restore", "--worktree"], &tracked)?;
        }
        if !untracked.is_empty() {
            exec_paths(root, &["clean", "-f", "-q"], &untracked)?;
        }
        Ok(())
    })
}

/// Undoes `commit` when it is still HEAD and not pushed anywhere, keeping its
/// changes staged.
pub(crate) fn uncommit(
    cwd: &Path,
    expected_root: Option<&str>,
    commit: &str,
) -> Result<GitStatusInfo> {
    let commit = checked_commit(commit)?;
    change(cwd, expected_root, |root| {
        let rev = |rev: &str| run_text(git(root).args(["rev-parse", "--verify", "--quiet", rev]));
        let head = rev("HEAD^{commit}")
            .map_err(|_| GitError::Rejected("there is no commit to undo".to_owned()))?;
        let target = rev(&format!("{commit}^{{commit}}"))
            .map_err(|_| GitError::Rejected("commit not found".to_owned()))?;
        if head != target {
            return Err(GitError::Rejected(
                "only the newest commit can be undone".to_owned(),
            ));
        }
        if !unpushed_commits(root, "1")?.contains(&head) {
            return Err(GitError::Rejected(
                "this commit is already pushed; undoing it would rewrite shared history".to_owned(),
            ));
        }
        if rev("HEAD~1^{commit}").is_ok() {
            return exec(git(root).args(["reset", "--soft", "-q", "HEAD~1"]));
        }
        // The first commit: delete its branch, keeping the index. Never HEAD
        // itself, which a detached HEAD would turn into a broken repository.
        let branch = run_text(git(root).args(["symbolic-ref", "-q", "HEAD"])).map_err(|_| {
            GitError::Rejected(
                "HEAD is detached; check out a branch to undo this commit".to_owned(),
            )
        })?;
        exec(git(root).args(["update-ref", "-d", &branch, &head]))
    })
}

// ---- background operations ----

/// Starts a commit of the staged changes (every change with `all`).
pub(crate) fn commit(
    cwd: &Path,
    expected_root: Option<&str>,
    message: &str,
    all: bool,
) -> Result<GitStatusInfo> {
    let message = message.trim().to_owned();
    if message.is_empty() {
        return Err(GitError::Rejected(
            "write a commit message first".to_owned(),
        ));
    }
    start_operation(cwd, expected_root, GitOperationKind::Commit, move |root| {
        if all {
            run(
                git(root).args(["add", "-A"]),
                None,
                OPERATION_TIMEOUT,
                usize::MAX,
            )?;
        }
        run(
            git(root).args(["commit", "-q", "-F", "-"]),
            Some(message.as_bytes()),
            OPERATION_TIMEOUT,
            usize::MAX,
        )
        .map(drop)
    })
}

/// Starts a push, publishing the branch to its push remote when it has no
/// upstream yet.
pub(crate) fn push(cwd: &Path, expected_root: Option<&str>) -> Result<GitStatusInfo> {
    start_operation(cwd, expected_root, GitOperationKind::Push, |root| {
        let status = status_at(root)?;
        let Some(branch) = status.branch else {
            return Err(GitError::Rejected(
                "HEAD is detached; check out a branch to push".to_owned(),
            ));
        };
        if status.upstream.is_some() {
            run(git(root).arg("push"), None, OPERATION_TIMEOUT, usize::MAX)?;
        } else {
            let remote = push_remote(root, &branch)?;
            run(
                git(root).args(["push", "-u", &remote, "HEAD"]),
                None,
                OPERATION_TIMEOUT,
                usize::MAX,
            )?;
        }
        Ok(())
    })
}

/// Starts a pull from the branch's upstream.
pub(crate) fn pull(cwd: &Path, expected_root: Option<&str>) -> Result<GitStatusInfo> {
    start_operation(cwd, expected_root, GitOperationKind::Pull, |root| {
        if status_at(root)?.upstream.is_none() {
            return Err(GitError::Rejected(
                "this branch has no upstream to pull from".to_owned(),
            ));
        }
        run(
            git(root).args(["pull", "--no-edit"]),
            None,
            OPERATION_TIMEOUT,
            usize::MAX,
        )
        .map(drop)
    })
}

#[derive(Debug, Clone)]
struct OperationSlot {
    id: u64,
    kind: GitOperationKind,
    running: bool,
    error: Option<String>,
}

/// The latest operation per repository root.
static OPERATIONS: Mutex<Option<HashMap<PathBuf, OperationSlot>>> = Mutex::new(None);

fn operation_label(kind: GitOperationKind) -> &'static str {
    match kind {
        GitOperationKind::Commit => "commit",
        GitOperationKind::Push => "push",
        GitOperationKind::Pull => "pull",
        GitOperationKind::Unknown => "git operation",
    }
}

fn running_operation(root: &Path) -> Option<GitOperationKind> {
    lock(&OPERATIONS)
        .as_ref()?
        .get(root)
        .filter(|slot| slot.running)
        .map(|slot| slot.kind)
}

fn operation_info(root: &Path) -> Option<GitOperationInfo> {
    lock(&OPERATIONS)
        .as_ref()?
        .get(root)
        .map(|slot| GitOperationInfo {
            id: slot.id,
            kind: slot.kind,
            running: slot.running,
            error: slot.error.clone(),
        })
}

/// Runs `work` on its own thread under the folder lock and answers with the
/// status, which reports the operation until the next one replaces it.
fn start_operation(
    cwd: &Path,
    expected_root: Option<&str>,
    kind: GitOperationKind,
    work: impl FnOnce(&Path) -> Result<()> + Send + 'static,
) -> Result<GitStatusInfo> {
    let root = checked_root(cwd, expected_root)?;
    let id = {
        let mut operations = lock(&OPERATIONS);
        let operations = operations.get_or_insert_with(HashMap::new);
        if let Some(slot) = operations.get(&root).filter(|slot| slot.running) {
            return Err(GitError::Rejected(format!(
                "wait for the {} to finish",
                operation_label(slot.kind)
            )));
        }
        let id = NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed);
        operations.insert(
            root.clone(),
            OperationSlot {
                id,
                kind,
                running: true,
                error: None,
            },
        );
        id
    };
    let finish = move |root: &Path, error: Option<String>| {
        if let Some(slot) = lock(&OPERATIONS)
            .as_mut()
            .and_then(|operations| operations.get_mut(root))
            .filter(|slot| slot.id == id)
        {
            slot.running = false;
            slot.error = error;
        }
    };
    let worker_root = root.clone();
    let spawned = crate::thread_spawn::spawn_named("herdr-git-operation", move || {
        let result = locked(&worker_root, || work(&worker_root));
        finish(&worker_root, result.err().map(|err| err.message()));
    });
    if let Err(err) = spawned {
        let message = format!("could not start the {}: {err}", operation_label(kind));
        finish(&root, Some(message.clone()));
        return Err(GitError::Failed(message));
    }
    status_at(&root)
}

/// A local change: refused while an operation runs in this repository, so a
/// click never waits behind a push.
fn change(
    cwd: &Path,
    expected_root: Option<&str>,
    work: impl FnOnce(&Path) -> Result<()>,
) -> Result<GitStatusInfo> {
    let root = checked_root(cwd, expected_root)?;
    if let Some(kind) = running_operation(&root) {
        return Err(GitError::Rejected(format!(
            "wait for the {} to finish",
            operation_label(kind)
        )));
    }
    locked(&root, || {
        work(&root)?;
        status_at(&root)
    })
}

// ---- status ----

fn status_at(root: &Path) -> Result<GitStatusInfo> {
    let output = run(
        git(root).args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ]),
        None,
        READ_TIMEOUT,
        MAX_STATUS_BYTES,
    )?;
    let mut status = parse_status(&output.stdout, output.truncated);
    status.root = root.display().to_string();
    status.operation = operation_info(root);
    Ok(status)
}

fn parse_status(bytes: &[u8], cut: bool) -> GitStatusInfo {
    let mut status = GitStatusInfo::default();
    let mut fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    // A cut-off output ends mid-record; the final field is always incomplete
    // then (and empty otherwise, after the trailing NUL).
    fields.pop();
    let mut fields = fields.into_iter();
    while let Some(field) = fields.next() {
        let line = String::from_utf8_lossy(field);
        let mut record = || -> Option<GitFileChange> {
            match line.as_bytes().first()? {
                b'#' => {
                    parse_branch_header(&line, &mut status);
                    None
                }
                b'1' => {
                    let parts: Vec<&str> = line.splitn(9, ' ').collect();
                    let xy = parts.get(1)?.as_bytes();
                    Some(change_entry(parts.get(8)?, None, xy))
                }
                b'2' => {
                    let parts: Vec<&str> = line.splitn(10, ' ').collect();
                    let xy = parts.get(1)?.as_bytes();
                    let orig = String::from_utf8_lossy(fields.next()?).into_owned();
                    Some(change_entry(parts.get(9)?, Some(orig), xy))
                }
                b'u' => {
                    let parts: Vec<&str> = line.splitn(11, ' ').collect();
                    Some(GitFileChange {
                        path: (*parts.get(10)?).to_owned(),
                        orig_path: None,
                        kind: GitChangeKind::Conflicted,
                        staged: false,
                        unstaged: true,
                    })
                }
                b'?' => Some(GitFileChange {
                    path: line.get(2..)?.to_owned(),
                    orig_path: None,
                    kind: GitChangeKind::Untracked,
                    staged: false,
                    unstaged: true,
                }),
                _ => None,
            }
        };
        if let Some(change) = record() {
            if status.files.len() == MAX_FILES {
                status.truncated = true;
                break;
            }
            status.files.push(change);
        }
    }
    status.truncated |= cut;
    status
}

fn parse_branch_header(line: &str, status: &mut GitStatusInfo) {
    let Some(rest) = line.strip_prefix("# branch.") else {
        return;
    };
    let Some((key, value)) = rest.split_once(' ') else {
        return;
    };
    match key {
        "oid" if value != "(initial)" => status.head = Some(value.to_owned()),
        "head" if value != "(detached)" => status.branch = Some(value.to_owned()),
        "upstream" => status.upstream = Some(value.to_owned()),
        "ab" => {
            let mut counts = value.split(' ');
            let mut count = |sign: char| {
                counts
                    .next()
                    .and_then(|count| count.strip_prefix(sign))
                    .and_then(|count| count.parse().ok())
                    .unwrap_or(0)
            };
            status.ahead = count('+');
            status.behind = count('-');
        }
        _ => {}
    }
}

fn change_entry(path: &str, orig_path: Option<String>, xy: &[u8]) -> GitFileChange {
    let (x, y) = (
        xy.first().copied().unwrap_or(b'.'),
        xy.get(1).copied().unwrap_or(b'.'),
    );
    let either = |code: u8| x == code || y == code;
    let kind = if either(b'R') {
        GitChangeKind::Renamed
    } else if either(b'C') {
        GitChangeKind::Copied
    } else if either(b'A') {
        GitChangeKind::Added
    } else if either(b'D') {
        GitChangeKind::Deleted
    } else if either(b'T') {
        GitChangeKind::TypeChanged
    } else {
        GitChangeKind::Modified
    };
    GitFileChange {
        path: path.to_owned(),
        orig_path,
        kind,
        staged: x != b'.',
        unstaged: y != b'.',
    }
}

// ---- diffs ----

/// One `@@` hunk, as raw bytes: a patch built from it applies byte for byte,
/// whatever the file's encoding or line endings.
struct RawHunk<'a> {
    header: &'a [u8],
    old_start: u32,
    new_start: u32,
    lines: Vec<&'a [u8]>,
}

struct ParsedDiff<'a> {
    /// The single file's header lines: `diff --git`, modes, `---`, `+++`.
    file_header: Vec<&'a [u8]>,
    hunks: Vec<RawHunk<'a>>,
    binary: bool,
    renamed: bool,
}

/// Splits unified diff output into hunks. Hunk bodies are read by their line
/// counts, so a following file's `--- a/…` header is never taken as a line.
fn parse_hunks(text: &[u8]) -> ParsedDiff<'_> {
    let mut parsed = ParsedDiff {
        file_header: Vec::new(),
        hunks: Vec::new(),
        binary: false,
        renamed: false,
    };
    let (mut old_left, mut new_left) = (0u32, 0u32);
    for line in text.split(|byte| *byte == b'\n') {
        if old_left > 0 || new_left > 0 {
            if let Some(hunk) = parsed.hunks.last_mut() {
                match line.first() {
                    Some(b'+') => new_left = new_left.saturating_sub(1),
                    Some(b'-') => old_left = old_left.saturating_sub(1),
                    Some(b'\\') => {}
                    // An empty line is a context line whose leading space was lost.
                    _ => {
                        old_left = old_left.saturating_sub(1);
                        new_left = new_left.saturating_sub(1);
                    }
                }
                hunk.lines.push(line);
                continue;
            }
        }
        if line.starts_with(b"\\") {
            if let Some(hunk) = parsed.hunks.last_mut() {
                hunk.lines.push(line);
            }
        } else if let Some((old_start, old_count, new_start, new_count)) =
            hunk_range(&String::from_utf8_lossy(line))
        {
            parsed.hunks.push(RawHunk {
                header: line,
                old_start,
                new_start,
                lines: Vec::new(),
            });
            (old_left, new_left) = (old_count, new_count);
        } else if line.starts_with(b"Binary files ") || line == b"GIT binary patch" {
            parsed.binary = true;
        } else if line.starts_with(b"diff --git ") {
            parsed.file_header = vec![line];
        } else if parsed.hunks.is_empty() && !parsed.file_header.is_empty() && !line.is_empty() {
            parsed.renamed |= line.starts_with(b"rename from ");
            parsed.file_header.push(line);
        }
    }
    parsed
}

/// `@@ -a[,b] +c[,d] @@…` → (a, b, c, d); a missing count is 1.
fn hunk_range(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some((old_start, old_count, new_start, new_count))
}

fn api_hunks(hunks: &[RawHunk<'_>], truncated: &mut bool) -> Vec<GitHunk> {
    hunks
        .iter()
        .map(|hunk| GitHunk {
            header: String::from_utf8_lossy(hunk.header).into_owned(),
            old_start: hunk.old_start,
            new_start: hunk.new_start,
            lines: hunk
                .lines
                .iter()
                .map(|line| api_line(&String::from_utf8_lossy(line), truncated))
                .collect(),
        })
        .collect()
}

fn api_line(line: &str, truncated: &mut bool) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => {
            *truncated = true;
            format!("{}…", &line[..cut])
        }
        None => line.to_owned(),
    }
}

fn untracked_diff(root: &Path, path: &str) -> Result<GitFileDiff> {
    let mut diff = GitFileDiff {
        path: path.to_owned(),
        ..GitFileDiff::default()
    };
    let full = root.join(path);
    // A directory (nested repository) or a file removed meanwhile: nothing to show.
    if full.is_dir() {
        return Ok(diff);
    }
    let Ok(file) = std::fs::File::open(&full) else {
        return Ok(diff);
    };
    let mut bytes = Vec::new();
    file.take(MAX_UNTRACKED_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| GitError::Failed(err.to_string()))?;
    if bytes.iter().take(8_000).any(|byte| *byte == 0) {
        diff.binary = true;
        return Ok(diff);
    }
    if bytes.len() as u64 > MAX_UNTRACKED_BYTES {
        bytes.truncate(MAX_UNTRACKED_BYTES as usize);
        diff.truncated = true;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut text_lines: Vec<&str> = text.lines().collect();
    if diff.truncated {
        // The last line may be cut mid-way.
        text_lines.pop();
    }
    if text_lines.is_empty() {
        return Ok(diff);
    }
    let mut truncated = diff.truncated;
    let lines = text_lines
        .iter()
        .map(|line| api_line(&format!("+{line}"), &mut truncated))
        .collect();
    diff.truncated = truncated;
    diff.hunks = vec![GitHunk {
        header: format!("@@ -0,0 +1,{} @@", text_lines.len()),
        old_start: 0,
        new_start: 1,
        lines,
    }];
    Ok(diff)
}

/// Stages (or, `reverse`, unstages) the hunk with this header by applying it
/// to the index. The diff is read again so a changed file fails cleanly, and
/// the patch keeps git's own file header, so new and deleted files apply as
/// such.
fn apply_hunk(root: &Path, path: &str, header: &str, reverse: bool) -> Result<()> {
    // A path-limited diff cannot see a rename's other half, so ask the status.
    let renamed = status_at(root)?
        .files
        .iter()
        .any(|file| file.path == path && file.orig_path.is_some());
    if renamed {
        return Err(GitError::Rejected(
            "stage or unstage a renamed file as a whole".to_owned(),
        ));
    }
    let args: &[&str] = if reverse {
        &["diff", "--cached"]
    } else {
        &["diff"]
    };
    let output = run(
        diff_command(root, args).arg("--").arg(path),
        None,
        READ_TIMEOUT,
        usize::MAX,
    )?;
    let parsed = parse_hunks(&output.stdout);
    if parsed.renamed {
        return Err(GitError::Rejected(
            "stage or unstage a renamed file as a whole".to_owned(),
        ));
    }
    let hunk = parsed
        .hunks
        .iter()
        .find(|hunk| String::from_utf8_lossy(hunk.header) == header)
        .ok_or(GitError::HunkNotFound)?;
    if parsed.file_header.is_empty() {
        return Err(GitError::HunkNotFound);
    }
    let mut patch = Vec::new();
    for line in parsed
        .file_header
        .iter()
        .chain(std::iter::once(&hunk.header))
        .chain(&hunk.lines)
    {
        patch.extend_from_slice(line);
        patch.push(b'\n');
    }
    let mut command = git(root);
    command.args(["apply", "--cached", "--whitespace=nowarn"]);
    if reverse {
        command.arg("--reverse");
    }
    run(command.arg("-"), Some(&patch), CHANGE_TIMEOUT, usize::MAX).map(drop)
}

// ---- history ----

fn parse_log(text: &str, unpushed: &HashSet<String>) -> Vec<GitCommitInfo> {
    text.split('\x1e')
        .filter_map(|record| {
            let fields: Vec<&str> = record.splitn(8, '\x1f').collect();
            let [commit, short_commit, author, time, parents, subject, body, stats] = fields[..]
            else {
                return None;
            };
            let mut info = GitCommitInfo {
                commit: commit.trim().to_owned(),
                short_commit: short_commit.to_owned(),
                author: author.to_owned(),
                time: time.parse().unwrap_or(0),
                subject: subject.to_owned(),
                body: cut_chars(body.trim(), MAX_BODY_CHARS),
                merge: parents
                    .split(' ')
                    .filter(|parent| !parent.is_empty())
                    .count()
                    > 1,
                pushed: false,
                files_changed: 0,
                insertions: 0,
                deletions: 0,
            };
            info.pushed = !unpushed.contains(&info.commit);
            for line in stats.lines() {
                let mut parts = line.splitn(3, '\t');
                let (Some(added), Some(deleted), Some(_path)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                info.files_changed += 1;
                info.insertions += added.parse::<u32>().unwrap_or(0);
                info.deletions += deleted.parse::<u32>().unwrap_or(0);
            }
            Some(info)
        })
        .collect()
}

/// `--numstat -z` and `--name-status -z` of the same diff, joined in order.
fn parse_commit_files(numstat: &[u8], names: &[u8]) -> Vec<GitCommitFile> {
    let mut kinds = Vec::new();
    let mut fields = names
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(status) = fields.next() {
        let kind = match status.first() {
            Some(b'A') => GitChangeKind::Added,
            Some(b'D') => GitChangeKind::Deleted,
            Some(b'R') => GitChangeKind::Renamed,
            Some(b'C') => GitChangeKind::Copied,
            Some(b'T') => GitChangeKind::TypeChanged,
            Some(b'U') => GitChangeKind::Conflicted,
            Some(b'M') => GitChangeKind::Modified,
            _ => GitChangeKind::Unknown,
        };
        let paired = matches!(kind, GitChangeKind::Renamed | GitChangeKind::Copied);
        let first = fields
            .next()
            .map(|field| String::from_utf8_lossy(field).into_owned());
        let second = if paired {
            fields
                .next()
                .map(|field| String::from_utf8_lossy(field).into_owned())
        } else {
            None
        };
        let Some(first) = first else { break };
        let (path, orig_path) = match second {
            Some(second) => (second, Some(first)),
            None => (first, None),
        };
        kinds.push((path, orig_path, kind));
    }
    let mut files = Vec::new();
    let mut fields = numstat.split(|byte| *byte == 0);
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(record);
        let mut parts = record.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // A rename leaves the path empty and puts the old and new paths in the next two fields.
        let path = if path.is_empty() {
            let _orig = fields.next();
            fields
                .next()
                .map(|field| String::from_utf8_lossy(field).into_owned())
                .unwrap_or_default()
        } else {
            path.to_owned()
        };
        let index = files.len();
        let (orig_path, kind) = match kinds.get(index) {
            Some((name, orig, kind)) if *name == path => (orig.clone(), *kind),
            _ => (None, GitChangeKind::Modified),
        };
        files.push(GitCommitFile {
            path,
            orig_path,
            kind,
            insertions: added.parse().unwrap_or(0),
            deletions: deleted.parse().unwrap_or(0),
            binary: added == "-",
        });
    }
    files
}

/// Commits reachable from HEAD but from no remote-tracking branch, newest first.
fn unpushed_commits(root: &Path, max: &str) -> Result<HashSet<String>> {
    let output = run(
        git(root).args(["rev-list", "--max-count", max, "HEAD", "--not", "--remotes"]),
        None,
        READ_TIMEOUT,
        usize::MAX,
    )?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

fn push_remote(root: &Path, branch: &str) -> Result<String> {
    for key in [
        format!("branch.{branch}.pushRemote"),
        "remote.pushDefault".to_owned(),
    ] {
        if let Ok(remote) = run_text(git(root).args(["config", "--get", &key])) {
            if !remote.is_empty() {
                return Ok(remote);
            }
        }
    }
    let remotes = run_text(git(root).arg("remote")).unwrap_or_default();
    let remotes: Vec<&str> = remotes.lines().collect();
    if remotes.contains(&"origin") {
        return Ok("origin".to_owned());
    }
    match remotes[..] {
        [only] => Ok(only.to_owned()),
        [] => Err(GitError::Rejected(
            "this repository has no remote to push to".to_owned(),
        )),
        _ => Err(GitError::Rejected(
            "several remotes and none is origin; push once from a terminal to pick one".to_owned(),
        )),
    }
}

// ---- helpers ----

fn toplevel(cwd: &Path) -> Result<PathBuf> {
    match run_text(git(cwd).args(["rev-parse", "--show-toplevel"])) {
        Ok(root) if !root.is_empty() => Ok(PathBuf::from(root)),
        // A bare repository or the inside of `.git` has no working tree either.
        Ok(_) => Err(GitError::NotARepository),
        Err(GitError::Failed(message)) if !message.contains("not a git repository") => {
            Err(GitError::Failed(message))
        }
        Err(_) => Err(GitError::NotARepository),
    }
}

/// The repository root, refused when it is not the one the caller shows.
fn checked_root(cwd: &Path, expected: Option<&str>) -> Result<PathBuf> {
    let root = toplevel(cwd)?;
    match expected {
        Some(expected) if expected != root.display().to_string() => Err(GitError::FolderChanged),
        _ => Ok(root),
    }
}

fn has_head(root: &Path) -> Result<bool> {
    let output = run_raw(
        git(root).args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
        None,
        READ_TIMEOUT,
        usize::MAX,
    )?;
    Ok(output.0.success())
}

fn is_untracked(root: &Path, path: &str) -> Result<bool> {
    let tracked = run(
        git(root).args(["ls-files", "-z", "--"]).arg(path),
        None,
        READ_TIMEOUT,
        usize::MAX,
    )?;
    Ok(tracked.stdout.is_empty() && root.join(path).symlink_metadata().is_ok())
}

/// Paths must stay inside the repository: relative, without `..`.
fn checked_path(path: &str) -> Result<&str> {
    let valid = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    valid
        .then_some(path)
        .ok_or_else(|| GitError::Rejected(format!("invalid path: {path}")))
}

fn checked_paths<'a>(path: &'a str, orig_path: Option<&'a str>) -> Result<Vec<&'a str>> {
    let mut paths = vec![checked_path(path)?];
    if let Some(orig) = orig_path {
        paths.push(checked_path(orig)?);
    }
    Ok(paths)
}

fn checked_path_list(paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() {
        return Err(GitError::Rejected("no paths given".to_owned()));
    }
    paths
        .iter()
        .map(|path| checked_path(path).map(str::to_owned))
        .collect()
}

fn single_path(paths: &[String]) -> Result<&str> {
    match paths {
        [path] => Ok(path),
        _ => Err(GitError::Rejected(
            "a hunk needs exactly one path".to_owned(),
        )),
    }
}

/// Only commit ids are accepted, never refs or options.
fn checked_commit(commit: &str) -> Result<&str> {
    let valid = (4..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit());
    valid
        .then_some(commit)
        .ok_or_else(|| GitError::Rejected(format!("invalid commit: {commit}")))
}

fn cut_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

fn nul_fields(bytes: &[u8]) -> impl Iterator<Item = &str> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .filter_map(|field| std::str::from_utf8(field).ok())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs `work` while holding this folder's lock: one mutating git command per
/// folder at a time, so two clicks never race for the index lock, while other
/// folders are not held up by a slow push.
fn locked<T>(root: &Path, work: impl FnOnce() -> Result<T>) -> Result<T> {
    static LOCKS: Mutex<Option<HashMap<PathBuf, Arc<Mutex<()>>>>> = Mutex::new(None);
    let folder = Arc::clone(
        lock(&LOCKS)
            .get_or_insert_with(HashMap::new)
            .entry(root.to_path_buf())
            .or_default(),
    );
    let _guard = lock(&folder);
    let result = work();
    // Even a failed command may have changed files (a half-applied pull).
    CHANGE_EPOCH.fetch_add(1, Ordering::Release);
    result
}

fn git(dir: &Path) -> Command {
    let mut command = crate::noninteractive_process::command("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off", "-c", "color.ui=false"])
        // Paths are file names, never glob patterns.
        .env("GIT_LITERAL_PATHSPECS", "1")
        // Reads never take the index lock an agent's git command may need.
        .env("GIT_OPTIONAL_LOCKS", "0")
        // Fail instead of waiting for a password nobody can type.
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

/// A diff whose hunks can be applied back: no external or text-converted diffs.
fn diff_command(root: &Path, args: &[&str]) -> Command {
    let mut command = git(root);
    command.args(args).args([
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-U3",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ]);
    command
}

/// Runs `git <args> -- <paths>` in batches that keep each command line short,
/// returning everything they printed.
fn exec_paths(root: &Path, args: &[&str], paths: &[String]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut start = 0;
    while start < paths.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < paths.len() && (end == start || bytes + paths[end].len() < MAX_PATH_ARG_BYTES) {
            bytes += paths[end].len() + 1;
            end += 1;
        }
        let batch = paths[start..end].iter().map(OsStr::new);
        let result = run(
            git(root).args(args).arg("--").args(batch),
            None,
            CHANGE_TIMEOUT,
            usize::MAX,
        )?;
        output.extend_from_slice(&result.stdout);
        start = end;
    }
    Ok(output)
}

struct Output {
    stdout: Vec<u8>,
    /// Stdout hit its cap; git was stopped early.
    truncated: bool,
}

fn run(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    cap: usize,
) -> Result<Output> {
    let (status, stdout, truncated, stderr) = run_raw(command, input, timeout, cap)?;
    if !status.success() && !truncated {
        let stderr = String::from_utf8_lossy(&stderr);
        let message = stderr.trim();
        return Err(GitError::Failed(if message.is_empty() {
            format!("git failed ({status})")
        } else {
            message.to_owned()
        }));
    }
    Ok(Output { stdout, truncated })
}

fn exec(command: &mut Command) -> Result<()> {
    run(command, None, CHANGE_TIMEOUT, usize::MAX).map(drop)
}

fn run_text(command: &mut Command) -> Result<String> {
    run(command, None, READ_TIMEOUT, usize::MAX)
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Runs git in its own process group with stdin fed and stdout/stderr read on
/// helper threads (so no pipe can fill up and stall it), stopping the whole
/// group after `timeout`. Stdout past `cap` is not read: git ends on the
/// closed pipe. Stderr is always drained, since hooks write there.
fn run_raw(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    cap: usize,
) -> Result<(ExitStatus, Vec<u8>, bool, Vec<u8>)> {
    crate::platform::isolate_process_tree(command);
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitError::Failed(format!("could not run git: {err}")))?;
    let failed = |err: io::Error| GitError::Failed(format!("could not run git: {err}"));
    if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
        let input = input.to_vec();
        crate::thread_spawn::spawn_named("herdr-git-stdin", move || {
            // A git that exits early closes the pipe; that error is git's to report.
            let _ = stdin.write_all(&input);
        })
        .map_err(failed)?;
    }
    let stdout = read_pipe(child.stdout.take(), cap, false).map_err(failed)?;
    let stderr = read_pipe(child.stderr.take(), MAX_STDERR_BYTES, true).map_err(failed)?;
    let status =
        wait_with_timeout(&mut child, timeout).map_err(|err| GitError::Failed(err.to_string()))?;
    let (stdout, truncated) = stdout.recv_timeout(PIPE_GRACE).unwrap_or_default();
    let (stderr, _) = stderr.recv_timeout(PIPE_GRACE).unwrap_or_default();
    Ok((status, stdout, truncated, stderr))
}

/// Reads a pipe on its own thread. A reader blocked by a process that
/// outlives git is simply left behind; nothing waits for it.
fn read_pipe(
    pipe: Option<impl Read + Send + 'static>,
    cap: usize,
    drain: bool,
) -> io::Result<mpsc::Receiver<(Vec<u8>, bool)>> {
    let (sender, receiver) = mpsc::channel();
    match pipe {
        Some(pipe) => {
            crate::thread_spawn::spawn_named("herdr-git-pipe", move || {
                let _ = sender.send(read_capped(pipe, cap, drain));
            })?;
        }
        None => {
            let _ = sender.send((Vec::new(), false));
        }
    }
    Ok(receiver)
}

fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    let mut pause = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            crate::platform::stop_process_tree(child, STOP_GRACE);
            return Err(io::Error::other(format!(
                "git took longer than {}s and was stopped",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(25));
    }
}

/// Reads up to `cap` bytes. Past the cap it either keeps reading and drops
/// the rest (`drain`), or stops so the writer ends on a closed pipe.
fn read_capped(mut reader: impl Read, cap: usize, drain: bool) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return (out, truncated),
            Ok(read) => {
                let room = cap - out.len();
                out.extend_from_slice(&chunk[..read.min(room)]);
                if read > room {
                    truncated = true;
                    if !drain {
                        return (out, true);
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return (out, truncated),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh repository isolated from the user's git config where it matters.
    fn repo(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-source-control-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        sh(&root, &["init", "-q", "-b", "main"]);
        configure(&root);
        root
    }

    fn configure(root: &Path) {
        for (key, value) in [
            ("user.name", "t"),
            ("user.email", "t@t"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "no-hooks"),
            ("core.autocrlf", "false"),
            ("pull.rebase", "false"),
        ] {
            sh(root, &["config", key, value]);
        }
    }

    fn sh(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn write(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }

    fn read(root: &Path, path: &str) -> Option<String> {
        std::fs::read_to_string(root.join(path)).ok()
    }

    fn numbered(count: usize) -> String {
        (1..=count).map(|n| format!("line {n}\n")).collect()
    }

    fn file<'a>(status: &'a GitStatusInfo, path: &str) -> &'a GitFileChange {
        status
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} not in {:?}", status.files))
    }

    /// Waits for the operation `started` reported and returns its outcome.
    fn outcome(root: &Path, started: GitStatusInfo) -> (GitStatusInfo, Option<String>) {
        let id = started.operation.as_ref().expect("an operation").id;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let now = status(root).unwrap();
            let operation = now.operation.clone().expect("operation info");
            if operation.id != id || !operation.running {
                assert_eq!(operation.id, id, "a newer operation replaced it");
                return (now, operation.error);
            }
            assert!(Instant::now() < deadline, "operation never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finished(root: &Path, started: Result<GitStatusInfo>) -> GitStatusInfo {
        let (status, error) = outcome(root, started.unwrap());
        assert_eq!(error, None);
        status
    }

    fn committed(root: &Path, message: &str) -> GitStatusInfo {
        finished(root, commit(root, None, message, true))
    }

    #[cfg(unix)]
    fn hook(root: &Path, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let hooks = root.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        sh(root, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    }

    #[test]
    fn status_parses_branch_headers_and_every_entry_kind() {
        let bytes = b"# branch.oid 1234abcd\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +3 -2\0\
1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
1 A. N... 000000 100644 100644 000 ccc new.rs\0\
2 R. N... 100644 100644 100644 ddd ddd R100 moved.rs\0orig.rs\0\
u UU N... 100644 100644 100644 100644 e f g conflict.rs\0\
? notes.md\0";
        let status = parse_status(bytes, false);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.head.as_deref(), Some("1234abcd"));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (3, 2));
        assert!(!status.truncated);
        let summary: Vec<_> = status
            .files
            .iter()
            .map(|f| {
                (
                    f.path.as_str(),
                    f.orig_path.as_deref(),
                    f.kind,
                    f.staged,
                    f.unstaged,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("src/a b.rs", None, GitChangeKind::Modified, false, true),
                ("new.rs", None, GitChangeKind::Added, true, false),
                (
                    "moved.rs",
                    Some("orig.rs"),
                    GitChangeKind::Renamed,
                    true,
                    false
                ),
                ("conflict.rs", None, GitChangeKind::Conflicted, false, true),
                ("notes.md", None, GitChangeKind::Untracked, false, true),
            ]
        );
    }

    #[test]
    fn status_drops_a_record_cut_off_by_the_output_cap() {
        let status = parse_status(b"# branch.head (detached)\0? one.md\0? tw", true);
        assert_eq!(status.branch, None);
        assert_eq!(status.files.len(), 1);
        assert!(status.truncated);
        // A rename whose old path was cut off is dropped too.
        let status = parse_status(b"2 R. N... 1 1 1 a a R100 new.rs\0", true);
        assert!(status.files.is_empty());
    }

    #[test]
    fn hunks_are_read_by_their_counts_not_by_line_prefixes() {
        let text = b"diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1,3 +1 @@ fn x()\n keep\n--- a/looks-like-a-header\n-gone\n\\ No newline at end of file\n@@ -10 +9,2 @@\n-old\n+new\n+more\n";
        let parsed = parse_hunks(text);
        assert!(!parsed.binary && !parsed.renamed);
        assert_eq!(
            parsed.file_header,
            [
                &b"diff --git a/x b/x"[..],
                b"index 1..2 100644",
                b"--- a/x",
                b"+++ b/x"
            ]
        );
        assert_eq!(parsed.hunks.len(), 2);
        let first = &parsed.hunks[0];
        assert_eq!(
            (first.header, first.old_start, first.new_start),
            (&b"@@ -1,3 +1 @@ fn x()"[..], 1, 1)
        );
        assert_eq!(
            first.lines,
            [
                &b" keep"[..],
                b"--- a/looks-like-a-header",
                b"-gone",
                b"\\ No newline at end of file"
            ]
        );
        assert_eq!(parsed.hunks[1].lines, [&b"-old"[..], b"+new", b"+more"]);
        assert_eq!(hunk_range("@@ -10 +9,2 @@"), Some((10, 1, 9, 2)));
        assert_eq!(hunk_range("@@ -0,0 +1 @@"), Some((0, 0, 1, 1)));
        assert_eq!(hunk_range("@@ broken"), None);
        assert!(parse_hunks(b"Binary files a/x and b/x differ\n").binary);
        let renamed =
            parse_hunks(b"diff --git a/o b/n\nsimilarity index 90%\nrename from o\nrename to n\n");
        assert!(renamed.renamed);
    }

    #[test]
    fn long_lines_are_cut_for_display_and_flagged() {
        let long = format!("+{}", "é".repeat(MAX_LINE_CHARS + 50));
        let hunk = RawHunk {
            header: b"@@ -0,0 +1 @@",
            old_start: 0,
            new_start: 1,
            lines: vec![long.as_bytes()],
        };
        let mut truncated = false;
        let hunks = api_hunks(&[hunk], &mut truncated);
        assert!(truncated);
        assert_eq!(hunks[0].lines[0].chars().count(), MAX_LINE_CHARS + 1);
        assert!(hunks[0].lines[0].ends_with('…'));
    }

    #[test]
    fn log_records_carry_numstat_totals_merges_and_push_state() {
        let text = "\x1eaaa111\x1faaa\x1fAda\x1f100\x1fp1\x1ffix: one\x1fbody line\n\x1f\n3\t1\tsrc/a.rs\n-\t-\timg.png\n\
\x1ebbb222\x1fbbb\x1fBo\x1f90\x1fp1 p2\x1fMerge x\x1f\x1f\n";
        let unpushed: HashSet<String> = ["aaa111".to_owned()].into();
        let commits = parse_log(text, &unpushed);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].subject, "fix: one");
        assert_eq!(commits[0].body, "body line");
        assert_eq!(
            (
                commits[0].files_changed,
                commits[0].insertions,
                commits[0].deletions
            ),
            (2, 3, 1)
        );
        assert!(!commits[0].pushed && !commits[0].merge);
        assert!(commits[1].pushed && commits[1].merge);
        assert_eq!(commits[1].files_changed, 0);
    }

    #[test]
    fn commit_files_join_numstat_with_name_status_including_renames() {
        let numstat = b"2\t0\tnew.rs\x000\t1\t\0old.rs\0moved.rs\0-\t-\timg.png\0";
        let names = b"A\0new.rs\0R090\0old.rs\0moved.rs\0M\0img.png\0";
        let files = parse_commit_files(numstat, names);
        let summary: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.path.as_str(),
                    f.orig_path.as_deref(),
                    f.kind,
                    f.insertions,
                    f.deletions,
                    f.binary,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("new.rs", None, GitChangeKind::Added, 2, 0, false),
                (
                    "moved.rs",
                    Some("old.rs"),
                    GitChangeKind::Renamed,
                    0,
                    1,
                    false
                ),
                ("img.png", None, GitChangeKind::Modified, 0, 0, true),
            ]
        );
    }

    #[test]
    fn inputs_are_checked_before_reaching_git() {
        assert!(checked_path("src/a b.rs").is_ok());
        for bad in ["", "../x", "/etc/passwd", "a/../b", "./a"] {
            assert!(checked_path(bad).is_err(), "{bad}");
        }
        assert!(checked_commit("1a2b3c4d").is_ok());
        for bad in ["HEAD", "--output=x", "abc", "zzzz1234"] {
            assert!(checked_commit(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_and_diff_show_staged_unstaged_and_untracked_changes() {
        let root = repo("status");
        write(&root, "a.rs", &numbered(30));
        write(&root, "gone.rs", "bye\n");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-q", "-m", "init"]);
        write(
            &root,
            "a.rs",
            &numbered(30).replace("line 2\n", "line 2 staged\n"),
        );
        sh(&root, &["add", "a.rs"]);
        write(
            &root,
            "a.rs",
            &numbered(30)
                .replace("line 2\n", "line 2 staged\n")
                .replace("line 29\n", "line 29 unstaged\n"),
        );
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        write(&root, "dir/notes.md", "one\ntwo\n");

        let status = status(&root.join("dir")).unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.head.is_some() && status.operation.is_none());
        let a = file(&status, "a.rs");
        assert!(a.staged && a.unstaged);
        assert_eq!(file(&status, "gone.rs").kind, GitChangeKind::Deleted);
        assert_eq!(file(&status, "dir/notes.md").kind, GitChangeKind::Untracked);
        assert_eq!(changed_file_count(&root).unwrap(), 3);

        let diff = diff(&root, "a.rs", None, None).unwrap();
        assert_eq!(diff.staged_hunks.len(), 1);
        assert!(diff.staged_hunks[0]
            .lines
            .contains(&"+line 2 staged".to_owned()));
        assert_eq!(diff.hunks.len(), 1);
        assert!(diff.hunks[0]
            .lines
            .contains(&"+line 29 unstaged".to_owned()));

        let new = super::diff(&root, "dir/notes.md", None, None).unwrap();
        assert_eq!(new.hunks[0].header, "@@ -0,0 +1,2 @@");
        assert_eq!(new.hunks[0].lines, ["+one", "+two"]);
        assert!(super::diff(&root, "../outside", None, None).is_err());
    }

    #[test]
    fn staging_one_hunk_leaves_the_other_unstaged_and_unstaging_reverses_it() {
        let root = repo("hunks");
        write(&root, "a.rs", &numbered(40));
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-q", "-m", "init"]);
        write(
            &root,
            "a.rs",
            &numbered(40)
                .replace("line 3\n", "line 3 top\n")
                .replace("line 38\n", "line 38 bottom\n"),
        );

        let first = diff(&root, "a.rs", None, None).unwrap();
        assert_eq!(first.hunks.len(), 2);
        let top = first.hunks[0].header.clone();
        let status = stage(&root, None, &["a.rs".to_owned()], Some(&top)).unwrap();
        assert!(file(&status, "a.rs").staged && file(&status, "a.rs").unstaged);
        let staged = sh(&root, &["diff", "--cached"]);
        assert!(staged.contains("+line 3 top") && !staged.contains("bottom"));

        assert_eq!(
            stage(
                &root,
                None,
                &["a.rs".to_owned()],
                Some("@@ -1,1 +1,1 @@ gone")
            ),
            Err(GitError::HunkNotFound)
        );

        let cached = diff(&root, "a.rs", None, None).unwrap();
        let status = unstage(
            &root,
            None,
            &["a.rs".to_owned()],
            Some(&cached.staged_hunks[0].header),
        )
        .unwrap();
        assert!(!file(&status, "a.rs").staged);
        assert_eq!(
            read(&root, "a.rs").unwrap().matches("top").count(),
            1,
            "working tree untouched"
        );
    }

    #[test]
    fn hunks_stage_cleanly_in_crlf_files_odd_paths_and_other_encodings() {
        let root = repo("crlf");
        let paths = [
            "dir with space/ünï \"q\".rs",
            "plain space.rs",
            "glob[*].rs",
        ];
        let crlf: String = (1..=20).map(|n| format!("row {n}\r\n")).collect();
        for path in paths {
            write(&root, path, &crlf);
        }
        // Latin-1 bytes are not UTF-8: the patch must still match byte for byte.
        let latin1 = |marker: &str| -> Vec<u8> {
            let mut bytes = Vec::new();
            for n in 1..=20 {
                bytes.extend_from_slice(b"caf\xe9 ");
                bytes.extend_from_slice(n.to_string().as_bytes());
                if n == 2 || n == 19 {
                    bytes.extend_from_slice(marker.as_bytes());
                }
                bytes.push(b'\n');
            }
            bytes
        };
        std::fs::write(root.join("latin1.txt"), latin1("")).unwrap();
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-q", "-m", "init"]);
        let changed = crlf
            .replace("row 2\r\n", "row 2 changed\r\n")
            .replace("row 19\r\n", "row 19 changed\r\n");
        for path in paths {
            write(&root, path, &changed);
            let hunks = diff(&root, path, None, None).unwrap().hunks;
            assert_eq!(hunks.len(), 2, "{path}");
            stage(&root, None, &[path.to_owned()], Some(&hunks[1].header)).unwrap();
            let staged = sh(&root, &["diff", "--cached", "--", path]);
            assert!(staged.contains("row 19 changed\r"), "{path}: {staged}");
            assert!(!staged.contains("row 2 changed"), "{path}");
        }
        std::fs::write(root.join("latin1.txt"), latin1(" x")).unwrap();
        let hunks = diff(&root, "latin1.txt", None, None).unwrap().hunks;
        assert_eq!(hunks.len(), 2);
        stage(
            &root,
            None,
            &["latin1.txt".to_owned()],
            Some(&hunks[0].header),
        )
        .unwrap();
        let staged = sh(&root, &["diff", "--cached", "--stat", "--", "latin1.txt"]);
        assert!(staged.contains("1 file changed"), "{staged}");
    }

    #[test]
    fn hunks_of_new_and_deleted_files_stage_as_additions_and_deletions() {
        let root = repo("new-deleted");
        write(&root, "seed.rs", "x\n");
        write(&root, "doomed.rs", "a\nb\n");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-q", "-m", "init"]);

        // A new file's only hunk stages the whole file, and unstaging takes it back out.
        write(&root, "new.rs", "a\nb\n");
        let status = stage(&root, None, &["new.rs".to_owned()], Some("@@ -0,0 +1,2 @@")).unwrap();
        let new = file(&status, "new.rs");
        assert_eq!(
            (new.kind, new.staged, new.unstaged),
            (GitChangeKind::Added, true, false)
        );
        let header = diff(&root, "new.rs", None, None).unwrap().staged_hunks[0]
            .header
            .clone();
        let status = unstage(&root, None, &["new.rs".to_owned()], Some(&header)).unwrap();
        assert_eq!(file(&status, "new.rs").kind, GitChangeKind::Untracked);

        // A deleted file's only hunk stages the deletion, not an empty file.
        std::fs::remove_file(root.join("doomed.rs")).unwrap();
        let header = diff(&root, "doomed.rs", None, None).unwrap().hunks[0]
            .header
            .clone();
        let status = stage(&root, None, &["doomed.rs".to_owned()], Some(&header)).unwrap();
        let doomed = file(&status, "doomed.rs");
        assert_eq!(
            (doomed.kind, doomed.staged, doomed.unstaged),
            (GitChangeKind::Deleted, true, false)
        );
    }

    #[test]
    fn a_renamed_file_is_staged_and_unstaged_whole() {
        let root = repo("rename");
        write(&root, "old.rs", &numbered(10));
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-q", "-m", "init"]);
        sh(&root, &["mv", "old.rs", "new.rs"]);
        let shown = diff(&root, "new.rs", Some("old.rs"), None).unwrap();
        let header = shown
            .staged_hunks
            .first()
            .map(|hunk| hunk.header.clone())
            .unwrap_or_else(|| "@@ -0,0 +1,10 @@".to_owned());
        assert!(matches!(
            unstage(&root, None, &["new.rs".to_owned()], Some(&header)),
            Err(GitError::Rejected(message)) if message.contains("renamed")
        ));
        let status = unstage(
            &root,
            None,
            &["new.rs".to_owned(), "old.rs".to_owned()],
            None,
        )
        .unwrap();
        assert!(status.files.iter().all(|file| !file.staged));
    }

    #[test]
    fn discard_restores_tracked_files_keeps_staged_changes_and_deletes_new_files() {
        let root = repo("discard");
        write(&root, "a.rs", "one\n");
        write(&root, "b.rs", "keep\n");
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-q", "-m", "init"]);
        write(&root, "a.rs", "staged\n");
        sh(&root, &["add", "a.rs"]);
        write(&root, "a.rs", "staged then edited\n");
        write(&root, "b.rs", "edited\n");
        write(&root, "new.rs", "new\n");
        write(&root, "target/build.o", "bin");

        let status = discard(
            &root,
            None,
            &["a.rs".to_owned(), "b.rs".to_owned(), "new.rs".to_owned()],
        )
        .unwrap();
        assert_eq!(
            read(&root, "a.rs").as_deref(),
            Some("staged\n"),
            "back to the staged content"
        );
        assert_eq!(read(&root, "b.rs").as_deref(), Some("keep\n"));
        assert_eq!(read(&root, "new.rs"), None);
        assert_eq!(
            read(&root, "target/build.o").as_deref(),
            Some("bin"),
            "ignored files stay"
        );
        let a = file(&status, "a.rs");
        assert!(a.staged && !a.unstaged);
        assert_eq!(status.files.len(), 1);
    }

    #[test]
    fn long_path_lists_run_in_batches() {
        let root = repo("batches");
        write(&root, "seed.rs", "x\n");
        committed(&root, "init");
        let paths: Vec<String> = (0..400)
            .map(|n| format!("a-rather-long-directory-name/file-number-{n:04}.txt"))
            .collect();
        for path in &paths {
            write(&root, path, "new\n");
        }
        let status = stage(&root, None, &paths, None).unwrap();
        assert_eq!(status.files.iter().filter(|file| file.staged).count(), 400);
        let status = unstage(&root, None, &paths, None).unwrap();
        assert!(status.files.iter().all(|file| !file.staged));
        let status = discard(&root, None, &paths).unwrap();
        assert!(status.files.is_empty());
    }

    #[test]
    fn changes_are_refused_when_the_folder_points_at_another_repository() {
        let root = repo("root-check");
        write(&root, "a.rs", "x\n");
        let shown = status(&root).unwrap().root;
        assert!(stage(&root, Some(&shown), &["a.rs".to_owned()], None).is_ok());
        assert_eq!(
            discard(&root, Some("/somewhere/else"), &["a.rs".to_owned()]),
            Err(GitError::FolderChanged)
        );
        assert_eq!(
            push(&root, Some("/somewhere/else")),
            Err(GitError::FolderChanged)
        );
        assert_eq!(read(&root, "a.rs").as_deref(), Some("x\n"));
    }

    #[test]
    fn commit_all_then_undo_keeps_the_changes_staged() {
        let root = repo("commit");
        assert_eq!(
            log(&root, None).unwrap(),
            (Vec::new(), false),
            "no commits yet"
        );
        write(&root, "a.rs", "one\n");
        let status = finished(&root, commit(&root, None, "  first\n\nbody  ", true));
        assert!(status.files.is_empty());
        let (commits, more) = log(&root, None).unwrap();
        assert!(!more);
        assert_eq!(commits[0].subject, "first");
        assert_eq!(commits[0].body, "body");
        assert_eq!((commits[0].files_changed, commits[0].insertions), (1, 1));
        assert!(!commits[0].pushed, "no remote: nothing is pushed");

        write(&root, "a.rs", "two\n");
        write(&root, "b.rs", "new\n");
        let head = committed(&root, "second").head.unwrap();
        assert_eq!(
            commit(&root, None, "   ", true),
            Err(GitError::Rejected(
                "write a commit message first".to_owned()
            ))
        );
        let files = show(&root, &head).unwrap();
        assert_eq!(files.len(), 2);
        let changed = diff(&root, "a.rs", None, Some(&head)).unwrap();
        assert_eq!(changed.hunks[0].lines, ["-one", "+two"]);

        let status = uncommit(&root, None, &head).unwrap();
        assert_ne!(status.head.as_deref(), Some(head.as_str()));
        assert!(status.files.iter().all(|file| file.staged));
        assert_eq!(status.files.len(), 2);
        assert!(
            matches!(uncommit(&root, None, &head), Err(GitError::Rejected(_))),
            "no longer HEAD"
        );

        // The very first commit can be undone too, leaving everything staged.
        let first = status.head.unwrap();
        let status = uncommit(&root, None, &first).unwrap();
        assert_eq!(status.head, None);
        assert_eq!(
            status.branch.as_deref(),
            Some("main"),
            "HEAD still names the branch"
        );
        assert!(status
            .files
            .iter()
            .all(|file| file.staged && file.kind == GitChangeKind::Added));
    }

    #[test]
    fn undoing_a_detached_first_commit_is_refused_and_leaves_the_repository_intact() {
        let root = repo("detached-root");
        write(&root, "a.rs", "one\n");
        let head = committed(&root, "first").head.unwrap();
        sh(&root, &["checkout", "-q", "--detach"]);
        assert!(matches!(
            uncommit(&root, None, &head),
            Err(GitError::Rejected(_))
        ));
        assert_eq!(status(&root).unwrap().head.as_deref(), Some(head.as_str()));
    }

    #[test]
    fn a_failing_operation_reports_its_error_and_frees_the_folder() {
        let root = repo("failing");
        write(&root, "a.rs", "x\n");
        committed(&root, "init");
        // Nothing staged and nothing changed: git itself refuses the commit.
        let (_, error) = outcome(&root, commit(&root, None, "empty", false).unwrap());
        assert!(error.is_some_and(|error| !error.is_empty()));
        let (_, error) = outcome(&root, push(&root, None).unwrap());
        assert!(error.is_some_and(|error| error.contains("no remote")));
        write(&root, "a.rs", "y\n");
        committed(&root, "works again");
    }

    #[cfg(unix)]
    #[test]
    fn a_chatty_hook_does_not_break_the_commit() {
        let root = repo("chatty");
        // 300 KB on stderr, far past what is kept of it.
        hook(&root, "pre-commit", "i=0; while [ $i -lt 6000 ]; do echo \"lint output line $i padded to be long enough\" >&2; i=$((i+1)); done; exit 0");
        write(&root, "a.rs", "x\n");
        let status = committed(&root, "through the hook");
        assert!(status.files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn local_changes_wait_for_a_running_operation_instead_of_racing_it() {
        let root = repo("busy");
        hook(&root, "pre-commit", "sleep 1");
        write(&root, "a.rs", "x\n");
        let started = commit(&root, None, "slow", true).unwrap();
        assert!(started
            .operation
            .as_ref()
            .is_some_and(|operation| operation.running));
        write(&root, "b.rs", "y\n");
        assert!(matches!(
            stage(&root, None, &["b.rs".to_owned()], None),
            Err(GitError::Rejected(message)) if message.contains("commit")
        ));
        assert!(matches!(
            push(&root, None),
            Err(GitError::Rejected(message)) if message.contains("commit")
        ));
        let (_, error) = outcome(&root, started);
        assert_eq!(error, None);
        assert!(stage(&root, None, &["b.rs".to_owned()], None).is_ok());
    }

    #[test]
    fn push_pull_and_pushed_flags_follow_the_remote() {
        let base = repo("remote-base");
        let remote = base.with_extension("git");
        let _ = std::fs::remove_dir_all(&remote);
        sh(
            &base,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote.to_str().unwrap(),
            ],
        );
        sh(
            &base,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        write(&base, "a.rs", "one\n");
        committed(&base, "first");

        // First push publishes the branch and sets its upstream.
        let status = finished(&base, push(&base, None));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (0, 0));
        write(&base, "a.rs", "two\n");
        let status = committed(&base, "second");
        assert_eq!(status.ahead, 1);
        let (commits, _) = log(&base, None).unwrap();
        assert_eq!(
            commits
                .iter()
                .map(|c| (c.subject.as_str(), c.pushed))
                .collect::<Vec<_>>(),
            [("second", false), ("first", true)]
        );
        assert!(
            matches!(
                uncommit(&base, None, &commits[1].commit),
                Err(GitError::Rejected(_))
            ),
            "an older commit is refused"
        );
        assert_eq!(finished(&base, push(&base, None)).ahead, 0);
        assert!(
            matches!(
                uncommit(&base, None, &commits[0].commit),
                Err(GitError::Rejected(_))
            ),
            "pushed"
        );

        // A second clone pulls the new commit.
        let clone = base.with_extension("clone");
        let _ = std::fs::remove_dir_all(&clone);
        sh(
            &base,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        configure(&clone);
        write(&base, "b.rs", "b\n");
        committed(&base, "third");
        finished(&base, push(&base, None));
        sh(&clone, &["fetch", "-q"]);
        assert_eq!(status_at(&toplevel(&clone).unwrap()).unwrap().behind, 1);
        let status = finished(&clone, pull(&clone, None));
        assert_eq!(status.behind, 0);
        assert_eq!(read(&clone, "b.rs").as_deref(), Some("b\n"));
    }

    #[test]
    fn a_folder_outside_git_is_reported_as_such() {
        let dir =
            std::env::temp_dir().join(format!("herdr-source-control-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The temp dir may itself sit inside a repository on some machines.
        if toplevel(&dir).is_err() {
            assert_eq!(status(&dir), Err(GitError::NotARepository));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_command_and_what_it_started_are_stopped_at_the_timeout() {
        let started = Instant::now();
        let result = run(
            Command::new("sh").args(["-c", "sleep 30 & sleep 30"]),
            None,
            Duration::from_millis(200),
            usize::MAX,
        );
        assert!(matches!(result, Err(GitError::Failed(message)) if message.contains("stopped")));
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "the background sleep holding the pipes was stopped too"
        );
    }

    #[test]
    fn the_sidebar_count_is_fresh_right_after_a_change_made_here() {
        let root = repo("count");
        write(&root, "a.rs", "one\n");
        committed(&root, "init");
        let folder = toplevel(&root).unwrap();
        assert_eq!(recent_changed_file_count(&folder), Some(0));
        write(&root, "b.rs", "new\n");
        write(&root, "c.rs", "new\n");
        // Staging through this module bumps the epoch, so the cache is skipped.
        stage(&root, None, &["b.rs".to_owned()], None).unwrap();
        assert_eq!(recent_changed_file_count(&folder), Some(2));
    }

    #[test]
    fn output_past_the_cap_is_cut_or_drained_and_flagged() {
        let (bytes, cut) = read_capped(&b"0123456789"[..], 4, false);
        assert_eq!((bytes.as_slice(), cut), (&b"0123"[..], true));
        let (bytes, cut) = read_capped(&b"0123"[..], 4, false);
        assert_eq!((bytes.as_slice(), cut), (&b"0123"[..], false));
        let (bytes, cut) = read_capped(&b"0123456789"[..], 4, true);
        assert_eq!((bytes.as_slice(), cut), (&b"0123"[..], true));
    }
}
