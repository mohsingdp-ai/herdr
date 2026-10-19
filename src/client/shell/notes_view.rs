//! Notes: the focused space's markdown notes, Obsidian style, shown in place
//! of its panes like a tab. Notes are plain `.md` files the server keeps per
//! project; every read and write goes through its `note.*` methods. This
//! module keeps what is on screen and asks for what the screen needs next.
//! Nothing here runs while no Notes tab is open.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{KeyEventKind, KeyModifiers};

use super::note_editor::{EditorOutcome, NoteEditor};
use super::notes_markdown::{self, DocRow, DocTarget};
use super::*;
use crate::api::schema::{Method, NoteFile, NoteVault, ResponseResult};

/// While Notes is on screen, it asks this often whether the files changed.
const STAMP_INTERVAL: Duration = Duration::from_secs(2);
/// Typing pauses this long before the note is saved.
const AUTOSAVE_DELAY: Duration = Duration::from_millis(800);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// What Notes can show for its space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NotesLoad {
    Loading,
    Ready,
    /// The server has no `note.*` methods.
    Unsupported,
    Failed(String),
}

#[derive(Debug)]
pub(super) enum NotesRequest {
    List,
    /// Whether the files changed since the list on screen.
    Stamp,
    /// `content` on its way to `path`. `revision` is the editor's when the
    /// editor sent it; `open` shows the note once it exists.
    Write {
        path: String,
        content: String,
        revision: Option<u64>,
        open: bool,
    },
    Rename {
        from: String,
        to: String,
    },
    Delete {
        path: String,
    },
    /// A pasted image on its way to the vault, for the note at `note`.
    Attach {
        note: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SwitcherPurpose {
    Find,
    New,
    Rename,
}

/// The quick switcher: find a note, name a new one, or rename the open one.
#[derive(Debug)]
pub(super) struct NotesSwitcher {
    pub(super) purpose: SwitcherPurpose,
    pub(super) query: TextEditor,
    pub(super) selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SwitcherChoice {
    Open(String),
    Create(String),
    Rename(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ListRow {
    Folder {
        path: String,
        name: String,
        depth: usize,
        open: bool,
    },
    Note {
        path: String,
        title: String,
        depth: usize,
    },
}

/// What the notes say about each other, rebuilt when the files change.
#[derive(Debug, Default)]
pub(super) struct NotesIndex {
    /// Lowercase title, and lowercase path without `.md`, to the note's path.
    by_name: HashMap<String, String>,
    /// Each note's `[[link]]` targets, lowercase.
    links: HashMap<String, Vec<String>>,
    /// Each tag's notes.
    pub(super) tags: BTreeMap<String, Vec<String>>,
}

#[derive(Debug)]
pub(super) struct ClientNotesView {
    /// Bumped when the view moves to another space or server: late answers
    /// for the old one are dropped.
    pub(super) generation: u64,
    pub(super) boot_id: String,
    pub(super) workspace_id: String,
    /// The space's tab in front when Notes opened; choosing another tab closes it.
    tab_id: Option<String>,
    focus_requested_until: Option<Instant>,
    pub(super) load: NotesLoad,
    pub(super) vault: Option<NoteVault>,
    pub(super) index: NotesIndex,
    /// The note on screen, by path.
    pub(super) open: Option<String>,
    /// The open note's editor, while editing.
    pub(super) editor: Option<NoteEditor>,
    /// The fingerprint of the file the open note was read or last saved as.
    base: Option<String>,
    /// The editor revision that is on disk.
    saved_revision: u64,
    pub(super) write_in_flight: bool,
    save_due: Option<Instant>,
    /// A rename waiting for the save on its way, so the two never cross.
    pending_rename: Option<String>,
    /// The file changed on disk under unsaved edits.
    pub(super) conflict: bool,
    pub(super) doc_scroll: usize,
    pub(super) list_scroll: usize,
    pub(super) collapsed: HashSet<String>,
    pub(super) tag_filter: Option<String>,
    pub(super) switcher: Option<NotesSwitcher>,
    /// Delete was clicked once; the next click deletes.
    pub(super) confirm_delete: bool,
    pub(super) message: Option<String>,
    /// Read mode's rows at the last frame's width.
    pub(super) doc_rows: Vec<DocRow>,
    /// The editor's size at the last frame, for paging and wrapping keys.
    pub(super) editor_size: (u16, u16),
    /// Scroll the editor's cursor into view at the next frame.
    pub(super) reveal_cursor: bool,
    /// A mouse drag is selecting in the editor.
    selecting: bool,
    last_click: Option<(Instant, u16, u16)>,
    list_stale: bool,
    stamp_in_flight: bool,
    next_stamp_check: Instant,
    list_in_flight: bool,
}

/// A note's name: its file name without `.md`.
pub(super) fn title(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

/// The folder part of a path, without the trailing slash.
pub(super) fn folder(path: &str) -> &str {
    path.rfind('/').map_or("", |at| &path[..at])
}

/// "a/B.md" → "a/b": how links and the switcher match names.
fn name_key(name: &str) -> String {
    name.trim()
        .trim_end_matches(".md")
        .trim_matches('/')
        .to_lowercase()
}

/// Turns what was typed as a name into a note path: no `.md` needed, no
/// leading or trailing slashes, backslashes as folders.
fn note_path(name: &str) -> Option<String> {
    let name = name.trim().replace('\\', "/");
    let name = name.trim_matches('/');
    let name = name.strip_suffix(".md").unwrap_or(name).trim();
    (!name.is_empty()).then(|| format!("{name}.md"))
}

/// Orders names the way people count: "2" before "10", case ignored.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(text: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for ch in text.chars() {
            let digit = ch.is_ascii_digit();
            match out.last_mut() {
                Some((was_digit, chunk)) if *was_digit == digit => chunk.push(ch),
                _ => out.push((digit, ch.to_string())),
            }
        }
        out
    }
    let (a_chunks, b_chunks) = (chunks(a), chunks(b));
    for ((a_digit, a), (b_digit, b)) in a_chunks.iter().zip(&b_chunks) {
        let order = if *a_digit && *b_digit {
            let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
            a.len().cmp(&b.len()).then_with(|| a.cmp(b))
        } else {
            a.to_lowercase().cmp(&b.to_lowercase())
        };
        if order.is_ne() {
            return order;
        }
    }
    a_chunks.len().cmp(&b_chunks.len())
}

/// Whether `query`'s characters appear in `text` in order; lower scores are closer.
fn fuzzy_score(query: &str, text: &str) -> Option<usize> {
    let text = text.to_lowercase();
    let mut chars = text.char_indices();
    let mut score = 0;
    let mut last = None;
    for wanted in query.to_lowercase().chars() {
        let (at, _) = chars.find(|(_, ch)| *ch == wanted)?;
        if let Some(last) = last {
            score += at - last - 1;
        }
        last = Some(at);
    }
    Some(score * 2 + text.len())
}

impl NotesIndex {
    fn build(notes: &[NoteFile]) -> Self {
        let mut index = Self::default();
        for note in notes {
            index
                .by_name
                .insert(name_key(&note.path), note.path.clone());
            // A bare title finds the first note with it, like Obsidian.
            index
                .by_name
                .entry(name_key(title(&note.path)))
                .or_insert_with(|| note.path.clone());
            index.links.insert(
                note.path.clone(),
                notes_markdown::links(&note.content)
                    .iter()
                    .map(|target| name_key(target))
                    .collect(),
            );
            for tag in notes_markdown::tags(&note.content) {
                let notes = index.tags.entry(tag).or_default();
                if !notes.contains(&note.path) {
                    notes.push(note.path.clone());
                }
            }
        }
        index
    }
}

impl ClientNotesView {
    fn new(generation: u64, boot_id: String, workspace_id: String) -> Self {
        Self {
            generation,
            boot_id,
            workspace_id,
            tab_id: None,
            focus_requested_until: None,
            load: NotesLoad::Loading,
            vault: None,
            index: NotesIndex::default(),
            open: None,
            editor: None,
            base: None,
            saved_revision: 0,
            write_in_flight: false,
            save_due: None,
            pending_rename: None,
            conflict: false,
            doc_scroll: 0,
            list_scroll: 0,
            collapsed: HashSet::new(),
            tag_filter: None,
            switcher: None,
            confirm_delete: false,
            message: None,
            doc_rows: Vec::new(),
            editor_size: (80, 20),
            reveal_cursor: false,
            selecting: false,
            last_click: None,
            list_stale: true,
            stamp_in_flight: false,
            next_stamp_check: Instant::now() + STAMP_INTERVAL,
            list_in_flight: false,
        }
    }

    pub(super) fn notes(&self) -> &[NoteFile] {
        self.vault
            .as_ref()
            .map_or(&[], |vault| vault.notes.as_slice())
    }

    pub(super) fn note(&self, path: &str) -> Option<&NoteFile> {
        self.notes().iter().find(|note| note.path == path)
    }

    fn note_mut(&mut self, path: &str) -> Option<&mut NoteFile> {
        self.vault
            .as_mut()?
            .notes
            .iter_mut()
            .find(|note| note.path == path)
    }

    /// The note a `[[link]]` or typed name means.
    pub(super) fn resolve(&self, name: &str) -> Option<&str> {
        self.index.by_name.get(&name_key(name)).map(String::as_str)
    }

    /// The open note's text: the editor's while editing.
    pub(super) fn open_text(&self) -> Option<String> {
        if let Some(editor) = self.editor.as_ref() {
            return Some(editor.text());
        }
        self.open
            .as_deref()
            .and_then(|path| self.note(path))
            .map(|note| note.content.clone())
    }

    pub(super) fn dirty(&self) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|editor| editor.revision() != self.saved_revision)
    }

    /// Notes that link to the open one.
    pub(super) fn backlinks(&self) -> Vec<&str> {
        let Some(open) = self.open.as_deref() else {
            return Vec::new();
        };
        let names = [name_key(open), name_key(title(open))];
        let mut out: Vec<&str> = self
            .index
            .links
            .iter()
            .filter(|(path, links)| {
                path.as_str() != open && links.iter().any(|link| names.contains(link))
            })
            .map(|(path, _)| path.as_str())
            .collect();
        out.sort_by(|a, b| natural_cmp(title(a), title(b)));
        out
    }

    /// The note list: folders first, then notes, each sorted naturally;
    /// a tag filter lists its notes flat.
    pub(super) fn list_rows(&self) -> Vec<ListRow> {
        if let Some(tag) = self.tag_filter.as_ref() {
            let mut paths: Vec<&String> = self
                .index
                .tags
                .get(tag)
                .map(|paths| paths.iter().collect())
                .unwrap_or_default();
            paths.sort_by(|a, b| natural_cmp(title(a), title(b)));
            return paths
                .into_iter()
                .map(|path| ListRow::Note {
                    path: path.clone(),
                    title: title(path).to_owned(),
                    depth: 0,
                })
                .collect();
        }
        let mut paths: Vec<&str> = self.notes().iter().map(|note| note.path.as_str()).collect();
        paths.sort_by(|a, b| natural_cmp(a, b));
        let mut rows = Vec::new();
        self.push_folder(&paths, "", 0, &mut rows);
        rows
    }

    fn push_folder(&self, paths: &[&str], prefix: &str, depth: usize, rows: &mut Vec<ListRow>) {
        let mut folders: Vec<&str> = Vec::new();
        let mut notes: Vec<&str> = Vec::new();
        for path in paths {
            let Some(rest) = path.strip_prefix(prefix) else {
                continue;
            };
            match rest.split_once('/') {
                Some((name, _)) if !folders.contains(&name) => folders.push(name),
                Some(_) => {}
                None => notes.push(path),
            }
        }
        folders.sort_by(|a, b| natural_cmp(a, b));
        notes.sort_by(|a, b| natural_cmp(title(a), title(b)));
        for name in folders {
            let path = format!("{prefix}{name}");
            let open = !self.collapsed.contains(&path);
            rows.push(ListRow::Folder {
                path: path.clone(),
                name: name.to_owned(),
                depth,
                open,
            });
            if open {
                self.push_folder(paths, &format!("{path}/"), depth + 1, rows);
            }
        }
        for path in notes {
            rows.push(ListRow::Note {
                path: path.to_owned(),
                title: title(path).to_owned(),
                depth,
            });
        }
    }

    /// The switcher's rows for what is typed.
    pub(super) fn switcher_choices(&self) -> Vec<SwitcherChoice> {
        let Some(switcher) = self.switcher.as_ref() else {
            return Vec::new();
        };
        let query = switcher.query.as_str().trim().to_owned();
        if switcher.purpose == SwitcherPurpose::Rename {
            return note_path(&query)
                .map(SwitcherChoice::Rename)
                .into_iter()
                .collect();
        }
        let mut choices: Vec<SwitcherChoice> = Vec::new();
        let create = note_path(&query)
            .filter(|_| !query.starts_with('#'))
            .filter(|path| self.resolve(path).is_none());
        if switcher.purpose == SwitcherPurpose::New {
            choices.extend(create.clone().map(SwitcherChoice::Create));
        }
        if let Some(tag) = query.strip_prefix('#') {
            let tag = tag.to_lowercase();
            for (name, paths) in &self.index.tags {
                if name.to_lowercase().starts_with(&tag) {
                    for path in paths {
                        let choice = SwitcherChoice::Open(path.clone());
                        if !choices.contains(&choice) {
                            choices.push(choice);
                        }
                    }
                }
            }
        } else {
            let mut found: Vec<(usize, &str)> = self
                .notes()
                .iter()
                .filter_map(|note| {
                    let by_title = fuzzy_score(&query, title(&note.path));
                    let by_path = fuzzy_score(&query, &note.path).map(|score| score + 50);
                    by_title
                        .into_iter()
                        .chain(by_path)
                        .min()
                        .map(|score| (score, note.path.as_str()))
                })
                .collect();
            found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| natural_cmp(a.1, b.1)));
            choices.extend(
                found
                    .into_iter()
                    .take(50)
                    .map(|(_, path)| SwitcherChoice::Open(path.to_owned())),
            );
        }
        if switcher.purpose == SwitcherPurpose::Find {
            choices.extend(create.map(SwitcherChoice::Create));
        }
        choices
    }

    /// The absolute file of a note.
    pub(super) fn file_path(&self, path: &str) -> Option<String> {
        let root = &self.vault.as_ref()?.root;
        Some(
            std::path::Path::new(root)
                .join(path)
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn set_vault(&mut self, vault: NoteVault) {
        self.index = NotesIndex::build(&vault.notes);
        self.vault = Some(vault);
    }

    fn rebuild_index(&mut self) {
        self.index = NotesIndex::build(self.notes());
    }
}

impl ClientShellState {
    /// Opens Notes on the focused space, or closes it when open.
    pub(super) fn toggle_notes_view(&mut self, outcome: &mut ClientShellInput) {
        if self.notes_view.is_some() {
            self.close_notes_view(outcome);
        } else {
            self.open_notes_view(None, outcome);
        }
    }

    /// Opens Notes, focusing `workspace_id` first when given. It takes the
    /// git view's and the Board's place: one stand-in tab at a time.
    pub(super) fn open_notes_view(
        &mut self,
        workspace_id: Option<String>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let focused = snapshot.focused_workspace_id.clone();
        let Some(target) = workspace_id.or_else(|| focused.clone()) else {
            return;
        };
        let boot_id = snapshot.boot_id.clone();
        let focused_tab = snapshot.focused_tab_id.clone();
        let needs_focus = focused.as_deref() != Some(target.as_str());
        if needs_focus {
            self.push_endpoint_method(
                Method::WorkspaceFocus(crate::api::schema::WorkspaceTarget {
                    workspace_id: target.clone(),
                }),
                outcome,
            );
        }
        self.close_git_view(outcome);
        self.close_board_view(outcome);
        let reuse = self
            .notes_view
            .as_ref()
            .is_some_and(|view| view.workspace_id == target && view.boot_id == boot_id);
        if !reuse {
            self.notes_view_generation = self.notes_view_generation.wrapping_add(1);
            self.notes_view = Some(ClientNotesView::new(
                self.notes_view_generation,
                boot_id,
                target,
            ));
        }
        if let Some(view) = self.notes_view.as_mut() {
            if needs_focus {
                view.tab_id = None;
                view.focus_requested_until = Some(Instant::now() + Duration::from_secs(3));
            } else {
                view.tab_id = focused_tab;
            }
        }
        self.mode = self.copy_or_terminal_mode();
        outcome.repaint = true;
        self.load_notes_view(outcome);
    }

    /// Closes Notes; unsaved typing is sent on its way first.
    pub(super) fn close_notes_view(&mut self, outcome: &mut ClientShellInput) {
        self.flush_note(outcome);
        if self.notes_view.take().is_some() {
            outcome.repaint = true;
        }
    }

    /// Follows the focused space like the Board does; choosing another of the
    /// space's tabs puts its panes back. Snapshots come without an outcome, so
    /// a note with unsaved typing keeps the view until the tick saves it.
    pub(super) fn retarget_notes_view(&mut self) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            self.notes_view = None;
            return;
        };
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let Some(focused) = snapshot.focused_workspace_id.clone() else {
            self.notes_view = None;
            return;
        };
        let same_boot = view.boot_id == snapshot.boot_id;
        if same_boot && view.workspace_id == focused {
            view.focus_requested_until = None;
            match view.tab_id.as_deref() {
                None => view.tab_id = snapshot.focused_tab_id.clone(),
                Some(tab_id) if snapshot.focused_tab_id.as_deref() != Some(tab_id) => {
                    self.notes_view_closing = true;
                }
                Some(_) => {}
            }
            return;
        }
        if same_boot
            && view
                .focus_requested_until
                .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        if same_boot && view.dirty() {
            // Save before following focus; the tick does both.
            self.notes_view_retarget = Some(focused);
            return;
        }
        let boot_id = snapshot.boot_id.clone();
        let focused_tab = snapshot.focused_tab_id.clone();
        self.notes_view_generation = self.notes_view_generation.wrapping_add(1);
        let mut view = ClientNotesView::new(self.notes_view_generation, boot_id, focused);
        view.tab_id = focused_tab;
        self.notes_view = Some(view);
        self.notes_view_needs_load = true;
    }

    /// Sends a load a retarget left waiting, saves typing that paused, and
    /// every [`STAMP_INTERVAL`] asks whether the files changed (an agent, an
    /// editor, Obsidian). Nothing happens while Notes is closed.
    pub(crate) fn tick_notes_view(&mut self, now: Instant) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        if std::mem::take(&mut self.notes_view_closing) {
            self.close_notes_view(&mut outcome);
            return outcome;
        }
        if let Some(workspace_id) = self.notes_view_retarget.take() {
            self.flush_note(&mut outcome);
            if let Some(boot_id) = self.notes_view.as_ref().map(|view| view.boot_id.clone()) {
                self.notes_view_generation = self.notes_view_generation.wrapping_add(1);
                let mut view =
                    ClientNotesView::new(self.notes_view_generation, boot_id, workspace_id);
                view.tab_id = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| snapshot.focused_tab_id.clone());
                self.notes_view = Some(view);
                self.notes_view_needs_load = true;
                outcome.repaint = true;
            }
        }
        if std::mem::take(&mut self.notes_view_needs_load) {
            self.load_notes_view(&mut outcome);
        }
        if self
            .notes_view
            .as_ref()
            .and_then(|view| view.save_due)
            .is_some_and(|due| now >= due)
        {
            self.flush_note(&mut outcome);
        }
        let in_background = self.outer_focused == Some(false);
        let Some(view) = self.notes_view.as_mut() else {
            return outcome;
        };
        if view.load != NotesLoad::Ready
            || in_background
            || now < view.next_stamp_check
            || view.stamp_in_flight
            || view.list_in_flight
        {
            return outcome;
        }
        view.next_stamp_check = now + STAMP_INTERVAL;
        view.stamp_in_flight = true;
        let generation = view.generation;
        let method = Method::NoteStamp(crate::api::schema::NoteTarget {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
        });
        self.send_notes_request(method, generation, NotesRequest::Stamp, &mut outcome);
        outcome
    }

    pub(super) fn load_notes_view(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        if view.load == NotesLoad::Unsupported || !view.list_stale || view.list_in_flight {
            return;
        }
        view.list_stale = false;
        view.list_in_flight = true;
        let generation = view.generation;
        let method = Method::NoteList(crate::api::schema::NoteTarget {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
        });
        self.send_notes_request(method, generation, NotesRequest::List, outcome);
    }

    /// Returns false (and marks Notes unsupported) when the server cannot take it.
    fn send_notes_request(
        &mut self,
        method: Method,
        generation: u64,
        request: NotesRequest,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let supported = self.supports_endpoint_method(&method);
        let is_write = matches!(request, NotesRequest::Write { .. });
        let rereads_list = matches!(request, NotesRequest::List);
        let sent = self.push_endpoint_method_with_kind(
            method,
            PendingEndpointKind::Notes {
                generation,
                request,
            },
            outcome,
        );
        if !sent {
            if let Some(view) = self.notes_view.as_mut() {
                if !supported {
                    view.load = NotesLoad::Unsupported;
                } else if is_write {
                    view.message = Some("not saved: the server is not reachable".to_owned());
                }
                view.list_stale |= rereads_list;
                view.list_in_flight = false;
                view.stamp_in_flight = false;
                view.write_in_flight = false;
            }
            outcome.repaint = true;
        }
        sent
    }

    pub(super) fn refresh_notes_view(&mut self, outcome: &mut ClientShellInput) {
        if let Some(view) = self.notes_view.as_mut() {
            if view.load == NotesLoad::Unsupported {
                view.load = NotesLoad::Loading;
            }
            // An answer that never came must not block reading again.
            view.list_stale = true;
            view.list_in_flight = false;
            outcome.repaint = true;
        }
        self.load_notes_view(outcome);
    }

    /// Sends the editor's text when it changed since the last save. One
    /// write at a time: typing during a save is sent when it comes back.
    fn flush_note(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        view.save_due = None;
        if !view.dirty() || view.conflict {
            return;
        }
        if view.write_in_flight {
            view.save_due = Some(Instant::now() + AUTOSAVE_DELAY);
            return;
        }
        let (Some(path), Some(editor)) = (view.open.clone(), view.editor.as_ref()) else {
            return;
        };
        let content = editor.text();
        let revision = editor.revision();
        view.write_in_flight = true;
        let generation = view.generation;
        let method = Method::NoteWrite(crate::api::schema::NoteWriteParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            path: path.clone(),
            content: content.clone(),
            base_fingerprint: view.base.clone(),
        });
        outcome.repaint = true;
        self.send_notes_request(
            method,
            generation,
            NotesRequest::Write {
                path,
                content,
                revision: Some(revision),
                open: false,
            },
            outcome,
        );
    }

    /// Shows `path`, in the editor when `edit`. Unsaved typing in the note
    /// it replaces is sent first.
    pub(super) fn open_note(&mut self, path: String, edit: bool, outcome: &mut ClientShellInput) {
        let same = self
            .notes_view
            .as_ref()
            .is_some_and(|view| view.open.as_ref() == Some(&path));
        if same {
            if edit {
                self.start_editing(None, outcome);
            }
            return;
        }
        self.flush_note(outcome);
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let Some(note) = view.note(&path) else {
            return;
        };
        view.base = Some(note.fingerprint.clone());
        view.open = Some(path.clone());
        view.editor = None;
        view.saved_revision = 0;
        view.conflict = false;
        view.confirm_delete = false;
        view.doc_scroll = 0;
        view.message = None;
        if let Some(root) = view.vault.as_ref().map(|vault| vault.root.clone()) {
            self.notes_last_open.insert(root, path);
        }
        outcome.repaint = true;
        if edit {
            self.start_editing(None, outcome);
        }
    }

    /// Switches the open note to the editor, with the cursor at `at` (line,
    /// byte) or where it last was.
    pub(super) fn start_editing(
        &mut self,
        at: Option<(usize, usize)>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        if view.editor.is_none() {
            let Some(content) = view.open_text() else {
                return;
            };
            let mut editor = NoteEditor::new(&content);
            // From the keyboard, writing carries on at the end of the note.
            let last = content.split('\n').count().saturating_sub(1);
            let end = content.rsplit('\n').next().map_or(0, str::len);
            editor.set_cursor(last, end);
            view.saved_revision = editor.revision();
            view.editor = Some(editor);
        }
        if let (Some((line, byte)), Some(editor)) = (at, view.editor.as_mut()) {
            editor.set_cursor(line, byte);
        }
        view.reveal_cursor = true;
        view.confirm_delete = false;
        outcome.repaint = true;
    }

    /// Back to reading: the editor's text is saved and shown rendered.
    pub(super) fn stop_editing(&mut self, outcome: &mut ClientShellInput) {
        self.flush_note(outcome);
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let Some(editor) = view.editor.take() else {
            return;
        };
        let content = editor.text();
        if let Some(path) = view.open.clone() {
            // Read mode shows what was typed even before the save comes back.
            if let Some(note) = view.note_mut(&path) {
                note.content = content;
            }
            view.rebuild_index();
        }
        outcome.repaint = true;
    }

    fn edited(&mut self) {
        if let Some(view) = self.notes_view.as_mut() {
            view.save_due = Some(Instant::now() + AUTOSAVE_DELAY);
            view.reveal_cursor = true;
            view.message = None;
        }
    }

    /// Ticks or unticks the task on `line` of the open note and saves it.
    fn toggle_checkbox(&mut self, line: usize, outcome: &mut ClientShellInput) {
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let (Some(path), Some(text)) = (view.open.clone(), view.open_text()) else {
            return;
        };
        if view.write_in_flight || view.conflict {
            return;
        }
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let Some(source) = lines.get_mut(line) else {
            return;
        };
        let indent = source.len() - source.trim_start_matches(' ').len();
        let mark = indent + 3;
        let flipped = match source.get(mark..mark + 1) {
            Some(" ") => "x",
            Some("x" | "X") => " ",
            _ => return,
        };
        source.replace_range(mark..mark + 1, flipped);
        let content = lines.join("\n");
        if let Some(note) = view.note_mut(&path) {
            note.content.clone_from(&content);
        }
        view.write_in_flight = true;
        let generation = view.generation;
        let method = Method::NoteWrite(crate::api::schema::NoteWriteParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            path: path.clone(),
            content: content.clone(),
            base_fingerprint: view.base.clone(),
        });
        outcome.repaint = true;
        self.send_notes_request(
            method,
            generation,
            NotesRequest::Write {
                path,
                content,
                revision: None,
                open: false,
            },
            outcome,
        );
    }

    /// Opens the note called `name`, or creates it when there is none.
    pub(super) fn open_or_create_note(&mut self, name: &str, outcome: &mut ClientShellInput) {
        let Some(view) = self.notes_view.as_ref() else {
            return;
        };
        if let Some(path) = view.resolve(name).map(str::to_owned) {
            self.open_note(path, false, outcome);
            return;
        }
        let Some(path) = note_path(name) else {
            return;
        };
        let content = format!("# {}\n\n", title(&path));
        self.create_note(path, content, outcome);
    }

    fn create_note(&mut self, path: String, content: String, outcome: &mut ClientShellInput) {
        self.flush_note(outcome);
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let generation = view.generation;
        let method = Method::NoteWrite(crate::api::schema::NoteWriteParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            path: path.clone(),
            content: content.clone(),
            base_fingerprint: None,
        });
        self.send_notes_request(
            method,
            generation,
            NotesRequest::Write {
                path,
                content,
                revision: None,
                open: true,
            },
            outcome,
        );
    }

    /// Today's daily note, `daily/YYYY-MM-DD.md`, made when missing.
    pub(super) fn open_today_note(&mut self, outcome: &mut ClientShellInput) {
        let Some(today) = crate::platform::local_datetime() else {
            return;
        };
        let date = format!(
            "{:04}-{:02}-{:02}",
            today.year(),
            u8::from(today.month()),
            today.day()
        );
        let path = format!("daily/{date}.md");
        let exists = self
            .notes_view
            .as_ref()
            .is_some_and(|view| view.note(&path).is_some());
        if exists {
            // A daily note is for jotting down: it opens ready to type at its end.
            self.open_note(path, true, outcome);
        } else {
            self.create_note(path, format!("# {date}\n\n- "), outcome);
        }
    }

    fn rename_open_note(&mut self, to: String, outcome: &mut ClientShellInput) {
        self.flush_note(outcome);
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let Some(from) = view.open.clone().filter(|from| *from != to) else {
            return;
        };
        if view.write_in_flight {
            view.pending_rename = Some(to);
            return;
        }
        let generation = view.generation;
        let method = Method::NoteRename(crate::api::schema::NoteRenameParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            from: from.clone(),
            to: to.clone(),
        });
        self.send_notes_request(
            method,
            generation,
            NotesRequest::Rename { from, to },
            outcome,
        );
    }

    /// The first click arms Delete, the second moves the note to the
    /// project's `.trash` folder.
    pub(super) fn delete_open_note(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let Some(path) = view.open.clone() else {
            return;
        };
        outcome.repaint = true;
        if !view.confirm_delete {
            view.confirm_delete = true;
            return;
        }
        view.confirm_delete = false;
        // Whatever was typed goes to the trash with the note, not after it.
        view.save_due = None;
        if let Some(editor) = view.editor.as_ref() {
            view.saved_revision = editor.revision();
        }
        let generation = view.generation;
        let method = Method::NoteDelete(crate::api::schema::NoteDeleteParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            path: path.clone(),
        });
        self.send_notes_request(method, generation, NotesRequest::Delete { path }, outcome);
    }

    /// Opens a file of the vault, such as a pasted image, in the desktop's
    /// viewer. Only a local server's files are on this machine.
    fn open_note_file(&mut self, path: &str, outcome: &mut ClientShellInput) {
        let local = self.active_endpoint_id == ClientEndpointId::Local;
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let file = view.file_path(path);
        match file {
            Some(file) if local => outcome.actions.push(ClientShellAction::OpenPath(file)),
            Some(file) => view.message = Some(format!("{file} is on the server")),
            None => {}
        }
        outcome.repaint = true;
    }

    /// Whether a paste goes into the note being edited.
    pub(super) fn notes_editor_focused(&self) -> bool {
        self.overlay.is_none()
            && self
                .notes_view
                .as_ref()
                .is_some_and(|view| view.switcher.is_none() && view.editor.is_some())
    }

    /// Pastes into the note being edited: the clipboard's text when it has
    /// some, else its image, saved into the vault and embedded at the cursor.
    pub(super) fn paste_into_note(
        &mut self,
        text: Option<String>,
        read_image: impl FnOnce() -> Option<crate::platform::ClipboardImage>,
        outcome: &mut ClientShellInput,
    ) {
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            outcome.repaint |= self.insert_notes_view_text(&text);
            return;
        }
        let Some(image) = read_image() else {
            return;
        };
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let Some(note) = view.open.clone() else {
            return;
        };
        outcome.repaint = true;
        if image.bytes.len() > crate::protocol::MAX_CLIPBOARD_IMAGE_PAYLOAD {
            view.message = Some("the image is too large to paste (16 MiB at most)".to_owned());
            return;
        }
        view.message = Some("pasting image…".to_owned());
        let generation = view.generation;
        let method = Method::NoteAttach(crate::api::schema::NoteAttachParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            extension: image.extension.to_owned(),
            data: {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(&image.bytes)
            },
        });
        self.send_notes_request(method, generation, NotesRequest::Attach { note }, outcome);
    }

    /// Types the open note's file path into the space's focused pane, usually
    /// an agent's prompt, and goes there.
    pub(super) fn send_note_to_agent(&mut self, outcome: &mut ClientShellInput) {
        self.flush_note(outcome);
        let Some(file) = self
            .notes_view
            .as_ref()
            .and_then(|view| view.open.as_deref().and_then(|path| view.file_path(path)))
        else {
            return;
        };
        let Some(pane_id) = self
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_pane_id.clone())
        else {
            return;
        };
        let text = if file.contains(char::is_whitespace) {
            format!("'{file}' ")
        } else {
            format!("{file} ")
        };
        self.close_notes_view(outcome);
        super::push_target_event(
            ClientInputTarget::Pane(pane_id),
            ClientPaneInputEvent::Paste(text),
            outcome,
        );
    }

    pub(super) fn open_notes_switcher(
        &mut self,
        purpose: SwitcherPurpose,
        outcome: &mut ClientShellInput,
    ) {
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        if view.load != NotesLoad::Ready {
            return;
        }
        let query = match purpose {
            SwitcherPurpose::Rename => match view.open.as_deref() {
                Some(path) => TextEditor::from(path.strip_suffix(".md").unwrap_or(path)),
                None => return,
            },
            _ => TextEditor::default(),
        };
        view.switcher = Some(NotesSwitcher {
            purpose,
            query,
            selected: 0,
        });
        view.confirm_delete = false;
        outcome.repaint = true;
    }

    fn choose_switcher(&mut self, choice: SwitcherChoice, outcome: &mut ClientShellInput) {
        if let Some(view) = self.notes_view.as_mut() {
            view.switcher = None;
        }
        outcome.repaint = true;
        match choice {
            SwitcherChoice::Open(path) => self.open_note(path, false, outcome),
            SwitcherChoice::Create(path) => {
                let content = format!("# {}\n\n", title(&path));
                self.create_note(path, content, outcome);
            }
            SwitcherChoice::Rename(to) => self.rename_open_note(to, outcome),
        }
    }

    /// The note before or after the open one in the list.
    fn step_note(&mut self, delta: isize, outcome: &mut ClientShellInput) {
        let Some(view) = self.notes_view.as_ref() else {
            return;
        };
        let notes: Vec<String> = view
            .list_rows()
            .into_iter()
            .filter_map(|row| match row {
                ListRow::Note { path, .. } => Some(path),
                ListRow::Folder { .. } => None,
            })
            .collect();
        let current = view
            .open
            .as_ref()
            .and_then(|open| notes.iter().position(|path| path == open));
        let next = match current {
            Some(index) => index
                .saturating_add_signed(delta)
                .min(notes.len().saturating_sub(1)),
            None => 0,
        };
        if let Some(path) = notes.get(next).cloned() {
            self.open_note(path, false, outcome);
        }
    }

    /// Jumps to a heading: the editor's cursor goes there, or reading scrolls to it.
    fn go_to_line(&mut self, line: usize, outcome: &mut ClientShellInput) {
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        if let Some(editor) = view.editor.as_mut() {
            editor.set_cursor(line, 0);
            view.reveal_cursor = true;
        } else if let Some(row) = view.doc_rows.iter().position(|row| row.line == line) {
            view.doc_scroll = row;
        }
        outcome.repaint = true;
    }

    /// Keys while Notes is in front and no popup or modal takes them.
    pub(super) fn route_notes_view_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let Some(view) = self.notes_view.as_mut() else {
            return;
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        if let Some(switcher) = view.switcher.as_mut() {
            match code {
                KeyCode::Esc => view.switcher = None,
                KeyCode::Enter => {
                    let selected = switcher.selected;
                    let choice = view.switcher_choices().into_iter().nth(selected);
                    match choice {
                        Some(choice) => self.choose_switcher(choice, outcome),
                        None => return,
                    }
                }
                KeyCode::Down | KeyCode::Tab => {
                    let count = view.switcher_choices().len();
                    if let Some(switcher) = view.switcher.as_mut() {
                        switcher.selected = (switcher.selected + 1).min(count.saturating_sub(1));
                    }
                }
                KeyCode::Up | KeyCode::BackTab => {
                    switcher.selected = switcher.selected.saturating_sub(1);
                }
                _ => {
                    if switcher.query.handle_key(key).is_none() {
                        return;
                    }
                    switcher.selected = 0;
                }
            }
            outcome.repaint = true;
            return;
        }
        // Shortcuts that work while reading and while editing.
        if ctrl {
            match code {
                KeyCode::Char('o') | KeyCode::Char('p') => {
                    self.open_notes_switcher(SwitcherPurpose::Find, outcome);
                    return;
                }
                KeyCode::Char('n') => {
                    self.open_notes_switcher(SwitcherPurpose::New, outcome);
                    return;
                }
                KeyCode::Char('e') => {
                    if view.editor.is_some() {
                        self.stop_editing(outcome);
                    } else {
                        self.start_editing(None, outcome);
                    }
                    return;
                }
                KeyCode::Char('s') => {
                    self.flush_note(outcome);
                    return;
                }
                _ => {}
            }
        }
        if let Some(editor) = view.editor.as_mut() {
            if code == KeyCode::Esc {
                self.stop_editing(outcome);
                return;
            }
            let (width, height) = view.editor_size;
            match editor.handle_key(key, width, height) {
                EditorOutcome::Changed => self.edited(),
                EditorOutcome::Moved => view.reveal_cursor = true,
                EditorOutcome::Copy(text) => {
                    outcome
                        .actions
                        .push(ClientShellAction::ClipboardWrite(text.into_bytes()));
                    // Cut changes the text too; copy leaves it.
                    if view.dirty() {
                        self.edited();
                    }
                }
                EditorOutcome::Unhandled => return,
            }
            outcome.repaint = true;
            return;
        }
        if !modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            return;
        }
        let page = usize::from(view.editor_size.1.max(2) - 1);
        match code {
            KeyCode::Esc if view.confirm_delete => {
                view.confirm_delete = false;
                outcome.repaint = true;
            }
            KeyCode::Esc if view.tag_filter.is_some() => {
                view.tag_filter = None;
                view.list_scroll = 0;
                outcome.repaint = true;
            }
            KeyCode::Esc => self.close_notes_view(outcome),
            KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char('i') => {
                self.start_editing(None, outcome)
            }
            KeyCode::Char('/') => self.open_notes_switcher(SwitcherPurpose::Find, outcome),
            KeyCode::Char('n') => self.open_notes_switcher(SwitcherPurpose::New, outcome),
            KeyCode::Char('r') => self.open_notes_switcher(SwitcherPurpose::Rename, outcome),
            KeyCode::Char('d') => self.delete_open_note(outcome),
            KeyCode::Char('t') => self.open_today_note(outcome),
            KeyCode::Char('a') => self.send_note_to_agent(outcome),
            KeyCode::Char(']') => self.step_note(1, outcome),
            KeyCode::Char('[') => self.step_note(-1, outcome),
            KeyCode::Char('R') => self.refresh_notes_view(outcome),
            KeyCode::Down | KeyCode::Char('j') => {
                view.doc_scroll = view.doc_scroll.saturating_add(1);
                outcome.repaint = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                view.doc_scroll = view.doc_scroll.saturating_sub(1);
                outcome.repaint = true;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                view.doc_scroll = view.doc_scroll.saturating_add(page);
                outcome.repaint = true;
            }
            KeyCode::PageUp => {
                view.doc_scroll = view.doc_scroll.saturating_sub(page);
                outcome.repaint = true;
            }
            KeyCode::Home | KeyCode::Char('g') => {
                view.doc_scroll = 0;
                outcome.repaint = true;
            }
            KeyCode::End | KeyCode::Char('G') => {
                view.doc_scroll = usize::MAX;
                outcome.repaint = true;
            }
            _ => {}
        }
    }

    /// Typed text: into the switcher or the editor, else a lone character is that key.
    pub(super) fn route_notes_view_text(&mut self, text: &str, outcome: &mut ClientShellInput) {
        if self.notes_view_text_field_focused() {
            outcome.repaint |= self.insert_notes_view_text(text);
            return;
        }
        let mut chars = text.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            let key = crate::input::TerminalKey::new(KeyCode::Char(ch), KeyModifiers::empty());
            self.route_notes_view_key(&key, outcome);
        }
    }

    /// Pasted or typed text goes to the switcher, else the editor.
    pub(super) fn insert_notes_view_text(&mut self, text: &str) -> bool {
        let Some(view) = self.notes_view.as_mut() else {
            return false;
        };
        if let Some(switcher) = view.switcher.as_mut() {
            switcher.selected = 0;
            // A name is one line.
            return switcher.query.insert(&text.replace(['\r', '\n'], " "));
        }
        let Some(editor) = view.editor.as_mut() else {
            return false;
        };
        let before = editor.revision();
        editor.insert(text);
        if editor.revision() != before {
            self.edited();
        }
        true
    }

    pub(super) fn notes_view_text_field_focused(&self) -> bool {
        self.notes_view
            .as_ref()
            .is_some_and(|view| view.switcher.is_some() || view.editor.is_some())
    }

    pub(super) fn route_notes_view_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (mouse.column, mouse.row);
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && (super::contains(self.hits.new_tab, point)
                || self
                    .hits
                    .tabs
                    .iter()
                    .any(|(rect, _)| super::contains(*rect, point)))
        {
            // A real tab brings its panes back.
            self.close_notes_view(outcome);
            return false;
        }
        let hits = &self.hits.notes_view;
        let selecting = self.notes_view.as_ref().is_some_and(|view| view.selecting);
        if selecting {
            // A selection follows the pointer anywhere until it is let go.
            let editor = hits.editor;
            if let Some(view) = self.notes_view.as_mut() {
                match mouse.kind {
                    MouseEventKind::Drag(MouseButton::Left) => {
                        let row = usize::from(mouse.row.saturating_sub(editor.y));
                        let x = usize::from(mouse.column.saturating_sub(editor.x));
                        let width = view.editor_size.0;
                        if let Some(editor) = view.editor.as_mut() {
                            editor.click(row + editor.scroll(), x, width, true);
                        }
                        view.reveal_cursor = true;
                        outcome.repaint = true;
                    }
                    MouseEventKind::Up(_) => view.selecting = false,
                    _ => {}
                }
            }
            return true;
        }
        if !super::contains(hits.area, point)
            || super::contains(self.hits.notification_toast, point)
        {
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta: isize = if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                let (list, doc, popup) = (hits.list, hits.doc, hits.switcher);
                let Some(view) = self.notes_view.as_mut() else {
                    return true;
                };
                if super::contains(popup, point) {
                    if let Some(switcher) = view.switcher.as_mut() {
                        switcher.selected = switcher.selected.saturating_add_signed(delta.signum());
                    }
                } else if super::contains(list, point) {
                    view.list_scroll = view.list_scroll.saturating_add_signed(delta);
                } else if super::contains(doc, point) {
                    let (width, height) = view.editor_size;
                    match view.editor.as_mut() {
                        Some(editor) => editor.scroll_by(delta, width, height),
                        None => view.doc_scroll = view.doc_scroll.saturating_add_signed(delta),
                    }
                }
                outcome.repaint = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let target = hits
                    .targets
                    .iter()
                    .rev()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, target)| target.clone());
                let doc = hits.doc;
                let editor_rect = hits.editor;
                if let Some(view) = self.notes_view.as_mut() {
                    if view.switcher.is_some()
                        && !matches!(
                            target,
                            Some(NotesTarget::Switcher(_) | NotesTarget::SwitcherInput)
                        )
                    {
                        view.switcher = None;
                        outcome.repaint = true;
                        return true;
                    }
                    if view.confirm_delete && target != Some(NotesTarget::Delete) {
                        view.confirm_delete = false;
                        outcome.repaint = true;
                    }
                }
                match target {
                    Some(NotesTarget::Editor) => {
                        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
                        let now = Instant::now();
                        let Some(view) = self.notes_view.as_mut() else {
                            return true;
                        };
                        let row = usize::from(mouse.row.saturating_sub(editor_rect.y));
                        let x = usize::from(mouse.column.saturating_sub(editor_rect.x));
                        let width = view.editor_size.0;
                        let double = view.last_click.is_some_and(|(at, cx, cy)| {
                            now.duration_since(at) < DOUBLE_CLICK && (cx, cy) == point
                        });
                        if let Some(editor) = view.editor.as_mut() {
                            let row = row + editor.scroll();
                            if double {
                                editor.select_word_at(row, x, width);
                            } else {
                                editor.click(row, x, width, shift);
                                view.selecting = true;
                            }
                        }
                        view.last_click = (!double).then_some((now, point.0, point.1));
                        view.reveal_cursor = true;
                        outcome.repaint = true;
                    }
                    Some(NotesTarget::Doc) => {
                        let row = usize::from(mouse.row.saturating_sub(doc.y));
                        let x = mouse.column.saturating_sub(doc.x);
                        self.click_doc(row, x, outcome);
                    }
                    Some(target) => self.activate_notes_target(target, outcome),
                    None => {}
                }
            }
            _ => {}
        }
        true
    }

    /// A click on the rendered note: links, tags and boxes do their thing;
    /// anywhere else starts editing right there.
    fn click_doc(&mut self, row: usize, x: u16, outcome: &mut ClientShellInput) {
        let Some(view) = self.notes_view.as_ref() else {
            return;
        };
        let Some(doc_row) = view.doc_rows.get(view.doc_scroll + row) else {
            self.start_editing(None, outcome);
            return;
        };
        match doc_row.target_at(x).cloned() {
            Some(DocTarget::Link(name)) => self.open_or_create_note(&name, outcome),
            Some(DocTarget::Tag(tag)) => {
                if let Some(view) = self.notes_view.as_mut() {
                    view.tag_filter = Some(tag);
                    view.list_scroll = 0;
                }
                outcome.repaint = true;
            }
            Some(DocTarget::Url(url)) => {
                outcome.actions.push(ClientShellAction::OpenSafeWebUrl(url));
            }
            Some(DocTarget::Checkbox(line)) => self.toggle_checkbox(line, outcome),
            Some(DocTarget::Image(path)) => self.open_note_file(&path, outcome),
            None => {
                let at = doc_row
                    .source_offset(x)
                    .map(|byte| (doc_row.line, byte))
                    .or(Some((doc_row.line, 0)));
                self.start_editing(at, outcome);
            }
        }
    }

    fn activate_notes_target(&mut self, target: NotesTarget, outcome: &mut ClientShellInput) {
        match target {
            NotesTarget::Close => self.close_notes_view(outcome),
            NotesTarget::Retry => self.refresh_notes_view(outcome),
            NotesTarget::New => self.open_notes_switcher(SwitcherPurpose::New, outcome),
            NotesTarget::Find => self.open_notes_switcher(SwitcherPurpose::Find, outcome),
            NotesTarget::Today => self.open_today_note(outcome),
            NotesTarget::Open(path) => self.open_note(path, false, outcome),
            NotesTarget::Folder(path) => {
                if let Some(view) = self.notes_view.as_mut() {
                    if !view.collapsed.remove(&path) {
                        view.collapsed.insert(path);
                    }
                }
                outcome.repaint = true;
            }
            NotesTarget::Tag(tag) => {
                if let Some(view) = self.notes_view.as_mut() {
                    view.tag_filter = (view.tag_filter.as_ref() != Some(&tag)).then_some(tag);
                    view.list_scroll = 0;
                }
                outcome.repaint = true;
            }
            NotesTarget::ClearTag => {
                if let Some(view) = self.notes_view.as_mut() {
                    view.tag_filter = None;
                    view.list_scroll = 0;
                }
                outcome.repaint = true;
            }
            NotesTarget::Read => self.stop_editing(outcome),
            NotesTarget::Edit => self.start_editing(None, outcome),
            NotesTarget::ToAgent => self.send_note_to_agent(outcome),
            NotesTarget::Rename => self.open_notes_switcher(SwitcherPurpose::Rename, outcome),
            NotesTarget::Delete => self.delete_open_note(outcome),
            NotesTarget::Line(line) => self.go_to_line(line, outcome),
            NotesTarget::Switcher(index) => {
                let choice = self
                    .notes_view
                    .as_ref()
                    .and_then(|view| view.switcher_choices().into_iter().nth(index));
                if let Some(choice) = choice {
                    self.choose_switcher(choice, outcome);
                }
            }
            NotesTarget::LoadDisk => {
                if let Some(view) = self.notes_view.as_mut() {
                    view.conflict = false;
                    let disk = view
                        .open
                        .as_deref()
                        .and_then(|path| view.note(path))
                        .map(|note| (note.content.clone(), note.fingerprint.clone()));
                    if let Some((content, fingerprint)) = disk {
                        view.base = Some(fingerprint);
                        if let Some(editor) = view.editor.as_mut() {
                            editor.set_text(&content);
                            view.saved_revision = editor.revision();
                        }
                    }
                }
                outcome.repaint = true;
            }
            NotesTarget::KeepMine => {
                if let Some(view) = self.notes_view.as_mut() {
                    view.conflict = false;
                    view.base = view
                        .open
                        .as_deref()
                        .and_then(|path| view.note(path))
                        .map(|note| note.fingerprint.clone());
                }
                self.flush_note(outcome);
            }
            NotesTarget::SwitcherInput | NotesTarget::Nothing => {}
            NotesTarget::Editor | NotesTarget::Doc => {}
        }
    }

    pub(super) fn handle_notes_view_result(
        &mut self,
        generation: u64,
        request: NotesRequest,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let last_open = &self.notes_last_open;
        let Some(view) = self
            .notes_view
            .as_mut()
            .filter(|view| view.generation == generation)
        else {
            if let (NotesRequest::Write { path, .. }, Err(error)) = (&request, &result) {
                // Notes closed before the save came back; the typing is not on disk.
                tracing::warn!(path, error = %error.message, "note save failed after closing");
            }
            return (false, Vec::new());
        };
        let mut open_after: Option<(String, bool)> = None;
        let mut rename_after: Option<String> = None;
        match request {
            NotesRequest::List => {
                view.list_in_flight = false;
                match result {
                    Ok(ResponseResult::NoteList { vault }) => {
                        view.load = NotesLoad::Ready;
                        let fallback = last_open.get(&vault.root).cloned();
                        apply_vault(view, vault);
                        if view.open.is_none() {
                            let first =
                                fallback
                                    .filter(|path| view.note(path).is_some())
                                    .or_else(|| {
                                        view.resolve("scratch").map(str::to_owned).or_else(|| {
                                            view.notes().first().map(|n| n.path.clone())
                                        })
                                    });
                            open_after = first.map(|path| (path, false));
                        }
                    }
                    Ok(_) => {
                        view.load = NotesLoad::Failed("unexpected answer from the server".into())
                    }
                    Err(error) => view.load = NotesLoad::Failed(error.message),
                }
            }
            NotesRequest::Stamp => {
                view.stamp_in_flight = false;
                if let Ok(ResponseResult::NoteStamp { stamp }) = result {
                    if view
                        .vault
                        .as_ref()
                        .is_some_and(|vault| vault.stamp != stamp)
                    {
                        view.list_stale = true;
                    }
                }
            }
            NotesRequest::Write {
                path,
                content,
                revision,
                open,
            } => {
                view.write_in_flight = false;
                match result {
                    Ok(ResponseResult::NoteSaved { note, stamp }) => {
                        rename_after = view.pending_rename.take();
                        let fingerprint = note.fingerprint.clone();
                        match view.note_mut(&path) {
                            Some(existing) => {
                                existing.content = content;
                                existing.fingerprint.clone_from(&fingerprint);
                                existing.modified_ms = note.modified_ms;
                            }
                            None => {
                                if let Some(vault) = view.vault.as_mut() {
                                    let mut note = note;
                                    note.content = content;
                                    vault.notes.push(note);
                                }
                            }
                        }
                        if let Some(vault) = view.vault.as_mut() {
                            // Our own write is not news; another change still shows.
                            if !view.list_stale {
                                vault.stamp = stamp;
                            }
                        }
                        view.rebuild_index();
                        if view.open.as_deref() == Some(path.as_str()) {
                            view.base = Some(fingerprint);
                            if let Some(revision) = revision {
                                view.saved_revision = revision;
                            }
                        }
                        if open {
                            open_after = Some((path, true));
                        }
                    }
                    Ok(_) => view.message = Some("unexpected answer from the server".into()),
                    Err(error) => match error.code.as_deref() {
                        Some("note_conflict") => {
                            view.conflict = true;
                            view.list_stale = true;
                        }
                        Some("note_exists") if open => {
                            // Someone made it meanwhile: open theirs.
                            view.list_stale = true;
                            view.message = Some(format!("{} already exists", title(&path)));
                        }
                        _ => view.message = Some(format!("not saved: {}", error.message)),
                    },
                }
                if view.pending_rename.take().is_some() {
                    view.message = Some("not renamed: the note could not be saved".into());
                }
                if view.dirty() && view.save_due.is_none() && !view.conflict {
                    // Typed during the save: send that too.
                    view.save_due = Some(Instant::now() + AUTOSAVE_DELAY);
                }
            }
            NotesRequest::Rename { from, to } => match result {
                Ok(ResponseResult::NoteList { vault }) => {
                    let editing = view.editor.is_some();
                    if view.open.as_deref() == Some(from.as_str()) {
                        view.open = None;
                        view.editor = None;
                    }
                    apply_vault(view, vault);
                    open_after = Some((to, editing));
                }
                Ok(_) => view.message = Some("unexpected answer from the server".into()),
                Err(error) => view.message = Some(format!("not renamed: {}", error.message)),
            },
            NotesRequest::Attach { note } => match result {
                Ok(ResponseResult::NoteAttached { path }) => {
                    view.message = None;
                    match view.editor.as_mut() {
                        // The note still being edited gets the image where the cursor is,
                        // a space apart from a word right before it.
                        Some(editor) if view.open.as_deref() == Some(note.as_str()) => {
                            let (line, byte) = editor.cursor();
                            let glued = editor.lines().get(line).is_some_and(|text| {
                                text[..byte.min(text.len())]
                                    .chars()
                                    .next_back()
                                    .is_some_and(|ch| !ch.is_whitespace())
                            });
                            let space = if glued { " " } else { "" };
                            editor.insert(&format!("{space}![[{path}]]"));
                            view.save_due = Some(Instant::now() + AUTOSAVE_DELAY);
                            view.reveal_cursor = true;
                        }
                        _ => view.message = Some(format!("image saved as {path}")),
                    }
                }
                Ok(_) => view.message = Some("unexpected answer from the server".into()),
                Err(error) => view.message = Some(format!("image not pasted: {}", error.message)),
            },
            NotesRequest::Delete { path } => match result {
                Ok(ResponseResult::NoteList { vault }) => {
                    let order: Vec<String> = view
                        .list_rows()
                        .into_iter()
                        .filter_map(|row| match row {
                            ListRow::Note { path, .. } => Some(path),
                            ListRow::Folder { .. } => None,
                        })
                        .collect();
                    let at = order.iter().position(|other| *other == path).unwrap_or(0);
                    if view.open.as_deref() == Some(path.as_str()) {
                        view.open = None;
                        view.editor = None;
                    }
                    apply_vault(view, vault);
                    let next = order
                        .iter()
                        .skip(at + 1)
                        .chain(order.iter().take(at).rev())
                        .find(|other| view.note(other).is_some())
                        .cloned();
                    view.message = Some(format!("moved {} to .trash", title(&path)));
                    open_after = next.map(|path| (path, false));
                }
                Ok(_) => view.message = Some("unexpected answer from the server".into()),
                Err(error) => view.message = Some(format!("not deleted: {}", error.message)),
            },
        }
        let mut outcome = ClientShellInput::default();
        if let Some((path, edit)) = open_after {
            self.open_note(path, edit, &mut outcome);
        }
        if let Some(to) = rename_after {
            self.rename_open_note(to, &mut outcome);
        }
        self.load_notes_view(&mut outcome);
        (true, outcome.actions)
    }
}

