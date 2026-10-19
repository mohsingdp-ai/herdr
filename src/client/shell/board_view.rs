//! Board: the focused space's Backlog.md tasks, shown in place of its panes
//! like a tab. Every read and write goes through the server's `task.*`
//! methods; this module only keeps what is on screen and asks for what the
//! screen needs next. Nothing here runs while no Board is open.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::KeyEventKind;

use super::*;
use crate::api::schema::{Method, ResponseResult, TaskBoardInfo, TaskDetail, TaskSummary};

/// While a Board is on screen, it asks this often whether its tasks changed.
const STAMP_INTERVAL: Duration = Duration::from_secs(2);

/// Priority filter choices after "all".
pub(super) const PRIORITIES: [&str; 3] = ["high", "medium", "low"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoardMode {
    Kanban,
    List,
}

/// What the Board can show for its space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BoardLoad {
    Loading,
    Ready,
    NotInstalled,
    NotInitialized,
    /// The server has no `task.*` methods.
    Unsupported,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoardSortKey {
    Id,
    Title,
    Priority,
    Agent,
    Deps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoardFilter {
    Agent,
    Priority,
    Status,
}

#[derive(Debug)]
pub(super) enum BoardRequest {
    List,
    /// Whether the tasks changed since the list on screen.
    Stamp,
    Get {
        id: String,
    },
    Init,
    /// A drag already shows the task in its new column; failing puts it back in `from`.
    Move {
        id: String,
        from: String,
    },
    /// The draft's title comes back to the input if the server refuses it.
    Create {
        status: String,
        title: String,
    },
    /// An edit already shown; failing puts `before` back.
    Edit {
        id: String,
        before: Box<TaskSnapshot>,
    },
}

/// A task as shown before an edit, to put back if the save fails.
#[derive(Debug)]
pub(super) struct TaskSnapshot {
    summary: TaskSummary,
    detail: Option<TaskDetail>,
}

/// A details field edited as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoardField {
    Title,
    Description,
    Assignee,
    Type,
    Comment,
}

#[derive(Debug)]
pub(super) struct BoardEditing {
    pub(super) id: String,
    pub(super) field: BoardField,
    pub(super) text: TextEditor,
}

/// A card or row held by the mouse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BoardDrag {
    pub(super) id: String,
    pub(super) start: (u16, u16),
    pub(super) at: (u16, u16),
    /// Moved since the press: a release drops instead of clicking.
    pub(super) active: bool,
    /// The status under the pointer.
    pub(super) over: Option<String>,
}

/// A new task being named, in the column or group it will start in.
#[derive(Debug)]
pub(super) struct BoardDraft {
    pub(super) status: String,
    pub(super) title: TextEditor,
}

#[derive(Debug)]
pub(super) struct ClientBoardView {
    /// Bumped when the view moves to another space or server: late answers
    /// for the old one are dropped.
    pub(super) generation: u64,
    pub(super) boot_id: String,
    pub(super) workspace_id: String,
    /// The space's tab in front when the Board opened; choosing another tab closes it.
    tab_id: Option<String>,
    /// The Board asked to focus its space and waits until then before following focus.
    focus_requested_until: Option<Instant>,
    pub(super) load: BoardLoad,
    pub(super) board: Option<TaskBoardInfo>,
    pub(super) mode: BoardMode,
    /// The project whose remembered view `mode` holds.
    mode_root: Option<String>,
    pub(super) selected: Option<String>,
    pub(super) detail: Option<TaskDetail>,
    pub(super) details_open: bool,
    pub(super) search: TextEditor,
    pub(super) search_focused: bool,
    /// Index into each filter's choices; 0 is "all".
    pub(super) agent_filter: usize,
    pub(super) priority_filter: usize,
    pub(super) status_filter: usize,
    pub(super) sort: BoardSortKey,
    pub(super) sort_ascending: bool,
    pub(super) collapsed: HashSet<String>,
    /// Parents whose subtasks the list hides.
    pub(super) folded_tasks: HashSet<String>,
    pub(super) list_scroll: usize,
    pub(super) details_scroll: usize,
    /// Scroll the selection into view at the next frame.
    pub(super) reveal_selection: bool,
    pub(super) init_busy: bool,
    pub(super) message: Option<String>,
    /// Cards scrolled past at the top of each kanban column.
    pub(super) column_scroll: HashMap<String, usize>,
    pub(super) drag: Option<BoardDrag>,
    /// Moves on their way to the server, by task: shown already, kept over older lists.
    pub(super) pending_moves: HashMap<String, String>,
    pub(super) draft: Option<BoardDraft>,
    /// A details field being typed into.
    pub(super) editing: Option<BoardEditing>,
    /// Tasks with an edit on its way; the next waits for its new fingerprint.
    pub(super) saving: HashSet<String>,
    /// The list should be read again.
    list_stale: bool,
    stamp_in_flight: bool,
    next_stamp_check: Instant,
    list_in_flight: bool,
    get_in_flight: Option<String>,
}

