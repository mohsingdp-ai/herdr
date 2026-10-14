//! Code checkpoints: hidden git commits of a folder's files, taken before an
//! agent works on a prompt so the folder can be rewound later. Snapshots live
//! in the repository's own object store under `refs/herdr/checkpoints/`, so
//! branches, history and the staging area are never touched, and a normal
//! `git push` never sends them.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const REF_ROOT: &str = "refs/herdr/checkpoints";
/// New files bigger than this stay out of snapshots, and so out of rewinds.
const MAX_NEW_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Snapshots kept per folder; older ones are deleted.
const KEEP_PER_FOLDER: usize = 50;
const FOLDER_TRAILER: &str = "Herdr-Folder";

// ponytail: one lock for all checkpoint git work; per-folder locks if snapshots ever queue up.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static NEXT_TEMP_INDEX: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    pub(crate) id: String,
    /// Unix seconds.
    pub(crate) created_at: u64,
    pub(crate) label: String,
}

#[derive(Debug)]
pub(crate) enum RestoreError {
    NotFound,
    /// Agents still working in the folder; rewinding would pull files out from under them.
    Busy,
    Git(io::Error),
}

impl From<io::Error> for RestoreError {
    fn from(err: io::Error) -> Self {
        Self::Git(err)
    }
}

/// Snapshots `cwd`'s folder unless nothing changed since its newest snapshot.
pub(crate) fn snapshot(cwd: &Path, label: &str) -> io::Result<Option<Checkpoint>> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = toplevel(cwd)?;
    let checkpoint = snapshot_root(&root, label)?;
    prune_missing_folders(&root)?;
    Ok(checkpoint)
}

/// The folder's root and its snapshots, newest first.
pub(crate) fn list(cwd: &Path) -> io::Result<(PathBuf, Vec<Checkpoint>)> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = toplevel(cwd)?;
    let checkpoints = folder_checkpoints(&root)?;
    Ok((root, checkpoints))
}

/// Puts the folder back to checkpoint `id`: files are restored, files created
/// since are deleted, ignored and oversized files are left alone, and the
/// staging area is untouched. Saves an undo snapshot first and returns its id.
/// Refused while any of `busy_cwds` (folders of working agents) is inside the
/// folder, unless `force`.
pub(crate) fn restore(
    cwd: &Path,
    id: &str,
    busy_cwds: &[PathBuf],
    force: bool,
) -> Result<String, RestoreError> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = toplevel(cwd)?;
    let checkpoints = folder_checkpoints(&root)?;
    let Some(target) = checkpoints.iter().find(|checkpoint| checkpoint.id == id) else {
        return Err(RestoreError::NotFound);
    };
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
    if !force
        && busy_cwds.iter().any(|busy| {
            busy.canonicalize()
                .unwrap_or_else(|_| busy.clone())
                .starts_with(&canonical_root)
        })
    {
        return Err(RestoreError::Busy);
    }
    let target_ref = format!("{}{}", folder_prefix(&root), target.id);
    let undo_label = "before rewind";
    // Unchanged since the newest snapshot: that one already is the undo point.
    let undo_id = match snapshot_root(&root, undo_label)? {
        Some(undo) => undo.id,
        None => checkpoints
            .first()
            .map(|newest| newest.id.clone())
            .unwrap_or_default(),
    };
    let now_tree = write_tree(&root)?;
    let added = run_bytes(git(&root).args([
        "diff-tree",
        "-r",
        "-z",
        "--name-only",
        "--diff-filter=A",
        &format!("{target_ref}^{{tree}}"),
        &now_tree,
    ]))?;
    run(git(&root).args([
        "restore",
        &format!("--source={target_ref}"),
        "--worktree",
        "--",
        ".",
    ]))?;
    for path in nul_separated(&added) {
        let _ = std::fs::remove_file(root.join(path));
    }
    Ok(undo_id)
}

