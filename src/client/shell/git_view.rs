//! Git view: the focused space's changes and history, shown in place of its
//! panes like a tab. Every git command runs on the server through the
//! `git.*` methods; this module only keeps what is on screen and asks for
//! what the screen needs next.

use std::time::{Duration, Instant};

use crossterm::event::KeyEventKind;

use super::*;
use crate::api::schema::{
    GitChangeKind, GitCommitFile, GitCommitInfo, GitFileChange, GitFileDiff, GitStatusInfo, Method,
    ResponseResult,
};

/// An open view re-reads the status this often, so an agent's edits show up.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// While a commit, push or pull runs on the server, the status is read this often.
const OPERATION_POLL_INTERVAL: Duration = Duration::from_millis(400);
/// Columns a tab stops at when diff text is expanded for display.
const TAB_WIDTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitViewTab {
    Changes,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitViewFocus {
    List,
    Summary,
    Description,
}

/// What a diff shows: a working-tree file, or a file as one commit changed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GitDiffSource {
    WorkingTree {
        path: String,
        orig_path: Option<String>,
    },
    Commit {
        commit: String,
        path: String,
        orig_path: Option<String>,
    },
}

/// A discard waits for a second press or click on the same target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GitArmed {
    DiscardFile(String),
    /// The files listed when asked; files that appear later are not included.
    DiscardAll(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitBusy {
    Staging,
    Discarding,
    Committing,
    Undoing,
    Pushing,
    Pulling,
}

impl GitBusy {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Staging => "staging…",
            Self::Discarding => "discarding…",
            Self::Committing => "committing…",
            Self::Undoing => "undoing the commit…",
            Self::Pushing => "pushing…",
            Self::Pulling => "pulling…",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitTone {
    Info,
    Warning,
    Success,
    Error,
}

/// A commit, push or pull this view started, running on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitPendingOperation {
    id: u64,
    then: GitOperationThen,
}

/// What finishing a background operation means for the view.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GitOperationThen {
    Commit { message: String, push: bool },
    Push,
    Pull,
}

/// The commit made from this view, which "undo" takes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitUndo {
    pub(super) commit: String,
    pub(super) message: String,
    pub(super) at: Instant,
}