/// One row of the grouped list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BoardRow<'a> {
    Group {
        status: &'a str,
        count: usize,
        folded: bool,
    },
    /// A task with no parent on the board.
    Task(&'a TaskSummary),
    /// A subtask, listed under its parent whatever its own status.
    Subtask { task: &'a TaskSummary, last: bool },
    /// The new task being named in this group.
    Draft,
    /// A group with no tasks.
    Empty,
    /// Space between groups.
    Gap,
}

impl ClientBoardView {
    fn new(generation: u64, boot_id: String, workspace_id: String) -> Self {
        Self {
            generation,
            boot_id,
            workspace_id,
            tab_id: None,
            focus_requested_until: None,
            load: BoardLoad::Loading,
            board: None,
            mode: BoardMode::List,
            mode_root: None,
            selected: None,
            detail: None,
            details_open: false,
            search: TextEditor::default(),
            search_focused: false,
            agent_filter: 0,
            priority_filter: 0,
            status_filter: 0,
            sort: BoardSortKey::Id,
            sort_ascending: true,
            collapsed: HashSet::new(),
            folded_tasks: HashSet::new(),
            list_scroll: 0,
            details_scroll: 0,
            reveal_selection: false,
            init_busy: false,
            message: None,
            column_scroll: HashMap::new(),
            drag: None,
            pending_moves: HashMap::new(),
            draft: None,
            editing: None,
            saving: HashSet::new(),
            list_stale: true,
            stamp_in_flight: false,
            next_stamp_check: Instant::now() + STAMP_INTERVAL,
            list_in_flight: false,
            get_in_flight: None,
        }
    }

    pub(super) fn tasks(&self) -> &[TaskSummary] {
        self.board.as_ref().map_or(&[], |board| &board.tasks)
    }

    pub(super) fn statuses(&self) -> &[String] {
        self.board.as_ref().map_or(&[], |board| &board.statuses)
    }

    pub(super) fn task(&self, id: &str) -> Option<&TaskSummary> {
        self.tasks().iter().find(|task| task.id == id)
    }

    pub(super) fn selected_task(&self) -> Option<&TaskSummary> {
        self.selected.as_deref().and_then(|id| self.task(id))
    }

    /// Tasks not in the last column: the count on the Board's tab.
    pub(super) fn open_count(&self) -> usize {
        let done = self.statuses().last();
        self.tasks()
            .iter()
            .filter(|task| Some(&task.status) != done)
            .count()
    }

    /// The agent filter's choices: "all", each assignee, then "unassigned".
    pub(super) fn agent_choices(&self) -> Vec<String> {
        let mut agents: Vec<String> = self
            .tasks()
            .iter()
            .flat_map(|task| task.assignees.iter().cloned())
            .collect();
        agents.sort();
        agents.dedup();
        let mut choices = vec!["all".to_owned()];
        choices.extend(agents);
        choices.push("unassigned".to_owned());
        choices
    }

    pub(super) fn filter_label(&self, filter: BoardFilter) -> String {
        match filter {
            BoardFilter::Agent => self
                .agent_choices()
                .get(self.agent_filter)
                .cloned()
                .unwrap_or_else(|| "all".to_owned()),
            BoardFilter::Priority => self
                .priority_filter
                .checked_sub(1)
                .and_then(|index| PRIORITIES.get(index))
                .map_or("all", |priority| priority)
                .to_owned(),
            BoardFilter::Status => self.status_choice().unwrap_or("all").to_owned(),
        }
    }

    fn status_choice(&self) -> Option<&str> {
        self.status_filter
            .checked_sub(1)
            .and_then(|index| self.statuses().get(index))
            .map(String::as_str)
    }

    /// The statuses shown: every column, or just the filtered one.
    pub(super) fn shown_statuses(&self) -> Vec<&str> {
        match self.status_choice() {
            Some(status) => vec![status],
            None => self.statuses().iter().map(String::as_str).collect(),
        }
    }

    /// The subtasks of `id`, in ID order.
    pub(super) fn subtasks(&self, id: &str) -> Vec<&TaskSummary> {
        let mut children: Vec<&TaskSummary> = self
            .tasks()
            .iter()
            .filter(|task| task.parent_id.as_deref() == Some(id))
            .collect();
        children.sort_by(|a, b| id_order(&a.id, &b.id));
        children
    }

    fn matches(&self, task: &TaskSummary, query: &str, agent: Option<&str>) -> bool {
        self.matches_fields(task, query, agent)
            && self
                .status_choice()
                .is_none_or(|status| task.status == status)
    }

    fn matches_fields(&self, task: &TaskSummary, query: &str, agent: Option<&str>) -> bool {
        if !query.is_empty() {
            let haystack = format!("{} {} {}", task.id, task.title, task.labels.join(" "));
            if !haystack.to_lowercase().contains(query) {
                return false;
            }
        }
        let agent_ok = match agent {
            None | Some("all") => true,
            Some("unassigned") => task.assignees.is_empty(),
            Some(agent) => task.assignees.iter().any(|name| name == agent),
        };
        let priority_ok = self
            .priority_filter
            .checked_sub(1)
            .and_then(|index| PRIORITIES.get(index))
            .is_none_or(|priority| task.priority.as_deref() == Some(*priority));
        agent_ok && priority_ok
    }

    /// The visible tasks of one status, in the list's sort order.
    pub(super) fn column(&self, status: &str) -> Vec<&TaskSummary> {
        let query = self.search.as_str().trim().to_lowercase();
        let agent = self.agent_choices().get(self.agent_filter).cloned();
        let ids: HashSet<&str> = self.tasks().iter().map(|task| task.id.as_str()).collect();
        // Subtasks show inside their parent; one whose parent is gone stands alone.
        let top_level = |task: &&TaskSummary| {
            task.parent_id
                .as_deref()
                .is_none_or(|parent| !ids.contains(parent))
        };
        let mut tasks: Vec<&TaskSummary> = self
            .tasks()
            .iter()
            .filter(top_level)
            .filter(|task| {
                task.status == status
                    && (self.matches(task, &query, agent.as_deref())
                        // A matching subtask keeps its parent on screen; the
                        // status filter is about the parent's column.
                        || self.subtasks(&task.id).into_iter().any(|child| {
                            self.matches_fields(child, &query, agent.as_deref())
                        }))
            })
            .collect();
        tasks.sort_by(|a, b| {
            let order = match self.sort {
                BoardSortKey::Id => id_order(&a.id, &b.id),
                BoardSortKey::Title => a.title.to_lowercase().cmp(&b.title.to_lowercase()),
                BoardSortKey::Priority => {
                    priority_rank(a.priority.as_deref()).cmp(&priority_rank(b.priority.as_deref()))
                }
                // Unassigned sorts last, as "~" does after names.
                BoardSortKey::Agent => a
                    .assignees
                    .first()
                    .map_or("~", String::as_str)
                    .cmp(b.assignees.first().map_or("~", String::as_str)),
                BoardSortKey::Deps => a.dependencies.len().cmp(&b.dependencies.len()),
            };
            if self.sort_ascending {
                order
            } else {
                order.reverse()
            }
        });
        tasks
    }

    /// The grouped list: one block per shown status, folded ones as just their header.
    pub(super) fn list_rows(&self) -> Vec<BoardRow<'_>> {
        let mut rows = Vec::new();
        for status in self.shown_statuses() {
            let tasks = self.column(status);
            let folded = self.collapsed.contains(status);
            if !rows.is_empty() {
                rows.push(BoardRow::Gap);
            }
            rows.push(BoardRow::Group {
                status,
                count: tasks.len(),
                folded,
            });
            if folded {
                continue;
            }
            let drafting = self
                .draft
                .as_ref()
                .is_some_and(|draft| draft.status == status);
            if drafting {
                rows.push(BoardRow::Draft);
            } else if tasks.is_empty() {
                rows.push(BoardRow::Empty);
            }
            for task in tasks {
                rows.push(BoardRow::Task(task));
                if self.folded_tasks.contains(&task.id) {
                    continue;
                }
                let children = self.subtasks(&task.id);
                let count = children.len();
                rows.extend(children.into_iter().enumerate().map(|(index, child)| {
                    BoardRow::Subtask {
                        task: child,
                        last: index + 1 == count,
                    }
                }));
            }
        }
        rows
    }

    /// Task IDs in the order the list shows them, for the arrow keys.
    fn list_order(&self) -> Vec<String> {
        self.list_rows()
            .into_iter()
            .filter_map(|row| match row {
                BoardRow::Task(task) | BoardRow::Subtask { task, .. } => Some(task.id.clone()),
                _ => None,
            })
            .collect()
    }

    /// Takes in a task the server just saved: its card, and its details when shown.
    fn apply_saved(&mut self, task: TaskDetail) {
        if let Some(board) = self.board.as_mut() {
            match board
                .tasks
                .iter_mut()
                .find(|other| other.id == task.summary.id)
            {
                Some(summary) => *summary = task.summary.clone(),
                None => board.tasks.push(task.summary.clone()),
            }
        }
        self.detail = Some(task);
    }

    /// A move or edit of `id` is on its way.
    pub(super) fn is_busy(&self, id: &str) -> bool {
        self.pending_moves.contains_key(id) || self.saving.contains(id)
    }

    /// Puts a refused edit back and says why.
    fn restore(&mut self, before: TaskSnapshot, reason: String) {
        let id = before.summary.id.clone();
        if let Some(summary) = self
            .board
            .as_mut()
            .and_then(|board| board.tasks.iter_mut().find(|task| task.id == id))
        {
            *summary = before.summary;
        }
        if before.detail.is_some() {
            self.detail = before.detail;
        }
        self.message = Some(format!("{id} was not saved: {reason}"));
    }

    /// Puts a refused move back and says why.
    fn undo_move(&mut self, id: &str, from: &str, reason: String) {
        if let Some(task) = self
            .board
            .as_mut()
            .and_then(|board| board.tasks.iter_mut().find(|task| task.id == id))
        {
            from.clone_into(&mut task.status);
        }
        self.message = Some(format!("{id} was not moved: {reason}"));
    }

    /// The details on screen: the selected task's, once loaded.
    pub(super) fn shown_detail(&self) -> Option<&TaskDetail> {
        self.detail
            .as_ref()
            .filter(|detail| Some(&detail.summary.id) == self.selected.as_ref())
    }
}

