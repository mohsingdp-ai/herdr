//! A project's Markdown notes, behind the Notes tab and the `note.*` API.
//! Notes live outside the repository, in herdr's data folder, one vault per
//! project, so they never show up in git. Every function touches the disk,
//! so callers run them off the main loop.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::api::schema::{NoteFile, NoteVault};

/// More notes than this and the rest are left out of a list.
const MAX_NOTES: usize = 2000;
/// Larger notes are left out of a list and refused by a write.
const MAX_NOTE_BYTES: u64 = 1 << 20;
const MAX_COMPONENT_BYTES: usize = 255;
/// Obsidian's name for a vault's deleted notes; skipped like any dot folder.
const TRASH: &str = ".trash";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoteError {
    BadPath(String),
    Exists(String),
    Missing(String),
    /// The note changed since the caller read it; nothing was written.
    Conflict(String),
    TooLarge,
    Failed(String),
}

impl NoteError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::BadPath(_) => "note_bad_path",
            Self::Exists(_) => "note_exists",
            Self::Missing(_) => "note_missing",
            Self::Conflict(_) => "note_conflict",
            Self::TooLarge => "note_too_large",
            Self::Failed(_) => "note_failed",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::BadPath(path) => format!(
                "{path:?} is not a note path: use a relative path with `/` separators, \
                 no dot names, ending in .md"
            ),
            Self::Exists(path) => format!("note {path} already exists"),
            Self::Missing(path) => format!("note {path} not found"),
            Self::Conflict(path) => {
                format!("note {path} changed since it was read; reload and try again")
            }
            Self::TooLarge => format!("a note can hold at most {MAX_NOTE_BYTES} bytes"),
            Self::Failed(message) => message.clone(),
        }
    }
}

type Result<T> = std::result::Result<T, NoteError>;

fn failed(err: impl std::fmt::Display) -> NoteError {
    NoteError::Failed(err.to_string())
}

/// One project's notes folder. It may not exist yet: reads see an empty
/// vault and the first write creates it.
pub(crate) struct Vault {
    pub(crate) root: PathBuf,
    pub(crate) project: String,
}

impl Vault {
    pub(crate) fn for_folder(folder: &Path) -> Self {
        let project = project_name(folder);
        Self {
            root: crate::config::data_dir().join("notes").join(&project),
            project,
        }
    }