fn snapshot_root(root: &Path, label: &str) -> io::Result<Option<Checkpoint>> {
    let tree = write_tree(root)?;
    let prefix = folder_prefix(root);
    let newest_tree = run(git(root).args([
        "for-each-ref",
        "--count=1",
        "--sort=-refname",
        "--format=%(tree)",
        &prefix,
    ]))?;
    if newest_tree == tree {
        return Ok(None);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut commit = git(root);
    commit
        .args(["commit-tree", &tree, "-m", label, "-m"])
        .arg(format!("{FOLDER_TRAILER}: {}", root.display()))
        .envs([
            ("GIT_AUTHOR_NAME", "herdr"),
            ("GIT_AUTHOR_EMAIL", "herdr@localhost"),
            ("GIT_COMMITTER_NAME", "herdr"),
            ("GIT_COMMITTER_EMAIL", "herdr@localhost"),
        ]);
    if run(git(root).args(["rev-parse", "--verify", "-q", "HEAD"])).is_ok() {
        commit.args(["-p", "HEAD"]);
    }
    let oid = run(&mut commit)?;
    // Millisecond ids sort by name and stay unique per folder.
    let id = format!("{:013}", now.as_millis());
    run(git(root).args(["update-ref", &format!("{prefix}{id}"), &oid]))?;
    prune_old(root, &prefix)?;
    Ok(Some(Checkpoint {
        id,
        created_at: now.as_secs(),
        label: label.to_owned(),
    }))
}

/// Writes the folder's files to a tree: tracked files plus new files, never
/// ignored ones, through a throwaway index so the real staging area is untouched.
fn write_tree(root: &Path) -> io::Result<String> {
    let index =
        run(git(root).args(["rev-parse", "--path-format=absolute", "--git-path", "index"]))?;
    let temp = std::env::temp_dir().join(format!(
        "herdr-checkpoint-{}-{}.index",
        std::process::id(),
        NEXT_TEMP_INDEX.fetch_add(1, Ordering::Relaxed)
    ));
    // Starting from the real index skips re-hashing unchanged files.
    if std::fs::copy(&index, &temp).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    let result = (|| {
        let with_index = |args: &[&str]| {
            let mut command = git(root);
            command.env("GIT_INDEX_FILE", &temp).args(args);
            command
        };
        run(&mut with_index(&["add", "-u"]))?;
        let untracked = run_bytes(&mut with_index(&[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
        ]))?;
        let small: Vec<&str> = nul_separated(&untracked)
            .filter(|path| {
                std::fs::metadata(root.join(path))
                    .is_ok_and(|meta| meta.is_file() && meta.len() <= MAX_NEW_FILE_BYTES)
            })
            .collect();
        if !small.is_empty() {
            let mut add = with_index(&["add", "--pathspec-from-file=-", "--pathspec-file-nul"]);
            run_with_stdin(&mut add, small.join("\0").as_bytes())?;
        }
        run(&mut with_index(&["write-tree"]))
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

fn folder_checkpoints(root: &Path) -> io::Result<Vec<Checkpoint>> {
    let listed = run(git(root).args([
        "for-each-ref",
        "--sort=-refname",
        "--format=%(refname)%00%(committerdate:unix)%00%(subject)",
        &folder_prefix(root),
    ]))?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let id = fields.next()?.rsplit('/').next()?.to_owned();
            let created_at = fields.next()?.parse().ok()?;
            let label = fields.next().unwrap_or_default().to_owned();
            Some(Checkpoint {
                id,
                created_at,
                label,
            })
        })
        .collect())
}

fn prune_old(root: &Path, prefix: &str) -> io::Result<()> {
    let refs = run(git(root).args([
        "for-each-ref",
        "--sort=-refname",
        "--format=%(refname)",
        prefix,
    ]))?;
    delete_refs(root, refs.lines().skip(KEEP_PER_FOLDER))
}

/// Drops snapshots of folders that no longer exist, such as deleted
/// worktrees. Worktrees share one ref store, so this sees all of them.
fn prune_missing_folders(root: &Path) -> io::Result<()> {
    let refs = run(git(root).args([
        "for-each-ref",
        &format!("--format=%(refname)%00%(trailers:key={FOLDER_TRAILER},valueonly)"),
        REF_ROOT,
    ]))?;
    let missing = refs.lines().filter_map(|line| {
        let (name, folder) = line.split_once('\0')?;
        let folder = folder.trim();
        (!folder.is_empty() && !Path::new(folder).exists()).then_some(name)
    });
    delete_refs(root, missing)
}

fn delete_refs<'a>(root: &Path, names: impl Iterator<Item = &'a str>) -> io::Result<()> {
    let commands: String = names.map(|name| format!("delete {name}\n")).collect();
    if commands.is_empty() {
        return Ok(());
    }
    run_with_stdin(
        git(root).args(["update-ref", "--stdin"]),
        commands.as_bytes(),
    )
    .map(|_| ())
}

fn toplevel(cwd: &Path) -> io::Result<PathBuf> {
    run(git(cwd).args(["rev-parse", "--show-toplevel"])).map(PathBuf::from)
}