/// IDs in reading order: `TASK-2` before `TASK-10`, `TASK-1.9` before `TASK-1.10`.
pub(super) fn id_order(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(id: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut start = 0;
        let bytes = id.as_bytes();
        for index in 1..=bytes.len() {
            if index == bytes.len()
                || bytes[index].is_ascii_digit() != bytes[index - 1].is_ascii_digit()
            {
                parts.push(&id[start..index]);
                start = index;
            }
        }
        parts
    }
    let (left, right) = (chunks(a), chunks(b));
    for (x, y) in left.iter().zip(&right) {
        let order = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()),
        };
        if order.is_ne() {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

/// The task's pull request among its references, with a short label.
pub(super) fn pull_request(task: &TaskSummary) -> Option<(&str, String)> {
    task.references.iter().find_map(|reference| {
        let number = ["/pull/", "/merge_requests/", "/pull-requests/"]
            .iter()
            .find_map(|marker| reference.split_once(marker))?
            .1
            .split(|ch: char| !ch.is_ascii_digit())
            .next()
            .filter(|number| !number.is_empty())?;
        Some((reference.as_str(), format!("PR #{number}")))
    })
}

/// Who a comment from the Board is by: the person at this computer.
fn comment_author() -> String {
    let name = ["USER", "USERNAME"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|name| !name.trim().is_empty())
        })
        .unwrap_or_else(|| "you".to_owned());
    format!("@{}", name.trim())
}

pub(super) fn priority_rank(priority: Option<&str>) -> u8 {
    match priority {
        Some("high") => 0,
        Some("medium") => 1,
        Some("low") => 2,
        _ => 9,
    }
}

impl ClientShellState {
    /// Opens the Board on the focused space, or closes it when open.
    pub(super) fn toggle_board_view(&mut self, outcome: &mut ClientShellInput) {
        if self.board_view.is_some() {
            self.close_board_view(outcome);
        } else {
            self.open_board_view(None, outcome);
        }
    }

    /// Opens the Board, focusing `workspace_id` first when given. It takes
    /// the git view's place: one stand-in tab at a time.
    pub(super) fn open_board_view(
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
        self.close_notes_view(outcome);
        let reuse = self
            .board_view
            .as_ref()
            .is_some_and(|view| view.workspace_id == target && view.boot_id == boot_id);
        if !reuse {
            self.board_view_generation = self.board_view_generation.wrapping_add(1);
            self.board_view = Some(ClientBoardView::new(
                self.board_view_generation,
                boot_id,
                target,
            ));
        }
        if let Some(view) = self.board_view.as_mut() {
            if needs_focus {
                view.tab_id = None;
                view.focus_requested_until = Some(Instant::now() + Duration::from_secs(3));
            } else {
                view.tab_id = focused_tab;
            }
        }
        self.mode = self.copy_or_terminal_mode();
        outcome.repaint = true;
        self.load_board_view(outcome);
    }

    pub(super) fn close_board_view(&mut self, outcome: &mut ClientShellInput) {
        if self.board_view.take().is_some() {
            outcome.repaint = true;
        }
    }