/// Takes a fresh list from the server. The open note follows its file: a
/// note being edited reloads when nothing was typed, and is marked as in
/// conflict when something was.
fn apply_vault(view: &mut ClientNotesView, vault: NoteVault) {
    view.set_vault(vault);
    let Some(path) = view.open.clone() else {
        return;
    };
    let Some((content, fingerprint)) = view
        .note(&path)
        .map(|note| (note.content.clone(), note.fingerprint.clone()))
    else {
        // Gone from disk: renamed or deleted elsewhere.
        view.open = None;
        view.editor = None;
        view.conflict = false;
        return;
    };
    if view.base.as_ref() == Some(&fingerprint) {
        return;
    }
    let dirty = view.dirty() || view.write_in_flight;
    match view.editor.as_mut() {
        Some(_) if dirty => view.conflict = true,
        Some(editor) => {
            editor.set_text(&content);
            view.saved_revision = editor.revision();
            view.base = Some(fingerprint);
        }
        None => view.base = Some(fingerprint),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NotesTarget {
    Close,
    Retry,
    New,
    Find,
    Today,
    Open(String),
    Folder(String),
    Tag(String),
    ClearTag,
    Read,
    Edit,
    ToAgent,
    Rename,
    Delete,
    /// A heading in the outline, by line.
    Line(usize),
    /// The rendered note: what a click does depends on what is under it.
    Doc,
    /// The editor: a click places the cursor, a drag selects.
    Editor,
    Switcher(usize),
    SwitcherInput,
    /// The conflict bar's choices.
    LoadDisk,
    KeepMine,
    /// Takes a click so nothing under it does.
    Nothing,
}

#[derive(Debug, Default)]
pub(super) struct NotesViewHits {
    pub(super) area: Rect,
    pub(super) list: Rect,
    pub(super) doc: Rect,
    pub(super) editor: Rect,
    pub(super) switcher: Rect,
    pub(super) targets: Vec<(Rect, NotesTarget)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(path: &str, content: &str) -> NoteFile {
        NoteFile {
            path: path.to_owned(),
            content: content.to_owned(),
            fingerprint: format!("fp-{path}"),
            modified_ms: 0,
        }
    }

    fn view(notes: Vec<NoteFile>) -> ClientNotesView {
        let mut view = ClientNotesView::new(1, "boot".into(), "w1".into());
        view.set_vault(NoteVault {
            root: "/notes/herdr".into(),
            project: "herdr".into(),
            stamp: "s1".into(),
            notes,
            truncated: false,
        });
        view
    }

    #[test]
    fn names_resolve_by_title_or_path_and_paths_are_normalized() {
        let view = view(vec![
            note("File tree.md", "[[Decisions]]"),
            note("daily/2026-10-19.md", "#log"),
        ]);
        assert_eq!(view.resolve("file TREE"), Some("File tree.md"));
        assert_eq!(
            view.resolve("daily/2026-10-19"),
            Some("daily/2026-10-19.md")
        );
        assert_eq!(view.resolve("2026-10-19.md"), Some("daily/2026-10-19.md"));
        assert_eq!(view.resolve("missing"), None);
        assert_eq!(
            note_path(" /ideas\\new one.md "),
            Some("ideas/new one.md".into())
        );
        assert_eq!(note_path("  "), None);
    }

    #[test]
    fn list_puts_folders_first_and_sorts_naturally() {
        let mut view = view(vec![
            note("note10.md", ""),
            note("note2.md", ""),
            note("daily/b.md", ""),
            note("daily/a.md", ""),
            note("Alpha.md", ""),
        ]);
        let shown = |view: &ClientNotesView| -> Vec<String> {
            view.list_rows()
                .into_iter()
                .map(|row| match row {
                    ListRow::Folder { name, .. } => format!("{name}/"),
                    ListRow::Note { title, depth, .. } => format!("{}{title}", "  ".repeat(depth)),
                })
                .collect()
        };
        assert_eq!(
            shown(&view),
            vec!["daily/", "  a", "  b", "Alpha", "note2", "note10"]
        );
        view.collapsed.insert("daily".into());
        assert_eq!(shown(&view), vec!["daily/", "Alpha", "note2", "note10"]);
    }

    #[test]
    fn backlinks_tags_and_tag_filter() {
        let mut view = view(vec![
            note("Decisions.md", "see [[File tree]] #board"),
            note("daily/today.md", "[[file tree|the tree]] #log #board"),
            note("File tree.md", "# File tree"),
        ]);
        view.open = Some("File tree.md".into());
        assert_eq!(view.backlinks(), vec!["Decisions.md", "daily/today.md"]);
        assert_eq!(
            view.index.tags.keys().cloned().collect::<Vec<_>>(),
            vec!["board".to_owned(), "log".to_owned()]
        );
        view.tag_filter = Some("log".into());
        assert_eq!(
            view.list_rows(),
            vec![ListRow::Note {
                path: "daily/today.md".into(),
                title: "today".into(),
                depth: 0
            }]
        );
    }

    #[test]
    fn switcher_finds_fuzzily_offers_create_and_filters_by_tag() {
        let mut view = view(vec![
            note("File tree.md", "#ui"),
            note("Decisions.md", ""),
            note("scratch.md", "#ui"),
        ]);
        view.switcher = Some(NotesSwitcher {
            purpose: SwitcherPurpose::Find,
            query: TextEditor::from("ftr"),
            selected: 0,
        });
        assert_eq!(
            view.switcher_choices().first(),
            Some(&SwitcherChoice::Open("File tree.md".into()))
        );
        assert_eq!(
            view.switcher_choices().last(),
            Some(&SwitcherChoice::Create("ftr.md".into()))
        );
        view.switcher = Some(NotesSwitcher {
            purpose: SwitcherPurpose::New,
            query: TextEditor::from("Plan"),
            selected: 0,
        });
        assert_eq!(
            view.switcher_choices().first(),
            Some(&SwitcherChoice::Create("Plan.md".into()))
        );
        view.switcher = Some(NotesSwitcher {
            purpose: SwitcherPurpose::Find,
            query: TextEditor::from("#u"),
            selected: 0,
        });
        assert_eq!(
            view.switcher_choices(),
            vec![
                SwitcherChoice::Open("File tree.md".into()),
                SwitcherChoice::Open("scratch.md".into())
            ]
        );
    }

    #[test]
    fn a_fresh_list_reloads_a_clean_editor_and_flags_a_dirty_one() {
        let mut view = view(vec![note("a.md", "one")]);
        view.open = Some("a.md".into());
        view.base = Some("fp-a.md".into());
        let editor = NoteEditor::new("one");
        view.saved_revision = editor.revision();
        view.editor = Some(editor);
        let changed = |content: &str, fingerprint: &str| NoteVault {
            root: "/notes/herdr".into(),
            project: "herdr".into(),
            stamp: "s2".into(),
            notes: vec![NoteFile {
                path: "a.md".into(),
                content: content.into(),
                fingerprint: fingerprint.into(),
                modified_ms: 1,
            }],
            truncated: false,
        };
        apply_vault(&mut view, changed("two", "fp2"));
        assert_eq!(view.open_text().as_deref(), Some("two"));
        assert!(!view.conflict);
        if let Some(editor) = view.editor.as_mut() {
            editor.insert("!");
        }
        apply_vault(&mut view, changed("three", "fp3"));
        assert!(view.conflict);
        assert_eq!(view.open_text().as_deref(), Some("!two"));
    }
}