    pub(crate) fn list(&self) -> NoteVault {
        let (entries, mut truncated) = self.walk();
        let stamp = stamp_of(&entries, truncated);
        let mut notes = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.len > MAX_NOTE_BYTES {
                truncated = true;
                continue;
            }
            // Gone since the walk: the next stamp differs, so it is read again.
            let Ok(bytes) = std::fs::read(&entry.abs) else {
                continue;
            };
            notes.push(NoteFile {
                path: entry.path,
                fingerprint: fingerprint(&bytes),
                content: String::from_utf8_lossy(&bytes).into_owned(),
                modified_ms: millis(entry.modified),
            });
        }
        NoteVault {
            root: self.root.display().to_string(),
            project: self.project.clone(),
            stamp,
            notes,
            truncated,
        }
    }

    /// A cheap check for changes: only names, sizes and times, no content,
    /// so the Notes tab can ask every few seconds.
    pub(crate) fn stamp(&self) -> String {
        let (entries, truncated) = self.walk();
        stamp_of(&entries, truncated)
    }

    /// Creates the note when `base` is `None`, otherwise replaces it only
    /// when its fingerprint still is `base`.
    pub(crate) fn write(&self, path: &str, content: &str, base: Option<&str>) -> Result<NoteFile> {
        let file = self.checked(path)?;
        if content.len() as u64 > MAX_NOTE_BYTES {
            return Err(NoteError::TooLarge);
        }
        crate::tasks::locked(&self.root, || {
            match (std::fs::read(&file), base) {
                (Ok(_), None) => return Err(NoteError::Exists(path.to_owned())),
                (Ok(current), Some(base)) if fingerprint(&current) != base => {
                    return Err(NoteError::Conflict(path.to_owned()))
                }
                (Err(err), Some(_)) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Err(NoteError::Missing(path.to_owned()))
                }
                (Err(err), _) if err.kind() != std::io::ErrorKind::NotFound => {
                    return Err(failed(err))
                }
                _ => {}
            }
            crate::detect::manifest_update::atomic_write(&file, content.as_bytes())
                .map_err(NoteError::Failed)?;
            let modified = std::fs::metadata(&file)
                .and_then(|metadata| metadata.modified())
                .ok();
            Ok(NoteFile {
                path: path.to_owned(),
                content: content.to_owned(),
                fingerprint: fingerprint(content.as_bytes()),
                modified_ms: millis(modified),
            })
        })
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> Result<NoteVault> {
        let source = self.checked(from)?;
        let target = self.checked(to)?;
        crate::tasks::locked(&self.root, || {
            if !is_file(&source) {
                return Err(NoteError::Missing(from.to_owned()));
            }
            if std::fs::symlink_metadata(&target).is_ok() {
                return Err(NoteError::Exists(to.to_owned()));
            }
            move_file(&source, &target)?;
            self.prune(&source);
            self.relink(from, to)
        })?;
        Ok(self.list())
    }

    /// Points every `[[link]]` to the note that moved from `from` at `to`,
    /// as Obsidian does on a rename. Runs under the vault's write lock.
    fn relink(&self, from: &str, to: &str) -> Result<()> {
        let (entries, _) = self.walk();
        for entry in entries
            .into_iter()
            .filter(|entry| entry.len <= MAX_NOTE_BYTES)
        {
            let Ok(text) = std::fs::read_to_string(&entry.abs) else {
                continue;
            };
            if let Some(text) = relink_text(&text, from, to) {
                crate::detect::manifest_update::atomic_write(&entry.abs, text.as_bytes())
                    .map_err(NoteError::Failed)?;
            }
        }
        Ok(())
    }

    /// Moves the note into `.trash`, keeping its folders, as Obsidian does.
    pub(crate) fn delete(&self, path: &str) -> Result<NoteVault> {
        let source = self.checked(path)?;
        crate::tasks::locked(&self.root, || {
            if !is_file(&source) {
                return Err(NoteError::Missing(path.to_owned()));
            }
            let mut target = self.root.join(TRASH).join(path);
            let stem = path.trim_end_matches(".md");
            let mut copy = 2;
            while std::fs::symlink_metadata(&target).is_ok() {
                target = self.root.join(TRASH).join(format!("{stem} ({copy}).md"));
                copy += 1;
            }
            move_file(&source, &target)?;
            self.prune(&source);
            Ok(())
        })?;
        Ok(self.list())
    }

    /// The file a client's note path names. Every path from a client passes
    /// here: relative, `/`-separated, no dot names, `.md`, inside the vault,
    /// and through no symlink.
    fn checked(&self, path: &str) -> Result<PathBuf> {
        let bad = || NoteError::BadPath(path.to_owned());
        if !path.ends_with(".md") || path.contains(['\\', '\0']) {
            return Err(bad());
        }
        let mut file = self.root.clone();
        for part in path.split('/') {
            let mut components = Path::new(part).components();
            let normal = matches!(components.next(), Some(Component::Normal(_)))
                && components.next().is_none();
            if !normal || part.starts_with('.') || part.len() > MAX_COMPONENT_BYTES {
                return Err(bad());
            }
            file.push(part);
            if std::fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_symlink()) {
                return Err(bad());
            }
        }
        Ok(file)
    }

    /// Removes the now-empty folders above a moved note, never the vault.
    fn prune(&self, moved: &Path) {
        for dir in moved.ancestors().skip(1) {
            if dir == self.root || !dir.starts_with(&self.root) || std::fs::remove_dir(dir).is_err()
            {
                break;
            }
        }
    }

    /// Every `.md` file under the vault, sorted by path and capped, skipping
    /// dot names (`.trash`, `.obsidian`) and symlinks. The flag says some
    /// were left out.
    fn walk(&self) -> (Vec<Entry>, bool) {
        let mut entries = Vec::new();
        let mut dirs = vec![(self.root.clone(), String::new())];
        while let Some((dir, prefix)) = dirs.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.flatten() {
                // A name the API cannot carry back is skipped, not mangled.
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                // The entry's own type: symlinks are neither files nor folders here.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                let path = format!("{prefix}{name}");
                if kind.is_dir() {
                    dirs.push((entry.path(), format!("{path}/")));
                } else if kind.is_file() && name.ends_with(".md") {
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    entries.push(Entry {
                        path,
                        abs: entry.path(),
                        len: metadata.len(),
                        modified: metadata.modified().ok(),
                    });
                }
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let truncated = entries.len() > MAX_NOTES;
        entries.truncate(MAX_NOTES);
        (entries, truncated)
    }
}

