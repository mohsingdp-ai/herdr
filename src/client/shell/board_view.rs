//! Board: the focused space's Backlog.md tasks, shown in place of its panes
//! like a tab. Every read and write goes through the server's `task.*`
//! methods; this module only keeps what is on screen and asks for what the
//! screen needs next. Nothing here runs while no Board is open.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::KeyEventKind;

use super::*;
use crate::api::schema::{Method, ResponseResult, TaskBoardInfo, TaskDetail, TaskSummary};

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
    Get { id: String },
    Init,
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
    pub(super) list_scroll: usize,
    pub(super) details_scroll: usize,
    /// Scroll the selection into view at the next frame.
    pub(super) reveal_selection: bool,
    pub(super) init_busy: bool,
    pub(super) message: Option<String>,
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
    Task(&'a TaskSummary),
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
            list_scroll: 0,
            details_scroll: 0,
            reveal_selection: false,
            init_busy: false,
            message: None,
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

    fn matches(&self, task: &TaskSummary, query: &str, agent: Option<&str>) -> bool {
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
        agent_ok
            && priority_ok
            && self
                .status_choice()
                .is_none_or(|status| task.status == status)
    }

    /// The visible tasks of one status, in the list's sort order.
    pub(super) fn column(&self, status: &str) -> Vec<&TaskSummary> {
        let query = self.search.as_str().trim().to_lowercase();
        let agent = self.agent_choices().get(self.agent_filter).cloned();
        let mut tasks: Vec<&TaskSummary> = self
            .tasks()
            .iter()
            .filter(|task| task.status == status && self.matches(task, &query, agent.as_deref()))
            .collect();
        tasks.sort_by(|a, b| {
            let order = match self.sort {
                BoardSortKey::Id => id_number(&a.id)
                    .cmp(&id_number(&b.id))
                    .then_with(|| a.id.cmp(&b.id)),
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
            if tasks.is_empty() {
                rows.push(BoardRow::Empty);
            }
            rows.extend(tasks.into_iter().map(BoardRow::Task));
        }
        rows
    }

    /// Task IDs in the order the list shows them, for the arrow keys.
    fn list_order(&self) -> Vec<String> {
        self.list_rows()
            .into_iter()
            .filter_map(|row| match row {
                BoardRow::Task(task) => Some(task.id.clone()),
                _ => None,
            })
            .collect()
    }

    /// The details on screen: the selected task's, once loaded.
    pub(super) fn shown_detail(&self) -> Option<&TaskDetail> {
        self.detail
            .as_ref()
            .filter(|detail| Some(&detail.summary.id) == self.selected.as_ref())
    }
}

fn id_number(id: &str) -> u64 {
    id.rsplit(['-', '.'])
        .next()
        .and_then(|number| number.parse().ok())
        .unwrap_or(u64::MAX)
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

    /// Sends a load a retarget left waiting.
    pub(crate) fn tick_board_view(&mut self) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        if std::mem::take(&mut self.board_view_needs_load) {
            self.load_board_view(&mut outcome);
        }
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
        if !view.list_in_flight {
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

    /// Returns false (and marks the Board unsupported) when the server cannot take it.
    fn send_board_request(
        &mut self,
        method: Method,
        generation: u64,
        request: BoardRequest,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let supported = self.supports_endpoint_method(&method);
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
                view.list_in_flight = false;
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

    /// Pasted text goes to the search field when it is focused.
    pub(super) fn insert_board_view_text(&mut self, text: &str) -> bool {
        let Some(view) = self.board_view.as_mut().filter(|view| view.search_focused) else {
            return false;
        };
        view.list_scroll = 0;
        view.search.insert(text)
    }

    pub(super) fn board_view_text_field_focused(&self) -> bool {
        self.board_view
            .as_ref()
            .is_some_and(|view| view.search_focused)
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
                if let Some(view) = self.board_view.as_mut() {
                    if super::contains(details, point) {
                        view.details_scroll = view.details_scroll.saturating_add_signed(delta);
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
                if let Some(target) = target {
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
            BoardTarget::Task(id) => self.select_board_task(id, outcome),
            BoardTarget::Nothing => {}
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
                    BoardTarget::CloseDetails => view.details_open = false,
                    _ => {}
                }
                outcome.repaint = true;
            }
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
    Task(String),
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
        });
        view
    }

    fn ids(rows: &[BoardRow<'_>]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                BoardRow::Group { status, count, .. } => format!("[{status} {count}]"),
                BoardRow::Task(task) => task.id.clone(),
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