/// The request a pending `git.*` answer belongs to.
#[derive(Debug)]
pub(super) enum GitRequest {
    Status,
    Log,
    Show {
        commit: String,
    },
    Diff(GitDiffSource),
    /// Stage, unstage or discard: answered with the new status.
    Change,
    Commit {
        message: String,
        push: bool,
    },
    Uncommit {
        message: String,
    },
    Push,
    Pull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitDiffLineKind {
    Context,
    Added,
    Removed,
    /// "\ No newline at end of file".
    Marker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GitDiffRow {
    Gap,
    /// `staged` is `None` for a commit's hunks, which cannot be staged.
    Hunk {
        header: String,
        staged: Option<bool>,
    },
    Line {
        kind: GitDiffLineKind,
        old: Option<u32>,
        new: Option<u32>,
        /// Tabs expanded, ready to draw.
        text: String,
        width: usize,
    },
    Note(String),
}

/// A diff flattened into rows once, with its wrapped layout cached per width
/// so drawing only touches the visible rows.
#[derive(Debug, Clone)]
pub(super) struct GitDiffDoc {
    pub(super) source: GitDiffSource,
    pub(super) rows: Vec<GitDiffRow>,
    /// The text width the layout was built for, and each row's first display row
    /// (one extra entry: the total).
    layout: Option<(u16, Vec<usize>)>,
}

impl GitDiffDoc {
    pub(super) fn new(source: GitDiffSource, diff: &GitFileDiff) -> Self {
        let mut rows = Vec::new();
        let commit = matches!(source, GitDiffSource::Commit { .. });
        let sections: [(&[crate::api::schema::GitHunk], Option<bool>); 2] = if commit {
            [(&diff.hunks, None), (&[], None)]
        } else {
            [(&diff.staged_hunks, Some(true)), (&diff.hunks, Some(false))]
        };
        for (hunks, staged) in sections {
            for hunk in hunks {
                if !rows.is_empty() {
                    rows.push(GitDiffRow::Gap);
                }
                rows.push(GitDiffRow::Hunk {
                    header: hunk.header.clone(),
                    staged,
                });
                let (mut old, mut new) = (hunk.old_start, hunk.new_start);
                for line in &hunk.lines {
                    let (kind, rest) = match line.as_bytes().first() {
                        Some(b'+') => (GitDiffLineKind::Added, &line[1..]),
                        Some(b'-') => (GitDiffLineKind::Removed, &line[1..]),
                        Some(b'\\') => (GitDiffLineKind::Marker, line.as_str()),
                        Some(b' ') => (GitDiffLineKind::Context, &line[1..]),
                        _ => (GitDiffLineKind::Context, line.as_str()),
                    };
                    let (old_no, new_no) = match kind {
                        GitDiffLineKind::Added => (None, Some(post_increment(&mut new))),
                        GitDiffLineKind::Removed => (Some(post_increment(&mut old)), None),
                        GitDiffLineKind::Context => (
                            Some(post_increment(&mut old)),
                            Some(post_increment(&mut new)),
                        ),
                        GitDiffLineKind::Marker => (None, None),
                    };
                    let text = expand_tabs(rest);
                    let width = UnicodeWidthStr::width(text.as_str());
                    rows.push(GitDiffRow::Line {
                        kind,
                        old: old_no,
                        new: new_no,
                        text,
                        width,
                    });
                }
            }
        }
        if diff.binary {
            rows.push(GitDiffRow::Note("binary file · not shown".to_owned()));
        } else if rows.is_empty() {
            rows.push(GitDiffRow::Note("no changes to show".to_owned()));
        }
        if diff.truncated {
            rows.push(GitDiffRow::Gap);
            rows.push(GitDiffRow::Note(
                "diff cut short · open the file to see all of it".to_owned(),
            ));
        }
        Self {
            source,
            rows,
            layout: None,
        }
    }

    /// Builds (or reuses) the wrapped layout for `text_width` columns of text.
    pub(super) fn ensure_layout(&mut self, text_width: u16) {
        if self
            .layout
            .as_ref()
            .is_some_and(|(width, _)| *width == text_width)
        {
            return;
        }
        let text_width = usize::from(text_width.max(1));
        let mut starts = Vec::with_capacity(self.rows.len() + 1);
        let mut row = 0usize;
        for item in &self.rows {
            starts.push(row);
            row += match item {
                GitDiffRow::Line { width, .. } => width.div_ceil(text_width).max(1),
                _ => 1,
            };
        }
        starts.push(row);
        self.layout = Some((text_width as u16, starts));
    }

    /// Display rows in total, for the layout last built.
    pub(super) fn total_rows(&self) -> usize {
        self.layout
            .as_ref()
            .and_then(|(_, starts)| starts.last().copied())
            .unwrap_or(self.rows.len())
    }

    /// The row item at display row `display_row`, and how many of its
    /// wrapped rows come before it.
    pub(super) fn locate(&self, display_row: usize) -> Option<(usize, usize)> {
        let (_, starts) = self.layout.as_ref()?;
        let index = starts.partition_point(|start| *start <= display_row);
        let index = index.checked_sub(1)?;
        (index < self.rows.len()).then(|| (index, display_row - starts[index]))
    }
}

fn post_increment(value: &mut u32) -> u32 {
    let current = *value;
    *value = value.saturating_add(1);
    current
}

/// Tabs to spaces (the buffer drops control characters, tabs included).
fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut column = 0usize;
    for ch in text.chars() {
        if ch == '\t' {
            let spaces = TAB_WIDTH - column % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            out.push(ch);
            column += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

#[derive(Debug)]
pub(super) struct ClientGitView {
    /// Bumped when the view moves to another space or server: late answers
    /// for the old one are dropped.
    pub(super) generation: u64,
    pub(super) boot_id: String,
    pub(super) workspace_id: String,
    /// The space's tab in front when the view opened; choosing another tab closes it.
    tab_id: Option<String>,
    /// The view asked to focus its space and waits until then before following focus.
    focus_requested_until: Option<Instant>,
    pub(super) tab: GitViewTab,
    pub(super) focus: GitViewFocus,
    pub(super) status: Option<GitStatusInfo>,
    pub(super) commits: Option<Vec<GitCommitInfo>>,
    pub(super) has_more: bool,
    pub(super) selected_file: Option<String>,
    pub(super) selected_commit: Option<String>,
    pub(super) commit_files: Option<(String, Vec<GitCommitFile>)>,
    pub(super) selected_commit_file: usize,
    pub(super) diff: Option<GitDiffDoc>,
    pub(super) summary: TextEditor,
    pub(super) description: TextEditor,
    pub(super) list_scroll: usize,
    pub(super) files_scroll: usize,
    pub(super) diff_scroll: usize,
    pub(super) body_open: bool,
    /// Scroll the selection into view at the next frame.
    pub(super) reveal_selection: bool,
    pub(super) armed: Option<GitArmed>,
    pub(super) busy: Option<GitBusy>,
    pub(super) message: Option<(String, GitTone)>,
    pub(super) undo: Option<GitUndo>,
    pub(super) operation: Option<GitPendingOperation>,
    /// The server has no `git.*` methods.
    pub(super) unsupported: bool,
    pub(super) error: Option<String>,
    status_stale: bool,
    log_stale: bool,
    diff_stale: bool,
    status_in_flight: bool,
    log_in_flight: bool,
    show_in_flight: Option<String>,
    diff_in_flight: Option<GitDiffSource>,
    next_refresh: Instant,
}

impl ClientGitView {
    fn new(generation: u64, boot_id: String, workspace_id: String, tab: GitViewTab) -> Self {
        Self {
            generation,
            boot_id,
            workspace_id,
            tab_id: None,
            focus_requested_until: None,
            tab,
            focus: GitViewFocus::List,
            status: None,
            commits: None,
            has_more: false,
            selected_file: None,
            selected_commit: None,
            commit_files: None,
            selected_commit_file: 0,
            diff: None,
            summary: TextEditor::default(),
            description: TextEditor::default(),
            list_scroll: 0,
            files_scroll: 0,
            diff_scroll: 0,
            body_open: false,
            reveal_selection: true,
            armed: None,
            busy: None,
            message: None,
            undo: None,
            operation: None,
            unsupported: false,
            error: None,
            status_stale: true,
            log_stale: true,
            diff_stale: false,
            status_in_flight: false,
            log_in_flight: false,
            show_in_flight: None,
            diff_in_flight: None,
            next_refresh: Instant::now() + REFRESH_INTERVAL,
        }
    }

    /// The repository root on screen; changes are refused for any other.
    fn shown_root(&self) -> Option<String> {
        self.status.as_ref().map(|status| status.root.clone())
    }

    pub(super) fn files(&self) -> &[GitFileChange] {
        self.status.as_ref().map_or(&[], |status| &status.files)
    }

    pub(super) fn selected_file_index(&self) -> Option<usize> {
        let path = self.selected_file.as_deref()?;
        self.files().iter().position(|file| file.path == path)
    }

    pub(super) fn selected_file(&self) -> Option<&GitFileChange> {
        self.selected_file_index().map(|index| &self.files()[index])
    }

    pub(super) fn commit_list(&self) -> &[GitCommitInfo] {
        self.commits.as_deref().unwrap_or_default()
    }

    pub(super) fn selected_commit_index(&self) -> Option<usize> {
        let commit = self.selected_commit.as_deref()?;
        self.commit_list()
            .iter()
            .position(|info| info.commit == commit)
    }

    pub(super) fn selected_commit(&self) -> Option<&GitCommitInfo> {
        self.selected_commit_index()
            .map(|index| &self.commit_list()[index])
    }

    /// The selected commit's files, once they have arrived.
    pub(super) fn shown_commit_files(&self) -> Option<&[GitCommitFile]> {
        let (commit, files) = self.commit_files.as_ref()?;
        (Some(commit.as_str()) == self.selected_commit.as_deref()).then_some(files.as_slice())
    }

    pub(super) fn staged_count(&self) -> usize {
        self.files().iter().filter(|file| file.staged).count()
    }

    /// What the diff pane should show right now.
    pub(super) fn wanted_diff(&self) -> Option<GitDiffSource> {
        match self.tab {
            GitViewTab::Changes => self.selected_file().map(|file| GitDiffSource::WorkingTree {
                path: file.path.clone(),
                orig_path: file.orig_path.clone(),
            }),
            GitViewTab::History => {
                let commit = self.selected_commit.clone()?;
                let file = self.shown_commit_files()?.get(self.selected_commit_file)?;
                Some(GitDiffSource::Commit {
                    commit,
                    path: file.path.clone(),
                    orig_path: file.orig_path.clone(),
                })
            }
        }
    }

    /// The diff on screen, if it is the one wanted.
    pub(super) fn shown_diff(&self) -> Option<&GitDiffDoc> {
        let wanted = self.wanted_diff()?;
        self.diff.as_ref().filter(|doc| doc.source == wanted)
    }

    fn set_message(&mut self, text: impl Into<String>, tone: GitTone) {
        self.message = Some((text.into(), tone));
    }

    /// Keeps the file selection on the same path, else the first file.
    fn reconcile_file_selection(&mut self) {
        if self.selected_file_index().is_none() {
            self.selected_file = self.files().first().map(|file| file.path.clone());
            self.reveal_selection = true;
        }
    }

    fn reconcile_commit_selection(&mut self) {
        if self.selected_commit_index().is_none() {
            self.selected_commit = self.commit_list().first().map(|info| info.commit.clone());
            self.selected_commit_file = 0;
            self.body_open = false;
            self.reveal_selection = true;
        }
    }

    /// Puts the result of a finished background operation on screen. Returns
    /// whether a push follows (commit and push).
    fn settle_operation(&mut self) -> bool {
        let Some(pending_id) = self.operation.as_ref().map(|pending| pending.id) else {
            return false;
        };
        let reported = self
            .status
            .as_ref()
            .and_then(|status| status.operation.as_ref())
            .map(|operation| (operation.id, operation.running, operation.error.clone()));
        let error = match reported {
            Some((id, true, _)) if id == pending_id => return false,
            Some((id, false, error)) if id == pending_id => error,
            // The server no longer reports it: it restarted, or another client started one.
            _ => {
                self.operation = None;
                self.busy = None;
                self.status_stale = true;
                self.log_stale = true;
                self.set_message(
                    "lost track of the operation · check History for its result",
                    GitTone::Warning,
                );
                return false;
            }
        };
        let Some(pending) = self.operation.take() else {
            return false;
        };
        self.busy = None;
        if let Some(error) = error {
            // A failed commit keeps its message in the fields, ready to try again.
            self.set_message(error, GitTone::Error);
            return false;
        }
        self.log_stale = true;
        match pending.then {
            GitOperationThen::Commit { message, push } => {
                if let Some(commit) = self.status.as_ref().and_then(|status| status.head.clone()) {
                    self.undo = Some(GitUndo {
                        commit,
                        message,
                        at: Instant::now(),
                    });
                }
                self.summary.clear();
                self.description.clear();
                self.focus = GitViewFocus::List;
                self.set_message("committed", GitTone::Success);
                push
            }
            GitOperationThen::Push => {
                self.undo = None;
                self.set_message("pushed", GitTone::Success);
                false
            }
            GitOperationThen::Pull => {
                self.set_message("pulled", GitTone::Success);
                false
            }
        }
    }

    fn apply_status(&mut self, status: GitStatusInfo) {
        let head_moved = self
            .status
            .as_ref()
            .is_some_and(|previous| previous.head != status.head);
        if head_moved {
            self.log_stale = true;
            // Another tool committed or reset: the commit made here is no longer HEAD.
            if self
                .undo
                .as_ref()
                .is_some_and(|undo| status.head.as_deref() != Some(undo.commit.as_str()))
            {
                self.undo = None;
            }
        }
        self.status = Some(status);
        self.error = None;
        self.reconcile_file_selection();
        // The selected file's diff may have changed with it.
        self.diff_stale = true;
    }
}

impl ClientShellState {
    /// Opens the git view on the focused space, or closes it when open.
    pub(super) fn toggle_git_view(&mut self, outcome: &mut ClientShellInput) {
        if self.git_view.is_some() {
            self.close_git_view(outcome);
        } else {
            self.open_git_view(None, outcome);
        }
    }

    /// Opens the git view, focusing `workspace_id` first when given.
    pub(super) fn open_git_view(
        &mut self,
        workspace_id: Option<String>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let focused = snapshot.focused_workspace_id.clone();
        let target = workspace_id.clone().or_else(|| focused.clone());
        let Some(target) = target else {
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
        let reuse = self
            .git_view
            .as_ref()
            .is_some_and(|view| view.workspace_id == target && view.boot_id == boot_id);
        if !reuse {
            self.git_view_generation = self.git_view_generation.wrapping_add(1);
            self.git_view = Some(ClientGitView::new(
                self.git_view_generation,
                boot_id,
                target,
                GitViewTab::Changes,
            ));
        }
        if let Some(view) = self.git_view.as_mut() {
            if needs_focus {
                view.tab_id = None;
                view.focus_requested_until = Some(Instant::now() + Duration::from_secs(3));
            } else {
                view.tab_id = focused_tab;
            }
        }
        self.mode = self.copy_or_terminal_mode();
        outcome.repaint = true;
        self.load_git_view(outcome);
    }

    pub(super) fn close_git_view(&mut self, outcome: &mut ClientShellInput) {
        if self.git_view.take().is_some() {
            outcome.repaint = true;
        }
    }

    /// Follows the focused space: the view always shows the space in front.
    /// Choosing another of that space's tabs puts its panes back instead.
    pub(super) fn retarget_git_view(&mut self) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            self.git_view = None;
            return;
        };
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        let Some(focused) = snapshot.focused_workspace_id.clone() else {
            self.git_view = None;
            return;
        };
        let same_boot = view.boot_id == snapshot.boot_id;
        if same_boot && view.workspace_id == focused {
            view.focus_requested_until = None;
            match view.tab_id.as_deref() {
                None => view.tab_id = snapshot.focused_tab_id.clone(),
                Some(tab_id) if snapshot.focused_tab_id.as_deref() != Some(tab_id) => {
                    self.git_view = None;
                }
                Some(_) => {}
            }
            return;
        }
        // Still waiting for the focus this view asked for.
        if same_boot
            && view
                .focus_requested_until
                .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        let tab = view.tab;
        let boot_id = snapshot.boot_id.clone();
        let focused_tab = snapshot.focused_tab_id.clone();
        self.git_view_generation = self.git_view_generation.wrapping_add(1);
        let mut view = ClientGitView::new(self.git_view_generation, boot_id, focused, tab);
        view.tab_id = focused_tab;
        // Load on the next tick; snapshots arrive without an outcome to send through.
        view.next_refresh = Instant::now();
        self.git_view = Some(view);
    }

    /// Re-reads the status every [`REFRESH_INTERVAL`] while the view is open,
    /// and sends anything a retarget left waiting.
    pub(crate) fn tick_git_view(&mut self, now: Instant) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        let Some(view) = self.git_view.as_mut() else {
            return outcome;
        };
        // Nobody is looking while the terminal window is in the background, but
        // a running operation is still followed to its end.
        let idle_in_background = self.outer_focused == Some(false) && view.operation.is_none();
        if now < view.next_refresh || view.unsupported || idle_in_background {
            return outcome;
        }
        view.next_refresh = now
            + if view.operation.is_some() {
                OPERATION_POLL_INTERVAL
            } else {
                REFRESH_INTERVAL
            };
        // A local change answers with the status anyway; a background operation is polled.
        if view.busy.is_none() || view.operation.is_some() {
            view.status_stale = true;
        }
        self.load_git_view(&mut outcome);
        outcome
    }

    /// Asks for whatever the screen needs and is not already on its way.
    pub(super) fn load_git_view(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        if view.unsupported {
            return;
        }
        let generation = view.generation;
        let target = |view: &ClientGitView| crate::api::schema::GitTarget {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            root: None,
        };
        let mut requests: Vec<(Method, GitRequest)> = Vec::new();
        let waiting_on_change = view.busy.is_some() && view.operation.is_none();
        if view.status_stale && !view.status_in_flight && !waiting_on_change {
            view.status_stale = false;
            view.status_in_flight = true;
            requests.push((Method::GitStatus(target(view)), GitRequest::Status));
        }
        let wants_log = view.tab == GitViewTab::History || view.commits.is_none();
        if wants_log && view.log_stale && !view.log_in_flight {
            view.log_stale = false;
            view.log_in_flight = true;
            requests.push((
                Method::GitLog(crate::api::schema::GitLogParams {
                    workspace_id: Some(view.workspace_id.clone()),
                    pane_id: None,
                    limit: None,
                }),
                GitRequest::Log,
            ));
        }
        if view.tab == GitViewTab::History && view.show_in_flight.is_none() {
            let needs_files = view.selected_commit.as_ref().filter(|commit| {
                view.commit_files
                    .as_ref()
                    .is_none_or(|(loaded, _)| loaded != *commit)
            });
            if let Some(commit) = needs_files.cloned() {
                view.show_in_flight = Some(commit.clone());
                requests.push((
                    Method::GitShow(crate::api::schema::GitShowParams {
                        workspace_id: Some(view.workspace_id.clone()),
                        pane_id: None,
                        commit: commit.clone(),
                    }),
                    GitRequest::Show { commit },
                ));
            }
        }
        if view.diff_in_flight.is_none() {
            if let Some(wanted) = view.wanted_diff() {
                let have = view.diff.as_ref().is_some_and(|doc| doc.source == wanted);
                // A working-tree diff goes stale with the status; a commit's never does.
                let stale = view.diff_stale && matches!(wanted, GitDiffSource::WorkingTree { .. });
                if !have || stale {
                    view.diff_stale = false;
                    view.diff_in_flight = Some(wanted.clone());
                    let (path, orig_path, commit) = match &wanted {
                        GitDiffSource::WorkingTree { path, orig_path } => {
                            (path.clone(), orig_path.clone(), None)
                        }
                        GitDiffSource::Commit {
                            commit,
                            path,
                            orig_path,
                        } => (path.clone(), orig_path.clone(), Some(commit.clone())),
                    };
                    requests.push((
                        Method::GitDiff(crate::api::schema::GitDiffParams {
                            workspace_id: Some(view.workspace_id.clone()),
                            pane_id: None,
                            path,
                            orig_path,
                            commit,
                        }),
                        GitRequest::Diff(wanted),
                    ));
                }
            }
        }
        for (method, request) in requests {
            if !self.send_git_request(method, generation, request, outcome) {
                return;
            }
        }
    }

    /// Returns false (and marks the view unsupported) when the server cannot take it.
    fn send_git_request(
        &mut self,
        method: Method,
        generation: u64,
        request: GitRequest,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let supported = self.supports_endpoint_method(&method);
        let sent = self.push_endpoint_method_with_kind(
            method,
            PendingEndpointKind::Git {
                generation,
                request,
            },
            outcome,
        );
        if !sent {
            if let Some(view) = self.git_view.as_mut() {
                // A server that is briefly away is retried on the next tick.
                view.unsupported = !supported;
                view.busy = None;
                view.operation = None;
                view.status_in_flight = false;
                view.log_in_flight = false;
                view.show_in_flight = None;
                view.diff_in_flight = None;
            }
            outcome.repaint = true;
        }
        sent
    }

    /// Starts a stage, unstage, discard, commit, undo, push or pull. One at a
    /// time: the server answers each with the new status.
    fn start_git_operation(
        &mut self,
        busy: GitBusy,
        method: Method,
        request: GitRequest,
        outcome: &mut ClientShellInput,
    ) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        if view.busy.is_some() || view.unsupported {
            return;
        }
        view.busy = Some(busy);
        view.armed = None;
        view.message = None;
        let generation = view.generation;
        outcome.repaint = true;
        self.send_git_request(method, generation, request, outcome);
    }

    fn git_paths_params(
        view: &ClientGitView,
        paths: Vec<String>,
        hunk: Option<String>,
    ) -> crate::api::schema::GitPathsParams {
        crate::api::schema::GitPathsParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            root: view.shown_root(),
            paths,
            hunk,
        }
    }