struct Entry {
    /// Relative to the vault, `/`-separated.
    path: String,
    abs: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

fn stamp_of(entries: &[Entry], truncated: bool) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for entry in entries {
        (&entry.path, entry.len, entry.modified).hash(&mut hasher);
    }
    truncated.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// `text` with its links to the note at `from` pointing at `to`, or `None`
/// when it has none. A link written as a path gets the new path, a link
/// written as a name gets the new name; headings and aliases stay. Fenced
/// code is left alone.
// ponytail: a bare name also matches when another note shares the old name;
// resolve each link like the Notes tab does if that ever matters.
fn relink_text(text: &str, from: &str, to: &str) -> Option<String> {
    let stem = |path: &str| path.strip_suffix(".md").unwrap_or(path).to_owned();
    let name = |path: &str| stem(path.rsplit('/').next().unwrap_or(path));
    let (old_path, old_name) = (stem(from).to_lowercase(), name(from).to_lowercase());
    let (new_path, new_name) = (stem(to), name(to));
    let mut changed = false;
    let mut in_fence = false;
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        let mut rest = line;
        while let Some(open) = rest.find("[[") {
            let after = &rest[open + 2..];
            let Some(close) = after.find("]]") else {
                break;
            };
            let inner = &after[..close];
            let target_len = inner.find(['#', '|']).unwrap_or(inner.len());
            let target = inner[..target_len].trim();
            let key = target.strip_suffix(".md").unwrap_or(target).to_lowercase();
            let replacement = if target.contains('/') && key == old_path {
                Some(&new_path)
            } else if !target.contains('/') && key == old_name {
                Some(&new_name)
            } else {
                None
            };
            out.push_str(&rest[..open + 2]);
            match replacement.filter(|new| new.as_str() != target) {
                Some(new) => {
                    out.push_str(new);
                    out.push_str(&inner[target_len..]);
                    changed = true;
                }
                None => out.push_str(inner),
            }
            out.push_str("]]");
            rest = &after[close + 2..];
        }
        out.push_str(rest);
    }
    changed.then_some(out)
}

fn fingerprint(bytes: &[u8]) -> String {
    crate::checksum::to_lower_hex(&Sha256::digest(bytes))
}

fn millis(time: Option<SystemTime>) -> u64 {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as u64)
}

fn is_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn move_file(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    std::fs::rename(source, target).map_err(failed)
}