    /// Follows the focused space like the git view does; choosing another of
    /// the space's tabs puts its panes back.
    pub(super) fn retarget_board_view(&mut self) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            self.board_view = None;
            return;
        };
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        let Some(focused) = snapshot.focused_workspace_id.clone() else {
            self.board_view = None;
            return;
        };
        let same_boot = view.boot_id == snapshot.boot_id;
        if same_boot && view.workspace_id == focused {
            view.focus_requested_until = None;
            match view.tab_id.as_deref() {
                None => view.tab_id = snapshot.focused_tab_id.clone(),
                Some(tab_id) if snapshot.focused_tab_id.as_deref() != Some(tab_id) => {
                    self.board_view = None;
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
        let boot_id = snapshot.boot_id.clone();
        let focused_tab = snapshot.focused_tab_id.clone();
        self.board_view_generation = self.board_view_generation.wrapping_add(1);
        let mut view = ClientBoardView::new(self.board_view_generation, boot_id, focused);
        view.tab_id = focused_tab;
        self.board_view = Some(view);
        // Snapshots arrive without an outcome to send through; the tick loads it.
        self.board_view_needs_load = true;
    }

    /// Sends a load a retarget left waiting, and every [`STAMP_INTERVAL`]
    /// asks whether the tasks changed (an agent's `herdr task`, a raw
    /// `backlog` edit, another Board). Nothing happens while no Board is open.
    pub(crate) fn tick_board_view(&mut self, now: Instant) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        if std::mem::take(&mut self.board_view_needs_load) {
            self.load_board_view(&mut outcome);
        }
        // Nobody is looking while the terminal window is in the background.
        let in_background = self.outer_focused == Some(false);
        let Some(view) = self.board_view.as_mut() else {
            return outcome;
        };
        let watching = matches!(view.load, BoardLoad::Ready | BoardLoad::NotInitialized);
        if !watching
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
        let method = Method::TaskStamp(crate::api::schema::TaskTarget {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
        });
        self.send_board_request(method, generation, BoardRequest::Stamp, &mut outcome);
        outcome
    }

    /// Asks for the task list, and the selected task's details when shown.
    pub(super) fn load_board_view(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        if view.load == BoardLoad::Unsupported {
            return;
        }
        let generation = view.generation;
        let workspace_id = Some(view.workspace_id.clone());
        let mut requests = Vec::new();
        if view.list_stale && !view.list_in_flight {
            view.list_stale = false;
            view.list_in_flight = true;
            requests.push((
                Method::TaskList(crate::api::schema::TaskTarget {
                    workspace_id: workspace_id.clone(),
                    pane_id: None,
                }),
                BoardRequest::List,
            ));
        }
        let wanted = view
            .selected
            .clone()
            .filter(|_| view.details_open)
            .filter(|id| view.get_in_flight.as_ref() != Some(id))
            .filter(|id| {
                // Fetch again when the list says the task changed.
                let have = view
                    .shown_detail()
                    .map(|detail| &detail.summary.fingerprint);
                let listed = view.task(id).map(|task| &task.fingerprint);
                have.is_none() || (listed.is_some() && have != listed)
            });
        if let Some(id) = wanted {
            view.get_in_flight = Some(id.clone());
            requests.push((
                Method::TaskGet(crate::api::schema::TaskGetParams {
                    workspace_id,
                    pane_id: None,
                    id: id.clone(),
                }),
                BoardRequest::Get { id },
            ));
        }
        for (method, request) in requests {
            if !self.send_board_request(method, generation, request, outcome) {
                return;
            }
        }
    }

    /// Returns false (and marks the Board unsupported) when the server cannot
    /// take it; what the request already showed is put back.
    fn send_board_request(
        &mut self,
        method: Method,
        generation: u64,
        request: BoardRequest,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let supported = self.supports_endpoint_method(&method);
        let rereads_list = matches!(request, BoardRequest::List);
        let undo = match &request {
            BoardRequest::Move { id, from } => Some((id.clone(), from.clone())),
            _ => None,
        };
        let draft = match &request {
            BoardRequest::Create { status, title } => Some(BoardDraft {
                status: status.clone(),
                title: TextEditor::from(title.as_str()),
            }),
            _ => None,
        };
        let sent = self.push_endpoint_method_with_kind(
            method,
            PendingEndpointKind::Board {
                generation,
                request,
            },
            outcome,
        );
        if !sent {
            if let Some(view) = self.board_view.as_mut() {
                if !supported {
                    view.load = BoardLoad::Unsupported;
                }
                view.list_stale |= rereads_list;
                if let Some((id, from)) = undo {
                    view.pending_moves.remove(&id);
                    view.undo_move(&id, &from, "the server is not reachable".to_owned());
                }
                if draft.is_some() {
                    view.draft = draft;
                }
                view.list_in_flight = false;
                view.stamp_in_flight = false;
                view.get_in_flight = None;
                view.init_busy = false;
            }
            outcome.repaint = true;
        }
        sent
    }

    pub(super) fn refresh_board_view(&mut self, outcome: &mut ClientShellInput) {
        if let Some(view) = self.board_view.as_mut() {
            if view.load == BoardLoad::Unsupported {
                view.load = BoardLoad::Loading;
            }
            // An answer that never came must not block reading again.
            view.list_stale = true;
            view.list_in_flight = false;
            view.get_in_flight = None;
            outcome.repaint = true;
        }
        self.load_board_view(outcome);
    }

    pub(super) fn init_board(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        if view.init_busy {
            return;
        }
        view.init_busy = true;
        view.message = None;
        let generation = view.generation;
        let method = Method::TaskInit(crate::api::schema::TaskTarget {
            workspace_id: Some(view.workspace_id.clone()),
            pane_id: None,
        });
        outcome.repaint = true;
        self.send_board_request(method, generation, BoardRequest::Init, outcome);
    }

    pub(super) fn select_board_task(&mut self, id: String, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        if view.selected.as_ref() != Some(&id) {
            view.details_scroll = 0;
        }
        view.selected = Some(id);
        view.details_open = true;
        view.reveal_selection = true;
        outcome.repaint = true;
        self.load_board_view(outcome);
    }

    pub(super) fn set_board_mode(&mut self, mode: BoardMode, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        if view.mode == mode {
            return;
        }
        view.mode = mode;
        view.list_scroll = 0;
        outcome.repaint = true;
        if let Some(root) = view.board.as_ref().map(|board| board.root.clone()) {
            let changed = match mode {
                BoardMode::Kanban => self.board_kanban_roots.insert(root),
                BoardMode::List => self.board_kanban_roots.remove(&root),
            };
            if changed {
                self.persist_chrome_preferences(outcome);
            }
        }
    }

    fn move_board_selection(&mut self, delta: isize, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_ref() else {
            return;
        };
        let order = view.list_order();
        if order.is_empty() {
            return;
        }
        let current = view
            .selected
            .as_ref()
            .and_then(|id| order.iter().position(|other| other == id));
        let next = match current {
            Some(index) => index
                .saturating_add_signed(delta)
                .min(order.len().saturating_sub(1)),
            None => 0,
        };
        if let Some(id) = order.get(next).cloned() {
            self.select_board_task(id, outcome);
        }
    }

    /// Keys while the Board is in front and no popup or modal takes them.
    pub(super) fn route_board_view_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        if let Some(editing) = view.editing.as_mut() {
            match code {
                KeyCode::Esc => view.editing = None,
                KeyCode::Enter => self.commit_board_edit(outcome),
                _ => {
                    if editing.text.handle_key(key).is_none() {
                        return;
                    }
                }
            }
            outcome.repaint = true;
            return;
        }
        if let Some(draft) = view.draft.as_mut() {
            match code {
                KeyCode::Esc => view.draft = None,
                KeyCode::Enter => self.commit_board_draft(outcome),
                _ => {
                    if draft.title.handle_key(key).is_none() {
                        return;
                    }
                }
            }
            outcome.repaint = true;
            return;
        }
        if view.search_focused {
            match code {
                KeyCode::Esc => {
                    view.search_focused = false;
                    view.search.clear();
                }
                KeyCode::Enter => view.search_focused = false,
                _ => {
                    if view.search.handle_key(key).is_none() {
                        return;
                    }
                    view.list_scroll = 0;
                }
            }
            outcome.repaint = true;
            return;
        }
        if !modifiers
            .difference(crossterm::event::KeyModifiers::SHIFT)
            .is_empty()
        {
            return;
        }
        match code {
            KeyCode::Char('/') => {
                view.search_focused = true;
                outcome.repaint = true;
            }
            KeyCode::Esc if view.details_open => {
                view.details_open = false;
                outcome.repaint = true;
            }
            KeyCode::Esc => self.close_board_view(outcome),
            KeyCode::Down | KeyCode::Char('j') => self.move_board_selection(1, outcome),
            KeyCode::Up | KeyCode::Char('k') => self.move_board_selection(-1, outcome),
            KeyCode::Enter => {
                if let Some(id) = view.selected.clone() {
                    self.select_board_task(id, outcome);
                }
            }
            KeyCode::Char('r') => self.refresh_board_view(outcome),
            KeyCode::Char('n') => {
                if let Some(status) = view.statuses().first().cloned() {
                    self.start_board_draft(status, outcome);
                }
            }
            _ => {}
        }
    }

    /// Typed text: into the search field, else a lone character is that key.
    pub(super) fn route_board_view_text(&mut self, text: &str, outcome: &mut ClientShellInput) {
        if self.board_view_text_field_focused() {
            outcome.repaint |= self.insert_board_view_text(text);
            return;
        }
        let mut chars = text.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            let key = crate::input::TerminalKey::new(
                KeyCode::Char(ch),
                crossterm::event::KeyModifiers::empty(),
            );
            self.route_board_view_key(&key, outcome);
        }
    }

    /// Pasted text goes to the new task's title or the search field.
    pub(super) fn insert_board_view_text(&mut self, text: &str) -> bool {
        let Some(view) = self.board_view.as_mut() else {
            return false;
        };
        if let Some(editing) = view.editing.as_mut() {
            return editing.text.insert(text);
        }
        if let Some(draft) = view.draft.as_mut() {
            // A title is one line.
            return draft.title.insert(&text.replace(['\r', '\n'], " "));
        }
        if !view.search_focused {
            return false;
        }
        view.list_scroll = 0;
        view.search.insert(text)
    }

    pub(super) fn board_view_text_field_focused(&self) -> bool {
        self.board_view.as_ref().is_some_and(|view| {
            view.search_focused || view.draft.is_some() || view.editing.is_some()
        })
    }

    /// Starts naming a new task in `status`.
    pub(super) fn start_board_draft(&mut self, status: String, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        view.search_focused = false;
        view.collapsed.remove(&status);
        view.column_scroll.remove(&status);
        view.draft = Some(BoardDraft {
            status,
            title: TextEditor::default(),
        });
        view.reveal_selection = true;
        outcome.repaint = true;
    }

    /// Creates the drafted task; an empty title just drops the draft.
    fn commit_board_draft(&mut self, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        let Some(draft) = view.draft.take() else {
            return;
        };
        outcome.repaint = true;
        let title = draft.title.as_str().trim().to_owned();
        if title.is_empty() {
            return;
        }
        view.message = None;
        let generation = view.generation;
        let method = Method::TaskCreate(crate::api::schema::TaskCreateParams {
            workspace_id: Some(view.workspace_id.clone()),
            title: title.clone(),
            status: Some(draft.status.clone()),
            ..crate::api::schema::TaskCreateParams::default()
        });
        let request = BoardRequest::Create {
            status: draft.status,
            title,
        };
        self.send_board_request(method, generation, request, outcome);
    }

    /// Starts typing into a details field of the selected task.
    fn start_board_edit(&mut self, field: BoardField, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        let Some(task) = view.selected_task() else {
            return;
        };
        let text = match field {
            BoardField::Title => task.title.clone(),
            BoardField::Assignee => task.assignees.join(", "),
            BoardField::Type => task.task_type.clone().unwrap_or_default(),
            BoardField::Comment => String::new(),
            BoardField::Description => {
                let Some(detail) = view.shown_detail() else {
                    return;
                };
                let text = detail.description.clone().unwrap_or_default();
                // ponytail: the field is one line; multi-line text would lose its breaks.
                if text.trim().contains('\n') {
                    view.message =
                        Some("a description with several lines is edited in the task file".into());
                    outcome.repaint = true;
                    return;
                }
                text
            }
        };
        let id = task.id.clone();
        view.search_focused = false;
        view.editing = Some(BoardEditing {
            id,
            field,
            text: TextEditor::from(text.trim()),
        });
        view.message = None;
        outcome.repaint = true;
    }

    /// Saves the field being typed into, when it changed.
    fn commit_board_edit(&mut self, outcome: &mut ClientShellInput) {
        let Some(editing) = self
            .board_view
            .as_mut()
            .and_then(|view| view.editing.take())
        else {
            return;
        };
        outcome.repaint = true;
        let Some(task) = self
            .board_view
            .as_ref()
            .and_then(|view| view.task(&editing.id))
            .cloned()
        else {
            return;
        };
        let text = editing.text.as_str().trim().to_owned();
        let id = editing.id;
        match editing.field {
            BoardField::Title if !text.is_empty() && text != task.title => {
                let value = text.clone();
                self.edit_board_task(
                    &id,
                    |params| params.title = Some(value),
                    move |summary, _| summary.title = text,
                    outcome,
                );
            }
            BoardField::Description => {
                let current = self
                    .board_view
                    .as_ref()
                    .and_then(|view| view.shown_detail())
                    .and_then(|detail| detail.description.clone())
                    .unwrap_or_default();
                if text != current.trim() {
                    let value = text.clone();
                    self.edit_board_task(
                        &id,
                        |params| params.description = Some(value),
                        move |_, detail| {
                            if let Some(detail) = detail {
                                detail.description = Some(text);
                            }
                        },
                        outcome,
                    );
                }
            }
            BoardField::Assignee => {
                let assignees: Vec<String> = text
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(|name| {
                        if name.starts_with('@') {
                            name.to_owned()
                        } else {
                            format!("@{name}")
                        }
                    })
                    .collect();
                if assignees != task.assignees {
                    let value = assignees.clone();
                    self.edit_board_task(
                        &id,
                        |params| params.assignees = Some(value),
                        move |summary, _| summary.assignees = assignees,
                        outcome,
                    );
                }
            }
            BoardField::Type if !text.is_empty() && Some(&text) != task.task_type.as_ref() => {
                let value = text.clone();
                self.edit_board_task(
                    &id,
                    |params| params.task_type = Some(value),
                    move |summary, _| summary.task_type = Some(text),
                    outcome,
                );
            }
            BoardField::Comment if !text.is_empty() => {
                let author = comment_author();
                let (body, value_author) = (text.clone(), author.clone());
                self.edit_board_task(
                    &id,
                    |params| {
                        params.comment = Some(body);
                        params.comment_author = Some(value_author);
                    },
                    move |_, detail| {
                        if let Some(detail) = detail {
                            detail.comments.push(crate::api::schema::TaskComment {
                                author: Some(author),
                                created_at: None,
                                body: text,
                            });
                        }
                    },
                    outcome,
                );
            }
            _ => {}
        }
    }

    /// Shows an edit at once and saves it with the fingerprint on screen;
    /// a refusal puts the task back as it was.
    fn edit_board_task(
        &mut self,
        id: &str,
        params: impl FnOnce(&mut crate::api::schema::TaskUpdateParams),
        show: impl FnOnce(&mut TaskSummary, Option<&mut TaskDetail>),
        outcome: &mut ClientShellInput,
    ) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        outcome.repaint = true;
        if view.is_busy(id) {
            view.message = Some(format!("{id} is still saving"));
            return;
        }
        let Some(summary) = view.task(id).cloned() else {
            return;
        };
        let detail = view.detail.clone().filter(|detail| detail.summary.id == id);
        let mut update = crate::api::schema::TaskUpdateParams {
            workspace_id: Some(view.workspace_id.clone()),
            id: id.to_owned(),
            fingerprint: Some(summary.fingerprint.clone()),
            ..crate::api::schema::TaskUpdateParams::default()
        };
        params(&mut update);
        let before = Box::new(TaskSnapshot { summary, detail });
        let shown_detail = view
            .detail
            .as_mut()
            .filter(|detail| detail.summary.id == id);
        if let Some(summary) = view
            .board
            .as_mut()
            .and_then(|board| board.tasks.iter_mut().find(|task| task.id == id))
        {
            show(summary, shown_detail);
            if let Some(detail) = view
                .detail
                .as_mut()
                .filter(|detail| detail.summary.id == id)
            {
                detail.summary = summary.clone();
            }
        }
        view.saving.insert(id.to_owned());
        view.message = None;
        let generation = view.generation;
        let request = BoardRequest::Edit {
            id: id.to_owned(),
            before,
        };
        self.send_board_request(Method::TaskUpdate(update), generation, request, outcome);
    }

    /// Shows `id` in `to` at once and saves it; a refusal puts it back.
    pub(super) fn move_board_task(&mut self, id: &str, to: &str, outcome: &mut ClientShellInput) {
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        if view.is_busy(id) {
            view.message = Some(format!("{id} is still saving"));
            outcome.repaint = true;
            return;
        }
        let Some(task) = view
            .board
            .as_mut()
            .and_then(|board| board.tasks.iter_mut().find(|task| task.id == id))
        else {
            return;
        };
        if task.status == to {
            return;
        }
        let from = std::mem::replace(&mut task.status, to.to_owned());
        let fingerprint = task.fingerprint.clone();
        view.pending_moves.insert(id.to_owned(), to.to_owned());
        view.message = None;
        outcome.repaint = true;
        let generation = view.generation;
        let method = Method::TaskUpdate(crate::api::schema::TaskUpdateParams {
            workspace_id: Some(view.workspace_id.clone()),
            id: id.to_owned(),
            fingerprint: Some(fingerprint),
            status: Some(to.to_owned()),
            ..crate::api::schema::TaskUpdateParams::default()
        });
        let request = BoardRequest::Move {
            id: id.to_owned(),
            from,
        };
        self.send_board_request(method, generation, request, outcome);
    }

    /// Mouse events over the Board. Returns false for events elsewhere (the
    /// sidebar and tab bar keep working; a real tab's click closes the Board).
    pub(super) fn route_board_view_mouse(
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
            self.board_view = None;
            outcome.repaint = true;
            return false;
        }
        if self
            .board_view
            .as_ref()
            .is_some_and(|view| view.drag.is_some())
        {
            // A held card follows the pointer anywhere until it is let go.
            self.route_board_drag(mouse, outcome);
            return true;
        }
        let hits = &self.hits.board_view;
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
                let (list, details) = (hits.list, hits.details);
                let column = hits
                    .columns
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, status)| status.clone());
                if let Some(view) = self.board_view.as_mut() {
                    if super::contains(details, point) {
                        view.details_scroll = view.details_scroll.saturating_add_signed(delta);
                    } else if let Some(status) = column {
                        let scroll = view.column_scroll.entry(status).or_default();
                        // Cards are tall; one per notch.
                        *scroll = scroll.saturating_add_signed(delta.signum());
                    } else if super::contains(list, point) {
                        view.list_scroll = view.list_scroll.saturating_add_signed(delta);
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
                if let Some(view) = self.board_view.as_mut() {
                    if view.search_focused && target != Some(BoardTarget::Search) {
                        view.search_focused = false;
                        outcome.repaint = true;
                    }
                }
                if self
                    .board_view
                    .as_ref()
                    .is_some_and(|view| view.draft.is_some())
                    && !matches!(target, Some(BoardTarget::Draft))
                {
                    // Clicking away keeps a typed title as a new task.
                    self.commit_board_draft(outcome);
                }
                if self
                    .board_view
                    .as_ref()
                    .is_some_and(|view| view.editing.is_some())
                    && !matches!(target, Some(BoardTarget::Editing))
                {
                    // Clicking away saves what was typed, as leaving a field does.
                    self.commit_board_edit(outcome);
                }
                if let Some(BoardTarget::Card(id)) = target {
                    if let Some(view) = self.board_view.as_mut() {
                        view.drag = Some(BoardDrag {
                            id,
                            start: point,
                            at: point,
                            active: false,
                            over: None,
                        });
                    }
                } else if let Some(target) = target {
                    self.activate_board_target(target, outcome);
                }
            }
            _ => {}
        }
        true
    }

    fn activate_board_target(&mut self, target: BoardTarget, outcome: &mut ClientShellInput) {
        match target {
            BoardTarget::Close => self.close_board_view(outcome),
            BoardTarget::Init => self.init_board(outcome),
            BoardTarget::Retry => self.refresh_board_view(outcome),
            BoardTarget::Mode(mode) => self.set_board_mode(mode, outcome),
            BoardTarget::Task(id) | BoardTarget::Card(id) => self.select_board_task(id, outcome),
            BoardTarget::NewTask(status) => self.start_board_draft(status, outcome),
            BoardTarget::Nothing | BoardTarget::Draft | BoardTarget::Editing => {}
            BoardTarget::Field(field) => self.start_board_edit(field, outcome),
            BoardTarget::OpenUrl(url) => {
                outcome.actions.push(ClientShellAction::OpenSafeWebUrl(url));
            }
            BoardTarget::NextStatus(id) => {
                let next = self.board_view.as_ref().and_then(|view| {
                    let task = view.task(&id)?;
                    let statuses = view.statuses();
                    let index = statuses.iter().position(|status| *status == task.status);
                    statuses
                        .get(index.map_or(0, |index| (index + 1) % statuses.len()))
                        .cloned()
                });
                if let Some(status) = next {
                    self.move_board_task(&id, &status, outcome);
                }
            }
            BoardTarget::CycleStatus => {
                let next = self.board_view.as_ref().and_then(|view| {
                    let task = view.selected_task()?;
                    let statuses = view.statuses();
                    let index = statuses.iter().position(|status| *status == task.status);
                    let next = index.map_or(0, |index| (index + 1) % statuses.len());
                    Some((task.id.clone(), statuses.get(next)?.clone()))
                });
                if let Some((id, status)) = next {
                    self.move_board_task(&id, &status, outcome);
                }
            }
            BoardTarget::CyclePriority => {
                let next = self.board_view.as_ref().and_then(|view| {
                    let task = view.selected_task()?;
                    let index = task
                        .priority
                        .as_deref()
                        .and_then(|priority| PRIORITIES.iter().position(|p| *p == priority));
                    let next = PRIORITIES[index.map_or(0, |index| (index + 1) % PRIORITIES.len())];
                    Some((task.id.clone(), next.to_owned()))
                });
                if let Some((id, priority)) = next {
                    let value = priority.clone();
                    self.edit_board_task(
                        &id,
                        |params| params.priority = Some(value),
                        move |summary, _| summary.priority = Some(priority),
                        outcome,
                    );
                }
            }
            BoardTarget::ToggleAc(index) => {
                let Some((id, checked)) = self.board_view.as_ref().and_then(|view| {
                    let detail = view.shown_detail()?;
                    let item = detail
                        .acceptance_criteria
                        .iter()
                        .find(|item| item.index == index)?;
                    Some((detail.summary.id.clone(), item.checked))
                }) else {
                    return;
                };
                self.edit_board_task(
                    &id,
                    |params| {
                        if checked {
                            params.uncheck_ac = vec![index];
                        } else {
                            params.check_ac = vec![index];
                        }
                    },
                    move |summary, detail| {
                        if let Some(item) = detail.and_then(|detail| {
                            detail
                                .acceptance_criteria
                                .iter_mut()
                                .find(|item| item.index == index)
                        }) {
                            item.checked = !checked;
                        }
                        summary.acceptance_criteria_checked = if checked {
                            summary.acceptance_criteria_checked.saturating_sub(1)
                        } else {
                            summary.acceptance_criteria_checked + 1
                        };
                    },
                    outcome,
                );
            }
            target => {
                let Some(view) = self.board_view.as_mut() else {
                    return;
                };
                match target {
                    BoardTarget::Search => view.search_focused = true,
                    BoardTarget::Filter(filter) => {
                        let agents = view.agent_choices().len();
                        let (slot, count) = match filter {
                            BoardFilter::Agent => (&mut view.agent_filter, agents),
                            BoardFilter::Priority => {
                                (&mut view.priority_filter, PRIORITIES.len() + 1)
                            }
                            BoardFilter::Status => (
                                &mut view.status_filter,
                                view.board.as_ref().map_or(0, |b| b.statuses.len()) + 1,
                            ),
                        };
                        *slot = (*slot + 1) % count.max(1);
                        view.list_scroll = 0;
                    }
                    BoardTarget::Sort(key) => {
                        view.sort_ascending = view.sort != key || !view.sort_ascending;
                        view.sort = key;
                    }
                    BoardTarget::Group(status) => {
                        if !view.collapsed.remove(&status) {
                            view.collapsed.insert(status);
                        }
                    }
                    BoardTarget::FoldTask(id) => {
                        if !view.folded_tasks.remove(&id) {
                            view.folded_tasks.insert(id);
                        }
                    }
                    BoardTarget::CloseDetails => view.details_open = false,
                    _ => {}
                }
                outcome.repaint = true;
            }
        }
    }

    /// Moves and releases while a card is held.
    fn route_board_drag(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        outcome: &mut ClientShellInput,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (mouse.column, mouse.row);
        let over = self
            .hits
            .board_view
            .drop_zones
            .iter()
            .rev()
            .find(|(rect, _)| super::contains(*rect, point))
            .map(|(_, status)| status.clone());
        let Some(view) = self.board_view.as_mut() else {
            return;
        };
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                if let Some(drag) = view.drag.as_mut() {
                    drag.active |= point != drag.start;
                    drag.at = point;
                    drag.over = over;
                    outcome.repaint |= drag.active;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(drag) = view.drag.take() else {
                    return;
                };
                outcome.repaint = true;
                if !drag.active {
                    self.select_board_task(drag.id, outcome);
                } else if let Some(to) = over {
                    self.move_board_task(&drag.id, &to, outcome);
                }
            }
            // Any other button or a second press drops the hold.
            MouseEventKind::Down(_) => {
                view.drag = None;
                outcome.repaint = true;
            }
            _ => {}
        }
    }

    /// A `task.*` answer. Returns whether to repaint and any follow-up requests.
    pub(super) fn handle_board_view_result(
        &mut self,
        generation: u64,
        request: BoardRequest,
        result: Result<ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let kanban_roots = &self.board_kanban_roots;
        let Some(view) = self
            .board_view
            .as_mut()
            .filter(|view| view.generation == generation)
        else {
            return (false, Vec::new());
        };
        match request {
            BoardRequest::List | BoardRequest::Init => {
                if matches!(request, BoardRequest::List) {
                    view.list_in_flight = false;
                } else {
                    view.init_busy = false;
                }
                match result {
                    Ok(ResponseResult::TaskList { board }) => {
                        if view.mode_root.as_ref() != Some(&board.root) {
                            view.mode = if kanban_roots.contains(&board.root) {
                                BoardMode::Kanban
                            } else {
                                BoardMode::List
                            };
                            view.mode_root = Some(board.root.clone());
                        }
                        view.status_filter = view.status_filter.min(board.statuses.len());
                        let mut board = board;
                        // A move still on its way is not in this list yet.
                        for task in &mut board.tasks {
                            if let Some(to) = view.pending_moves.get(&task.id) {
                                task.status.clone_from(to);
                            }
                        }
                        if view
                            .selected
                            .as_ref()
                            .is_some_and(|id| !board.tasks.iter().any(|task| &task.id == id))
                        {
                            view.selected = None;
                            view.details_open = false;
                        }
                        view.board = Some(board);
                        view.load = BoardLoad::Ready;
                    }
                    Ok(_) => {
                        view.load = BoardLoad::Failed("unexpected answer from the server".into())
                    }
                    Err(error) => match error.code.as_deref() {
                        Some("backlog_not_installed") => view.load = BoardLoad::NotInstalled,
                        Some("backlog_not_initialized") => view.load = BoardLoad::NotInitialized,
                        // The list itself still shows; only the button failed.
                        _ if matches!(request, BoardRequest::Init) => {
                            view.message = Some(error.message);
                        }
                        _ => view.load = BoardLoad::Failed(error.message),
                    },
                }
            }
            BoardRequest::Stamp => {
                view.stamp_in_flight = false;
                if let Ok(ResponseResult::TaskStamp { stamp }) = result {
                    // Changed since the list on screen, or set up since "not set up".
                    let shown = view.board.as_ref().map(|board| board.stamp.as_str());
                    view.list_stale |= shown != Some(stamp.as_str());
                }
            }
            BoardRequest::Get { id } => {
                if view.get_in_flight.as_ref() == Some(&id) {
                    view.get_in_flight = None;
                }
                match result {
                    Ok(ResponseResult::TaskDetail { task }) => view.detail = Some(task),
                    Ok(_) => view.message = Some("unexpected answer from the server".into()),
                    Err(error) => view.message = Some(error.message),
                }
            }
            BoardRequest::Move { id, from } => {
                view.pending_moves.remove(&id);
                match result {
                    Ok(ResponseResult::TaskDetail { task }) => view.apply_saved(task),
                    Ok(_) => view.undo_move(&id, &from, "unexpected answer from the server".into()),
                    Err(error) => {
                        let conflict = error.code.as_deref() == Some("task_conflict");
                        let reason = if conflict {
                            "it changed since the Board loaded it · reloaded".to_owned()
                        } else {
                            error.message
                        };
                        view.undo_move(&id, &from, reason);
                        view.list_stale |= conflict;
                    }
                }
            }
            BoardRequest::Edit { id, before } => {
                view.saving.remove(&id);
                match result {
                    Ok(ResponseResult::TaskDetail { task }) => view.apply_saved(task),
                    Ok(_) => view.restore(*before, "unexpected answer from the server".into()),
                    Err(error) => {
                        let conflict = error.code.as_deref() == Some("task_conflict");
                        let reason = if conflict {
                            "it changed since the Board loaded it · reloaded".to_owned()
                        } else {
                            error.message
                        };
                        view.restore(*before, reason);
                        if conflict {
                            view.list_stale = true;
                            // The details are read again with the new list's fingerprint.
                            view.detail = None;
                        }
                    }
                }
            }
            BoardRequest::Create { status, title } => match result {
                Ok(ResponseResult::TaskDetail { task }) => {
                    let id = task.summary.id.clone();
                    view.apply_saved(task);
                    view.selected = Some(id);
                    view.details_open = true;
                    view.details_scroll = 0;
                    view.reveal_selection = true;
                }
                Ok(_) => view.message = Some("unexpected answer from the server".into()),
                Err(error) => {
                    // The title comes back so nothing typed is lost.
                    view.draft = Some(BoardDraft {
                        status,
                        title: TextEditor::from(title.as_str()),
                    });
                    view.message = Some(format!("the task was not created: {}", error.message));
                }
            },
        }
        let mut outcome = ClientShellInput::default();
        // Details wanted while the list was on its way.
        self.load_board_view(&mut outcome);
        (true, outcome.actions)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BoardTarget {
    Close,
    Init,
    Retry,
    Mode(BoardMode),
    Search,
    Filter(BoardFilter),
    Sort(BoardSortKey),
    Group(String),
    /// A link to a task: a click opens it.
    Task(String),
    /// A card or list row: a click opens it, a drag moves it.
    Card(String),
    /// A column's or group's `+`.
    NewTask(String),
    /// The new task's title field.
    Draft,
    /// The details field being typed into.
    Editing,
    /// A details value edited as text.
    Field(BoardField),
    /// The details' status: a click moves the task to the next status.
    CycleStatus,
    /// The details' priority: a click sets the next one.
    CyclePriority,
    /// An acceptance criterion, by its 1-based index: a click ticks or unticks it.
    ToggleAc(u32),
    /// A parent's ▾/▸ in the list: shows or hides its subtasks.
    FoldTask(String),
    /// A subtask's status pill on a card: a click moves it to the next status.
    NextStatus(String),
    /// A link, such as a pull request: a click opens it in the browser.
    OpenUrl(String),
    CloseDetails,
    /// Takes a click so nothing under it does.
    Nothing,
}

#[derive(Debug, Default)]
pub(super) struct BoardViewHits {
    pub(super) area: Rect,
    pub(super) list: Rect,
    pub(super) details: Rect,
    pub(super) targets: Vec<(Rect, BoardTarget)>,
    /// Where a held card lands, by status.
    pub(super) drop_zones: Vec<(Rect, String)>,
    /// Kanban columns, for scrolling each on its own.
    pub(super) columns: Vec<(Rect, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: &str, priority: Option<&str>, agent: Option<&str>) -> TaskSummary {
        TaskSummary {
            id: id.into(),
            title: format!("title {id}"),
            status: status.into(),
            priority: priority.map(str::to_owned),
            task_type: None,
            assignees: agent.into_iter().map(str::to_owned).collect(),
            labels: Vec::new(),
            dependencies: Vec::new(),
            parent_id: None,
            milestone: None,
            references: Vec::new(),
            description_preview: None,
            acceptance_criteria_checked: 0,
            acceptance_criteria_total: 0,
            fingerprint: id.into(),
        }
    }

    fn view() -> ClientBoardView {
        let mut view = ClientBoardView::new(1, "boot".into(), "w1".into());
        view.board = Some(TaskBoardInfo {
            root: "/p".into(),
            statuses: vec!["To Do".into(), "Doing".into(), "Done".into()],
            tasks: vec![
                task("T-10", "To Do", Some("low"), None),
                task("T-2", "To Do", Some("high"), Some("@dev")),
                task("T-3", "Done", None, Some("@qa")),
            ],
            stamp: String::new(),
        });
        view
    }

    fn ids(rows: &[BoardRow<'_>]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                BoardRow::Group { status, count, .. } => format!("[{status} {count}]"),
                BoardRow::Task(task) => task.id.clone(),
                BoardRow::Subtask { task, .. } => format!("  {}", task.id),
                BoardRow::Draft => "draft".into(),
                BoardRow::Empty => "empty".into(),
                BoardRow::Gap => "-".into(),
            })
            .collect()
    }

    #[test]
    fn list_groups_by_status_sorts_ids_numerically_and_folds() {
        let mut view = view();
        assert_eq!(
            ids(&view.list_rows()),
            [
                "[To Do 2]",
                "T-2",
                "T-10",
                "-",
                "[Doing 0]",
                "empty",
                "-",
                "[Done 1]",
                "T-3"
            ]
        );
        view.sort = BoardSortKey::Priority;
        view.sort_ascending = false;
        view.collapsed.insert("Done".into());
        assert_eq!(
            ids(&view.list_rows()),
            [
                "[To Do 2]",
                "T-10",
                "T-2",
                "-",
                "[Doing 0]",
                "empty",
                "-",
                "[Done 1]"
            ]
        );
        assert_eq!(view.open_count(), 2);
    }

    #[test]
    fn ids_sort_in_reading_order_and_pull_requests_are_found() {
        let mut ids = vec!["T-10", "T-1.10", "T-2", "T-1.9", "T-1"];
        ids.sort_by(|a, b| id_order(a, b));
        assert_eq!(ids, ["T-1", "T-1.9", "T-1.10", "T-2", "T-10"]);
        let mut task = task("T-1", "To Do", None, None);
        task.references = vec![
            "docs/x.md".into(),
            "https://gitlab.com/g/p/-/merge_requests/9".into(),
        ];
        assert_eq!(
            pull_request(&task).map(|(_, label)| label),
            Some("PR #9".to_owned())
        );
    }

    #[test]
    fn filters_and_search_narrow_the_rows() {
        let mut view = view();
        assert_eq!(view.agent_choices(), ["all", "@dev", "@qa", "unassigned"]);
        view.agent_filter = 3;
        assert_eq!(ids(&view.list_rows())[..2], ["[To Do 1]", "T-10"]);
        view.agent_filter = 0;
        view.priority_filter = 1;
        view.status_filter = 1;
        assert_eq!(ids(&view.list_rows()), ["[To Do 1]", "T-2"]);
        view.priority_filter = 0;
        view.search.insert("t-1");
        assert_eq!(ids(&view.list_rows()), ["[To Do 1]", "T-10"]);
    }
}