    /// Stages the selected file, or unstages it when it is fully staged.
    pub(super) fn toggle_git_file(&mut self, index: Option<usize>, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_ref() else {
            return;
        };
        let Some(file) = index
            .or_else(|| view.selected_file_index())
            .and_then(|index| view.files().get(index))
        else {
            return;
        };
        let (method, busy) = if file.staged && !file.unstaged {
            // Unstaging a rename needs both of its paths.
            let mut paths = vec![file.path.clone()];
            paths.extend(file.orig_path.clone());
            (
                Method::GitUnstage(Self::git_paths_params(view, paths, None)),
                GitBusy::Staging,
            )
        } else {
            (
                Method::GitStage(Self::git_paths_params(view, vec![file.path.clone()], None)),
                GitBusy::Staging,
            )
        };
        self.start_git_operation(busy, method, GitRequest::Change, outcome);
    }

    /// Stages every change, or unstages everything when all is staged.
    pub(super) fn toggle_git_all(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_ref() else {
            return;
        };
        let files = view.files();
        if files.is_empty() {
            return;
        }
        let all_staged = files.iter().all(|file| file.staged && !file.unstaged);
        let method = if all_staged {
            let paths = files
                .iter()
                .flat_map(|file| std::iter::once(file.path.clone()).chain(file.orig_path.clone()))
                .collect();
            Method::GitUnstage(Self::git_paths_params(view, paths, None))
        } else {
            let paths = files
                .iter()
                .filter(|file| file.unstaged)
                .map(|file| file.path.clone())
                .collect();
            Method::GitStage(Self::git_paths_params(view, paths, None))
        };
        self.start_git_operation(GitBusy::Staging, method, GitRequest::Change, outcome);
    }