/// One ref namespace per folder, so each worktree has its own timeline.
fn folder_prefix(root: &Path) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{REF_ROOT}/{hash:016x}/")
}

fn nul_separated(bytes: &[u8]) -> impl Iterator<Item = &str> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .filter_map(|part| std::str::from_utf8(part).ok())
}

fn git(dir: &Path) -> Command {
    let mut command = crate::noninteractive_process::command("git");
    command.arg("-C").arg(dir);
    command
}

fn run(command: &mut Command) -> io::Result<String> {
    run_bytes(command).map(|out| String::from_utf8_lossy(&out).trim().to_owned())
}

fn run_bytes(command: &mut Command) -> io::Result<Vec<u8>> {
    let output = command.stdin(Stdio::null()).output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output.stdout)
}

fn run_with_stdin(command: &mut Command, input: &[u8]) -> io::Result<String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-checkpoint-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("a.rs"), "v1").unwrap();
        std::fs::write(root.join("b.rs"), "keep").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(root.join("notes.md"), "untracked").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/app"), "bin").unwrap();
        root
    }

    fn read(root: &Path, path: &str) -> Option<String> {
        std::fs::read_to_string(root.join(path)).ok()
    }

    #[test]
    fn rewind_restores_files_deletes_new_ones_and_keeps_ignored_and_staged() {
        let root = repo("rewind");
        let first = snapshot(&root, "before claude")
            .unwrap()
            .expect("first snapshot");
        assert_eq!(
            snapshot(&root, "again").unwrap(),
            None,
            "unchanged folder is not snapshotted twice"
        );

        // The agent's turn: edit, delete tracked and untracked files, create one, rebuild.
        std::fs::write(root.join("a.rs"), "v2").unwrap();
        std::fs::remove_file(root.join("b.rs")).unwrap();
        std::fs::remove_file(root.join("notes.md")).unwrap();
        std::fs::write(root.join("new.rs"), "created").unwrap();
        std::fs::write(root.join("target/app"), "rebuilt").unwrap();
        let staged = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "a.rs"])
            .status();
        assert!(staged.unwrap().success());

        assert!(matches!(
            restore(&root, &first.id, std::slice::from_ref(&root), false),
            Err(RestoreError::Busy)
        ));
        let undo = restore(&root, &first.id, &[], false).expect("rewind");
        assert_eq!(read(&root, "a.rs").as_deref(), Some("v1"));
        assert_eq!(read(&root, "b.rs").as_deref(), Some("keep"));
        assert_eq!(read(&root, "notes.md").as_deref(), Some("untracked"));
        assert_eq!(
            read(&root, "new.rs"),
            None,
            "files created since are deleted"
        );
        assert_eq!(
            read(&root, "target/app").as_deref(),
            Some("rebuilt"),
            "ignored files are left alone"
        );
        let index = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["show", ":a.rs"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&index.stdout),
            "v2",
            "the staging area is untouched"
        );

        // The undo point brings the agent's work back.
        restore(&root, &undo, &[], false).expect("undo rewind");
        assert_eq!(read(&root, "a.rs").as_deref(), Some("v2"));
        assert_eq!(read(&root, "new.rs").as_deref(), Some("created"));
        assert_eq!(read(&root, "b.rs"), None);

        let (_, listed) = list(&root).unwrap();
        assert_eq!(
            listed.last().map(|c| c.label.as_str()),
            Some("before claude")
        );
        let branches = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["log", "--oneline", "main"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&branches.stdout).lines().count(),
            1,
            "history is untouched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshots_of_deleted_folders_are_pruned() {
        let root = repo("prune");
        snapshot(&root, "main").unwrap();
        let gone =
            std::env::temp_dir().join(format!("herdr-checkpoint-gone-{}", std::process::id()));
        // A snapshot of a folder that has since been deleted, like a removed worktree.
        let tagged = run(git(&root)
            .args(["commit-tree", "HEAD^{tree}", "-m", "x", "-m"])
            .arg(format!("{FOLDER_TRAILER}: {}", gone.display()))
            .envs([("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t")])
            .envs([("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")]))
        .unwrap();
        run(git(&root).args(["update-ref", &format!("{}1", folder_prefix(&gone)), &tagged]))
            .unwrap();
        std::fs::write(root.join("a.rs"), "changed").unwrap();
        snapshot(&root, "main again").unwrap();
        let left = run(git(&root).args(["for-each-ref", "--format=%(refname)", REF_ROOT])).unwrap();
        assert!(!left.contains(&folder_prefix(&gone)), "{left}");
        assert_eq!(left.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }
}