/// The vault's name for the project holding `folder`: the folder name of its
/// repository's main checkout, so linked worktrees share one vault, or of
/// `folder` itself outside git.
// ponytail: keyed by folder name, two repos with the same folder name share
// notes; key by a hash of the main checkout's path if that bites.
fn project_name(folder: &Path) -> String {
    let main = crate::tasks::in_main_checkout(folder);
    let checkout = main
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(&main);
    let name: String = checkout
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim();
    if name.is_empty() || name.starts_with('.') {
        "default".to_owned()
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-notes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn vault(name: &str) -> Vault {
        Vault {
            root: temp(name).join("vault"),
            project: "project".to_owned(),
        }
    }

    fn paths(vault: &Vault) -> Vec<String> {
        vault
            .list()
            .notes
            .into_iter()
            .map(|note| note.path)
            .collect()
    }

    #[test]
    fn a_missing_vault_lists_empty_and_is_not_created() {
        let vault = vault("missing");
        let listed = vault.list();
        assert!(listed.notes.is_empty());
        assert!(!listed.truncated);
        assert!(!vault.root.exists());
    }

    #[test]
    fn list_skips_dot_names_and_other_files_and_sorts() {
        let vault = vault("list");
        for path in [
            "b.md",
            "a/z.md",
            "a/c.md",
            ".trash/old.md",
            ".obsidian/x.md",
        ] {
            let file = vault.root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "text").unwrap();
        }
        std::fs::write(vault.root.join("notes.txt"), "text").unwrap();
        std::fs::write(vault.root.join(".hidden.md"), "text").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(vault.root.join("b.md"), vault.root.join("link.md")).unwrap();
        assert_eq!(paths(&vault), ["a/c.md", "a/z.md", "b.md"]);
        let _ = std::fs::remove_dir_all(vault.root.parent().unwrap());
    }

    #[test]
    fn oversize_notes_are_left_out_and_flagged() {
        let vault = vault("oversize");
        std::fs::create_dir_all(&vault.root).unwrap();
        std::fs::write(
            vault.root.join("big.md"),
            vec![b'x'; MAX_NOTE_BYTES as usize + 1],
        )
        .unwrap();
        std::fs::write(vault.root.join("small.md"), "x").unwrap();
        let listed = vault.list();
        assert!(listed.truncated);
        assert_eq!(listed.notes.len(), 1);
        let _ = std::fs::remove_dir_all(vault.root.parent().unwrap());
    }

    #[test]
    fn writes_create_once_and_then_need_the_current_fingerprint() {
        let vault = vault("write");
        let created = vault.write("daily/today.md", "one", None).unwrap();
        assert_eq!(created.fingerprint, fingerprint(b"one"));
        assert_eq!(
            vault
                .write("daily/today.md", "two", None)
                .unwrap_err()
                .code(),
            "note_exists"
        );
        let before = vault.stamp();
        let stale = fingerprint(b"something else");
        assert_eq!(
            vault
                .write("daily/today.md", "two", Some(&stale))
                .unwrap_err()
                .code(),
            "note_conflict"
        );
        assert_eq!(
            std::fs::read_to_string(vault.root.join("daily/today.md")).unwrap(),
            "one"
        );
        let saved = vault
            .write("daily/today.md", "two!", Some(&created.fingerprint))
            .unwrap();
        assert_eq!(saved.fingerprint, fingerprint(b"two!"));
        assert_eq!(vault.list().notes[0].content, "two!");
        assert_ne!(vault.stamp(), before);
        assert_eq!(
            vault
                .write("gone.md", "x", Some(&saved.fingerprint))
                .unwrap_err()
                .code(),
            "note_missing"
        );
        let _ = std::fs::remove_dir_all(vault.root.parent().unwrap());
    }

    #[test]
    fn links_follow_a_renamed_note() {
        let text = "see [[File tree]], [[file tree#Plan|the plan]] and [[ideas/File tree]]\n\
                    ```\n[[File tree]]\n```\n[[Other]] [[File tree x]]";
        assert_eq!(
            relink_text(text, "ideas/File tree.md", "Explorer.md").as_deref(),
            Some(
                "see [[Explorer]], [[Explorer#Plan|the plan]] and [[Explorer]]\n\
                 ```\n[[File tree]]\n```\n[[Other]] [[File tree x]]"
            )
        );
        // A move between folders keeps name links as they are.
        assert_eq!(
            relink_text("[[File tree]]", "File tree.md", "a/File tree.md"),
            None
        );
        assert_eq!(relink_text("no links", "a.md", "b.md"), None);

        let vault = vault("relink");
        vault.write("File tree.md", "# File tree", None).unwrap();
        vault
            .write("daily/today.md", "worked on [[File tree]]", None)
            .unwrap();
        vault.rename("File tree.md", "Explorer.md").unwrap();
        assert_eq!(
            std::fs::read_to_string(vault.root.join("daily/today.md")).unwrap(),
            "worked on [[Explorer]]"
        );
        let _ = std::fs::remove_dir_all(vault.root.parent().unwrap());
    }

    #[test]
    fn rename_moves_the_note_and_removes_emptied_folders() {
        let vault = vault("rename");
        vault.write("a/b/note.md", "x", None).unwrap();
        vault.write("other.md", "y", None).unwrap();
        assert_eq!(
            vault.rename("a/b/note.md", "other.md").unwrap_err().code(),
            "note_exists"
        );
        assert_eq!(
            vault.rename("nope.md", "new.md").unwrap_err().code(),
            "note_missing"
        );
        let listed = vault.rename("a/b/note.md", "c/note.md").unwrap();
        let listed: Vec<_> = listed.notes.into_iter().map(|note| note.path).collect();
        assert_eq!(listed, ["c/note.md", "other.md"]);
        assert!(!vault.root.join("a").exists());
        assert!(vault.root.exists());
        let _ = std::fs::remove_dir_all(vault.root.parent().unwrap());
    }

    #[test]
    fn delete_moves_into_trash_with_a_suffix_on_collision() {
        let vault = vault("delete");
        vault.write("a/note.md", "first", None).unwrap();
        vault.delete("a/note.md").unwrap();
        vault.write("a/note.md", "second", None).unwrap();
        let listed = vault.delete("a/note.md").unwrap();
        assert!(listed.notes.is_empty());
        let trash = vault.root.join(TRASH);
        assert_eq!(
            std::fs::read_to_string(trash.join("a/note.md")).unwrap(),
            "first"
        );
        assert_eq!(
            std::fs::read_to_string(trash.join("a/note (2).md")).unwrap(),
            "second"
        );
        assert!(!vault.root.join("a").exists());
        assert_eq!(
            vault.delete("a/note.md").unwrap_err().code(),
            "note_missing"
        );
        let _ = std::fs::remove_dir_all(vault.root.parent().unwrap());
    }

    #[test]
    fn paths_outside_the_vault_or_not_notes_are_refused() {
        let vault = vault("paths");
        for path in [
            "../x.md",
            "/abs.md",
            "a/.hidden.md",
            ".md",
            "x.txt",
            "a\\b.md",
            "a//b.md",
            "./a.md",
            "a/../b.md",
            "nul\0.md",
            "",
        ] {
            assert_eq!(
                vault.write(path, "x", None).unwrap_err().code(),
                "note_bad_path",
                "{path:?}"
            );
        }
        let long = format!("{}.md", "x".repeat(MAX_COMPONENT_BYTES));
        assert_eq!(
            vault.write(&long, "x", None).unwrap_err().code(),
            "note_bad_path"
        );
        assert_eq!(
            vault.rename("ok.md", "../out.md").unwrap_err().code(),
            "note_bad_path"
        );
        assert_eq!(
            vault.delete("/etc/x.md").unwrap_err().code(),
            "note_bad_path"
        );
        let _ = std::fs::remove_dir_all(vault.root.parent().unwrap());
    }

    #[test]
    fn linked_worktrees_share_the_main_checkout_project() {
        let base = temp("project");
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
        std::fs::create_dir_all(main.join("src")).unwrap();
        assert_eq!(project_name(&main), "main");
        assert_eq!(project_name(&main.join("src")), "main");
        assert_eq!(project_name(&worktree), "main");
        assert_eq!(project_name(&worktree.join("src")), "main");
        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // Outside git the folder itself names the project. (The temp dir
        // itself must not be inside a checkout for this to hold.)
        if !plain.ancestors().any(|dir| dir.join(".git").exists()) {
            assert_eq!(project_name(&plain), "plain");
        }
        assert_eq!(project_name(Path::new("/")), "default");
        let _ = std::fs::remove_dir_all(&base);
    }
}