    pub(super) fn toggle_git_hunk(
        &mut self,
        header: String,
        staged: bool,
        outcome: &mut ClientShellInput,
    ) {
        let Some(view) = self.git_view.as_ref() else {
            return;
        };
        let Some(GitDiffSource::WorkingTree { path, .. }) =
            view.shown_diff().map(|doc| &doc.source)
        else {
            return;
        };
        let params = Self::git_paths_params(view, vec![path.clone()], Some(header));
        let method = if staged {
            Method::GitUnstage(params)
        } else {
            Method::GitStage(params)
        };
        self.start_git_operation(GitBusy::Staging, method, GitRequest::Change, outcome);
    }

    /// First press arms; a second press on the same file discards it.
    pub(super) fn discard_git_file(
        &mut self,
        index: Option<usize>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        let Some(file) = index
            .or_else(|| view.selected_file_index())
            .and_then(|index| view.files().get(index))
            .cloned()
        else {
            return;
        };
        if !file.unstaged {
            view.set_message("nothing to discard: its changes are staged", GitTone::Info);
            outcome.repaint = true;
            return;
        }
        let armed = GitArmed::DiscardFile(file.path.clone());
        if view.armed.as_ref() != Some(&armed) {
            let what = if file.kind == GitChangeKind::Untracked {
                format!("delete {}", base_name(&file.path))
            } else {
                format!("throw away your changes to {}", base_name(&file.path))
            };
            view.armed = Some(armed);
            view.set_message(
                format!("press d or click ↶ again to {what}"),
                GitTone::Warning,
            );
            outcome.repaint = true;
            return;
        }
        let method = Method::GitDiscard(crate::api::schema::GitDiscardParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            root: view.shown_root(),
            paths: vec![file.path],
        });
        self.start_git_operation(GitBusy::Discarding, method, GitRequest::Change, outcome);
    }

    pub(super) fn discard_git_all(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        let unstaged: Vec<String> = view
            .files()
            .iter()
            .filter(|file| file.unstaged)
            .map(|file| file.path.clone())
            .collect();
        let Some(GitArmed::DiscardAll(asked)) = view.armed.take() else {
            if unstaged.is_empty() {
                return;
            }
            view.set_message(
                format!(
                    "click again to throw away every unstaged change ({} files)",
                    unstaged.len()
                ),
                GitTone::Warning,
            );
            view.armed = Some(GitArmed::DiscardAll(unstaged));
            outcome.repaint = true;
            return;
        };
        // Only what was listed when asked, and still changed now.
        let paths: Vec<String> = asked
            .into_iter()
            .filter(|path| unstaged.contains(path))
            .collect();
        if paths.is_empty() {
            view.message = None;
            outcome.repaint = true;
            return;
        }
        let method = Method::GitDiscard(crate::api::schema::GitDiscardParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            root: view.shown_root(),
            paths,
        });
        self.start_git_operation(GitBusy::Discarding, method, GitRequest::Change, outcome);
    }

    /// Commits the staged files, or every change when none is staged.
    pub(super) fn commit_git_changes(&mut self, push: bool, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        if view.files().is_empty() {
            view.set_message("nothing to commit", GitTone::Info);
            outcome.repaint = true;
            return;
        }
        let summary = view.summary.as_str().trim().to_owned();
        if summary.is_empty() {
            view.focus = GitViewFocus::Summary;
            view.set_message("write a summary first", GitTone::Error);
            outcome.repaint = true;
            return;
        }
        let description = view.description.as_str().trim();
        let message = if description.is_empty() {
            summary
        } else {
            format!("{summary}\n\n{description}")
        };
        let method = Method::GitCommit(crate::api::schema::GitCommitParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            root: view.shown_root(),
            message: message.clone(),
            all: view.staged_count() == 0,
        });
        self.start_git_operation(
            GitBusy::Committing,
            method,
            GitRequest::Commit { message, push },
            outcome,
        );
    }

    pub(super) fn undo_git_commit(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_ref() else {
            return;
        };
        let Some(undo) = view.undo.clone() else {
            return;
        };
        let method = Method::GitUncommit(crate::api::schema::GitUncommitParams {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            root: view.shown_root(),
            commit: undo.commit,
        });
        self.start_git_operation(
            GitBusy::Undoing,
            method,
            GitRequest::Uncommit {
                message: undo.message,
            },
            outcome,
        );
    }

    pub(super) fn push_git_branch(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_ref() else {
            return;
        };
        let target = crate::api::schema::GitTarget {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            root: view.shown_root(),
        };
        self.start_git_operation(
            GitBusy::Pushing,
            Method::GitPush(target),
            GitRequest::Push,
            outcome,
        );
    }

    pub(super) fn pull_git_branch(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_ref() else {
            return;
        };
        let target = crate::api::schema::GitTarget {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
            root: view.shown_root(),
        };
        self.start_git_operation(
            GitBusy::Pulling,
            Method::GitPull(target),
            GitRequest::Pull,
            outcome,
        );
    }

    pub(super) fn set_git_view_tab(&mut self, tab: GitViewTab, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        if view.tab != tab {
            view.tab = tab;
            view.focus = GitViewFocus::List;
            view.armed = None;
            view.message = None;
            view.list_scroll = 0;
            view.diff_scroll = 0;
            view.reveal_selection = true;
        }
        outcome.repaint = true;
        self.load_git_view(outcome);
    }

    /// Moves the list selection (files or commits) by `delta`.
    pub(super) fn move_git_selection(&mut self, delta: isize, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        match view.tab {
            GitViewTab::Changes => {
                let count = view.files().len();
                if count == 0 {
                    return;
                }
                let current = view.selected_file_index().unwrap_or(0) as isize;
                let next = (current + delta).clamp(0, count as isize - 1) as usize;
                view.selected_file = Some(view.files()[next].path.clone());
            }
            GitViewTab::History => {
                let count = view.commit_list().len();
                if count == 0 {
                    return;
                }
                let current = view.selected_commit_index().unwrap_or(0) as isize;
                let next = (current + delta).clamp(0, count as isize - 1) as usize;
                let commit = view.commit_list()[next].commit.clone();
                if view.selected_commit.as_deref() != Some(commit.as_str()) {
                    view.selected_commit = Some(commit);
                    view.selected_commit_file = 0;
                    view.files_scroll = 0;
                    view.body_open = false;
                }
            }
        }
        view.armed = None;
        view.message = None;
        view.diff_scroll = 0;
        view.reveal_selection = true;
        outcome.repaint = true;
        self.load_git_view(outcome);
    }

    pub(super) fn select_git_file(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        let Some(path) = view.files().get(index).map(|file| file.path.clone()) else {
            return;
        };
        if view.selected_file.as_deref() != Some(path.as_str()) {
            view.selected_file = Some(path);
            view.diff_scroll = 0;
            view.armed = None;
        }
        view.focus = GitViewFocus::List;
        outcome.repaint = true;
        self.load_git_view(outcome);
    }

    pub(super) fn select_git_commit(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        let Some(commit) = view
            .commit_list()
            .get(index)
            .map(|info| info.commit.clone())
        else {
            return;
        };
        if view.selected_commit.as_deref() != Some(commit.as_str()) {
            view.selected_commit = Some(commit);
            view.selected_commit_file = 0;
            view.files_scroll = 0;
            view.diff_scroll = 0;
            view.body_open = false;
        }
        outcome.repaint = true;
        self.load_git_view(outcome);
    }

    pub(super) fn select_git_commit_file(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        let count = view.shown_commit_files().map_or(0, <[_]>::len);
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        if view.selected_commit_file != index {
            view.selected_commit_file = index;
            view.diff_scroll = 0;
        }
        view.reveal_selection = true;
        outcome.repaint = true;
        self.load_git_view(outcome);
    }

    /// Scrolls the diff by `rows`; the frame clamps it to the content.
    pub(super) fn scroll_git_diff(&mut self, rows: isize) {
        if let Some(view) = self.git_view.as_mut() {
            view.diff_scroll = view.diff_scroll.saturating_add_signed(rows);
        }
    }

    pub(super) fn refresh_git_view(&mut self, outcome: &mut ClientShellInput) {
        if let Some(view) = self.git_view.as_mut() {
            view.status_stale = true;
            view.log_stale = true;
            view.diff_stale = true;
            view.commit_files = None;
            view.unsupported = false;
            // An answer that never came must not block reading again.
            view.status_in_flight = false;
            view.log_in_flight = false;
            view.show_in_flight = None;
            view.diff_in_flight = None;
            outcome.repaint = true;
        }
        self.load_git_view(outcome);
    }

    /// Keys while the view is in front and no popup or modal takes them.
    pub(super) fn route_git_view_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let Some(view) = self.git_view.as_mut() else {
            return;
        };
        let (mut code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        // Shift+j may come as 'j' with Shift; the list keys use the capital.
        if let KeyCode::Char(ch) = code {
            if modifiers.contains(crossterm::event::KeyModifiers::SHIFT) && ch.is_ascii_lowercase()
            {
                code = KeyCode::Char(ch.to_ascii_uppercase());
            }
        }
        let shift_tab = code == KeyCode::BackTab;
        match view.focus {
            GitViewFocus::Summary | GitViewFocus::Description => {
                let alt = modifiers.contains(crossterm::event::KeyModifiers::ALT);
                match code {
                    KeyCode::Enter => {
                        self.commit_git_changes(alt, outcome);
                        return;
                    }
                    KeyCode::Esc => view.focus = GitViewFocus::List,
                    KeyCode::Tab | KeyCode::Down if !shift_tab => {
                        view.focus = match view.focus {
                            GitViewFocus::Summary => GitViewFocus::Description,
                            _ => GitViewFocus::List,
                        };
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        view.focus = match view.focus {
                            GitViewFocus::Description => GitViewFocus::Summary,
                            _ => GitViewFocus::List,
                        };
                    }
                    _ => {
                        let editor = match view.focus {
                            GitViewFocus::Summary => &mut view.summary,
                            _ => &mut view.description,
                        };
                        if editor.handle_key(key).is_none() {
                            return;
                        }
                        if view
                            .message
                            .as_ref()
                            .is_some_and(|(_, tone)| *tone == GitTone::Error)
                        {
                            view.message = None;
                        }
                    }
                }
                outcome.repaint = true;
                return;
            }
            GitViewFocus::List => {}
        }
        if !modifiers
            .difference(crossterm::event::KeyModifiers::SHIFT)
            .is_empty()
            && !matches!(code, KeyCode::Char('d' | 'u') if modifiers == crossterm::event::KeyModifiers::CONTROL)
        {
            return;
        }
        let control = modifiers == crossterm::event::KeyModifiers::CONTROL;
        let history = view.tab == GitViewTab::History;
        match code {
            KeyCode::Esc => {
                if view.armed.take().is_some() {
                    view.message = None;
                    outcome.repaint = true;
                } else {
                    self.close_git_view(outcome);
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let tab = if history {
                    GitViewTab::Changes
                } else {
                    GitViewTab::History
                };
                self.set_git_view_tab(tab, outcome);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_git_selection(-1, outcome),
            KeyCode::Down | KeyCode::Char('j') => self.move_git_selection(1, outcome),
            KeyCode::Home => self.move_git_selection(isize::MIN / 2, outcome),
            KeyCode::End => self.move_git_selection(isize::MAX / 2, outcome),
            KeyCode::PageDown | KeyCode::Char('d') if code == KeyCode::PageDown || control => {
                let page = self.hits.git_view.diff.height.max(2) as isize;
                self.scroll_git_diff(if control { page / 2 } else { page - 1 });
                outcome.repaint = true;
            }
            KeyCode::PageUp | KeyCode::Char('u') if code == KeyCode::PageUp || control => {
                let page = self.hits.git_view.diff.height.max(2) as isize;
                self.scroll_git_diff(-(if control { page / 2 } else { page - 1 }));
                outcome.repaint = true;
            }
            KeyCode::Char('J') => {
                self.scroll_git_diff(3);
                outcome.repaint = true;
            }
            KeyCode::Char('K') => {
                self.scroll_git_diff(-3);
                outcome.repaint = true;
            }
            KeyCode::Char(']') if history => {
                let next = view.selected_commit_file.saturating_add(1);
                self.select_git_commit_file(next, outcome);
            }
            KeyCode::Char('[') if history => {
                let previous = view.selected_commit_file.saturating_sub(1);
                self.select_git_commit_file(previous, outcome);
            }
            KeyCode::Char(' ' | 's') if !history => self.toggle_git_file(None, outcome),
            KeyCode::Char('a') if !history => self.toggle_git_all(outcome),
            KeyCode::Char('d') if !history => self.discard_git_file(None, outcome),
            KeyCode::Char('c') | KeyCode::Enter if !history => {
                view.focus = GitViewFocus::Summary;
                view.armed = None;
                outcome.repaint = true;
            }
            KeyCode::Char('u') if view.undo.is_some() => self.undo_git_commit(outcome),
            KeyCode::Char('p') => self.push_git_branch(outcome),
            KeyCode::Char('l') => self.pull_git_branch(outcome),
            KeyCode::Char('r') => self.refresh_git_view(outcome),
            _ => {}
        }
    }

    /// Typed text: into the focused commit field, else a lone character is
    /// that key (a host reporting every key sends plain keys as text).
    pub(super) fn route_git_view_text(&mut self, text: &str, outcome: &mut ClientShellInput) {
        if self.git_view_text_field_focused() {
            outcome.repaint |= self.insert_git_view_text(text);
            return;
        }
        let mut chars = text.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            let key = crate::input::TerminalKey::new(
                KeyCode::Char(ch),
                crossterm::event::KeyModifiers::empty(),
            );
            self.route_git_view_key(&key, outcome);
        }
    }

    /// Pasted text goes to the focused commit field.
    pub(super) fn insert_git_view_text(&mut self, text: &str) -> bool {
        let Some(view) = self.git_view.as_mut() else {
            return false;
        };
        match view.focus {
            GitViewFocus::Summary => view.summary.insert(text),
            GitViewFocus::Description => view.description.insert(text),
            GitViewFocus::List => false,
        }
    }

    pub(super) fn git_view_text_field_focused(&self) -> bool {
        self.git_view
            .as_ref()
            .is_some_and(|view| view.focus != GitViewFocus::List)
    }

    /// Mouse events over the view. Returns false for events elsewhere (the
    /// sidebar and tab bar keep working; a tab click closes the view).
    pub(super) fn route_git_view_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (mouse.column, mouse.row);
        let hits = &self.hits.git_view;
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && (super::contains(self.hits.new_tab, point)
                || self
                    .hits
                    .tabs
                    .iter()
                    .any(|(rect, _)| super::contains(*rect, point)))
        {
            // A real tab brings its panes back.
            self.git_view = None;
            outcome.repaint = true;
            return false;
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
                let (list, files, diff) = (hits.list, hits.files, hits.diff);
                if let Some(view) = self.git_view.as_mut() {
                    if super::contains(list, point) {
                        view.list_scroll = view.list_scroll.saturating_add_signed(delta);
                    } else if super::contains(files, point) {
                        view.files_scroll = view.files_scroll.saturating_add_signed(delta);
                    } else if super::contains(diff, point) {
                        view.diff_scroll = view.diff_scroll.saturating_add_signed(delta);
                    }
                    outcome.repaint = true;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let target = hits
                    .targets
                    .iter()
                    .rev()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, target)| target.clone());
                if let Some(target) = target {
                    self.activate_git_target(target, outcome);
                } else if let Some(view) = self.git_view.as_mut() {
                    // A click on empty space leaves the commit fields.
                    if view.focus != GitViewFocus::List {
                        view.focus = GitViewFocus::List;
                        outcome.repaint = true;
                    }
                }
            }
            _ => {}
        }
        true
    }

    fn activate_git_target(&mut self, target: GitViewTarget, outcome: &mut ClientShellInput) {
        let keep_armed = matches!(
            target,
            GitViewTarget::DiscardFile(_) | GitViewTarget::DiscardAll
        );
        if let Some(view) = self.git_view.as_mut() {
            if !keep_armed && view.armed.take().is_some() {
                view.message = None;
            }
        }
        match target {
            GitViewTarget::Close => self.close_git_view(outcome),
            GitViewTarget::Pull => self.pull_git_branch(outcome),
            GitViewTarget::Push => self.push_git_branch(outcome),
            GitViewTarget::Tab(tab) => self.set_git_view_tab(tab, outcome),
            GitViewTarget::StageAll => self.toggle_git_all(outcome),
            GitViewTarget::DiscardAll => self.discard_git_all(outcome),
            GitViewTarget::File(index) => self.select_git_file(index, outcome),
            GitViewTarget::FileBox(index) => self.toggle_git_file(Some(index), outcome),
            GitViewTarget::DiscardFile(index) => self.discard_git_file(Some(index), outcome),
            GitViewTarget::Hunk { header, staged } => self.toggle_git_hunk(header, staged, outcome),
            GitViewTarget::Field(focus) => {
                if let Some(view) = self.git_view.as_mut() {
                    view.focus = focus;
                    outcome.repaint = true;
                }
            }
            GitViewTarget::CommitButton => self.commit_git_changes(false, outcome),
            GitViewTarget::Undo => self.undo_git_commit(outcome),
            GitViewTarget::Commit(index) => self.select_git_commit(index, outcome),
            GitViewTarget::CommitFile(index) => self.select_git_commit_file(index, outcome),
            GitViewTarget::BodyToggle => {
                if let Some(view) = self.git_view.as_mut() {
                    view.body_open = !view.body_open;
                    outcome.repaint = true;
                }
            }
        }
    }

    /// A `git.*` answer. Returns whether to repaint and any follow-up requests.
    pub(super) fn handle_git_view_result(
        &mut self,
        generation: u64,
        request: GitRequest,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let current = self
            .git_view
            .as_ref()
            .is_some_and(|view| view.generation == generation);
        if !current {
            // The view closed or moved on. An operation's failure must still be seen.
            let failed = match (&request, &result) {
                (
                    GitRequest::Change
                    | GitRequest::Commit { .. }
                    | GitRequest::Uncommit { .. }
                    | GitRequest::Push
                    | GitRequest::Pull,
                    Err(error),
                ) => Some(error.message.clone()),
                _ => None,
            };
            return match failed {
                Some(message) => (
                    self.push_endpoint_notice(
                        ClientEndpointNoticeKind::Rejected,
                        "git",
                        "Git failed",
                        message,
                    ),
                    Vec::new(),
                ),
                None => (false, Vec::new()),
            };
        }
        let mut outcome = ClientShellInput::default();
        let mut push_after = false;
        if let Some(view) = self.git_view.as_mut() {
            match request {
                GitRequest::Status => {
                    view.status_in_flight = false;
                    match result {
                        Ok(ResponseResult::GitStatus { status }) => view.apply_status(status),
                        Ok(_) => view.error = Some("unexpected answer from the server".to_owned()),
                        Err(error) => view.error = Some(error.message),
                    }
                }
                GitRequest::Log => {
                    view.log_in_flight = false;
                    match result {
                        Ok(ResponseResult::GitLog { commits, has_more }) => {
                            // A selection on the newest commit follows new commits.
                            let followed_head =
                                view.selected_commit_index().is_none_or(|index| index == 0);
                            view.commits = Some(commits);
                            view.has_more = has_more;
                            if followed_head {
                                view.selected_commit = None;
                            }
                            view.reconcile_commit_selection();
                        }
                        Ok(_) => view.error = Some("unexpected answer from the server".to_owned()),
                        Err(error) => {
                            // Keep what is shown; an empty history is still a history.
                            view.commits.get_or_insert_with(Vec::new);
                            view.set_message(error.message, GitTone::Error);
                        }
                    }
                }
                GitRequest::Show { commit } => {
                    view.show_in_flight = None;
                    match result {
                        Ok(ResponseResult::GitCommitFiles { files, .. }) => {
                            view.commit_files = Some((commit, files));
                        }
                        Ok(_) | Err(_) => {
                            view.commit_files = Some((commit, Vec::new()));
                        }
                    }
                }
                GitRequest::Diff(source) => {
                    view.diff_in_flight = None;
                    let same_as_shown = view.diff.as_ref().is_some_and(|doc| doc.source == source);
                    match result {
                        Ok(ResponseResult::GitDiff { diff }) => {
                            if !same_as_shown {
                                view.diff_scroll = 0;
                            }
                            view.diff = Some(GitDiffDoc::new(source, &diff));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let mut doc = GitDiffDoc::new(source, &GitFileDiff::default());
                            doc.rows = vec![GitDiffRow::Note(error.message)];
                            view.diff = Some(doc);
                        }
                    }
                }
                GitRequest::Commit { .. } | GitRequest::Push | GitRequest::Pull => {
                    // The server started a background operation; the status polls follow it.
                    match result {
                        Ok(ResponseResult::GitStatus { status }) => {
                            let then = match request {
                                GitRequest::Commit { message, push } => {
                                    GitOperationThen::Commit { message, push }
                                }
                                GitRequest::Push => GitOperationThen::Push,
                                _ => GitOperationThen::Pull,
                            };
                            match status.operation.as_ref() {
                                Some(operation) => {
                                    view.operation = Some(GitPendingOperation {
                                        id: operation.id,
                                        then,
                                    });
                                    view.next_refresh = Instant::now() + OPERATION_POLL_INTERVAL;
                                }
                                None => {
                                    view.busy = None;
                                    view.set_message(
                                        "the server did not report the operation",
                                        GitTone::Error,
                                    );
                                }
                            }
                            view.apply_status(status);
                        }
                        Ok(_) => {
                            view.busy = None;
                            view.set_message("unexpected answer from the server", GitTone::Error);
                        }
                        Err(error) => {
                            view.busy = None;
                            view.status_stale = true;
                            view.set_message(error.message, GitTone::Error);
                        }
                    }
                }
                GitRequest::Change | GitRequest::Uncommit { .. } => {
                    let busy = view.busy.take();
                    match result {
                        Ok(ResponseResult::GitStatus { status }) => {
                            if let GitRequest::Uncommit { message } = request {
                                // Back in the fields, ready to commit again.
                                let (summary, description) = message
                                    .split_once("\n\n")
                                    .map_or((message.as_str(), ""), |(s, d)| (s, d));
                                view.summary = TextEditor::from(summary);
                                view.description = TextEditor::from(description);
                                view.undo = None;
                                view.log_stale = true;
                                view.set_message(
                                    "undid the commit · its changes are staged",
                                    GitTone::Success,
                                );
                            } else if busy == Some(GitBusy::Discarding) {
                                view.set_message("discarded", GitTone::Success);
                            }
                            view.apply_status(status);
                        }
                        Ok(_) => {
                            view.set_message("unexpected answer from the server", GitTone::Error)
                        }
                        Err(error) => {
                            if error.code.as_deref() == Some("hunk_not_found") {
                                view.diff_stale = true;
                            }
                            view.status_stale = true;
                            view.set_message(error.message, GitTone::Error);
                        }
                    }
                }
            }
            push_after = view.settle_operation();
        }
        if push_after {
            self.push_git_branch(&mut outcome);
        }
        self.load_git_view(&mut outcome);
        (true, outcome.actions)
    }
}

/// Click targets inside the view, from the last frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GitViewTarget {
    Close,
    Pull,
    Push,
    Tab(GitViewTab),
    StageAll,
    DiscardAll,
    File(usize),
    FileBox(usize),
    DiscardFile(usize),
    Hunk { header: String, staged: bool },
    Field(GitViewFocus),
    CommitButton,
    Undo,
    Commit(usize),
    CommitFile(usize),
    BodyToggle,
}

#[derive(Debug, Default)]
pub(super) struct GitViewHits {
    pub(super) area: Rect,
    pub(super) list: Rect,
    pub(super) files: Rect,
    pub(super) diff: Rect,
    pub(super) targets: Vec<(Rect, GitViewTarget)>,
}

pub(super) fn base_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::GitHunk;

    fn hunk(header: &str, old: u32, new: u32, lines: &[&str]) -> GitHunk {
        GitHunk {
            header: header.into(),
            old_start: old,
            new_start: new,
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
        }
    }

    #[test]
    fn diff_rows_number_lines_and_mark_staged_hunks() {
        let diff = GitFileDiff {
            path: "a.rs".into(),
            staged_hunks: vec![hunk("@@ -1 +1 @@", 1, 1, &["-old", "+new"])],
            hunks: vec![hunk(
                "@@ -9,2 +9,2 @@",
                9,
                9,
                &[" ctx", "+\tadd", "\\ No newline at end of file"],
            )],
            ..GitFileDiff::default()
        };
        let source = GitDiffSource::WorkingTree {
            path: "a.rs".into(),
            orig_path: None,
        };
        let doc = GitDiffDoc::new(source, &diff);
        assert!(matches!(
            &doc.rows[0],
            GitDiffRow::Hunk {
                staged: Some(true),
                ..
            }
        ));
        assert!(matches!(
            &doc.rows[1],
            GitDiffRow::Line {
                kind: GitDiffLineKind::Removed,
                old: Some(1),
                new: None,
                ..
            }
        ));
        assert!(matches!(&doc.rows[3], GitDiffRow::Gap));
        assert!(matches!(
            &doc.rows[4],
            GitDiffRow::Hunk {
                staged: Some(false),
                ..
            }
        ));
        assert!(matches!(
            &doc.rows[5],
            GitDiffRow::Line {
                kind: GitDiffLineKind::Context,
                old: Some(9),
                new: Some(9),
                ..
            }
        ));
        let GitDiffRow::Line { text, new, .. } = &doc.rows[6] else {
            panic!("added line");
        };
        assert_eq!(
            (text.as_str(), *new),
            ("    add", Some(10)),
            "tabs expanded"
        );
        assert!(matches!(
            &doc.rows[7],
            GitDiffRow::Line {
                kind: GitDiffLineKind::Marker,
                ..
            }
        ));
    }

    #[test]
    fn wrapped_layout_locates_display_rows() {
        let diff = GitFileDiff {
            path: "a.rs".into(),
            hunks: vec![hunk("@@ -1 +1 @@", 1, 1, &["+0123456789", "+ab"])],
            ..GitFileDiff::default()
        };
        let source = GitDiffSource::Commit {
            commit: "abcd".into(),
            path: "a.rs".into(),
            orig_path: None,
        };
        let mut doc = GitDiffDoc::new(source, &diff);
        doc.ensure_layout(4);
        // header, 10 chars in 3 rows of 4, then 2 chars in 1.
        assert_eq!(doc.total_rows(), 1 + 3 + 1);
        assert_eq!(doc.locate(0), Some((0, 0)));
        assert_eq!(doc.locate(2), Some((1, 1)));
        assert_eq!(doc.locate(4), Some((2, 0)));
        assert_eq!(doc.locate(5), None);
        doc.ensure_layout(20);
        assert_eq!(doc.total_rows(), 3);
    }

    #[test]
    fn tabs_expand_to_the_next_stop() {
        assert_eq!(expand_tabs("a\tb"), "a   b");
        assert_eq!(expand_tabs("\t\tx"), "        x");
        assert_eq!(expand_tabs("abcd\te"), "abcd    e");
        assert_eq!(base_name("src/a/b.rs"), "b.rs");
        assert_eq!(base_name("top.rs"), "top.rs");
    }
}
