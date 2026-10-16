use super::*;
use crate::api::schema::{
    Method, ResponseResult, TaskBoardInfo, TaskCheckItem, TaskComment, TaskDetail, TaskSummary,
};
use crate::client::shell::board_view::{BoardMode, BoardTarget};

fn requests(actions: Vec<ClientShellAction>) -> Vec<crate::api::schema::Request> {
    actions
        .into_iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(*request),
            _ => None,
        })
        .collect()
}

fn state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

/// Opens the Board by its tab and returns the requests that sent.
fn open_by_tab(state: &mut ClientShellState) -> Vec<crate::api::schema::Request> {
    screen(state, 150);
    let tab = state.hits.board_tab;
    assert!(tab.width > 0, "the Board's tab is in the tab bar");
    requests(click(state, tab).actions)
}

fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + 1,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })])
}

fn mouse(state: &mut ClientShellState, kind: MouseEventKind, at: (u16, u16)) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind,
        column: at.0,
        row: at.1,
        modifiers: KeyModifiers::empty(),
    })])
}

/// A press and release on a card or row: it opens rather than moves.
fn click_card(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    let at = (rect.x + 1, rect.y);
    mouse(state, MouseEventKind::Down(MouseButton::Left), at);
    mouse(state, MouseEventKind::Up(MouseButton::Left), at)
}

/// Drags `from` onto `to` and returns what the drop sent.
fn drag(state: &mut ClientShellState, from: Rect, to: Rect) -> Vec<crate::api::schema::Request> {
    let start = (from.x + 2, from.y + from.height / 2);
    let end = (to.x + 2, to.y + to.height / 2);
    mouse(state, MouseEventKind::Down(MouseButton::Left), start);
    mouse(state, MouseEventKind::Drag(MouseButton::Left), end);
    requests(mouse(state, MouseEventKind::Up(MouseButton::Left), end).actions)
}

fn zone(state: &ClientShellState, status: &str) -> Rect {
    state
        .hits
        .board_view
        .drop_zones
        .iter()
        .find(|(_, zone)| zone == status)
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("no {status} drop zone"))
}

fn status_of(state: &ClientShellState, id: &str) -> String {
    state
        .board_view
        .as_ref()
        .and_then(|view| view.task(id))
        .map(|task| task.status.clone())
        .expect("the task")
}

fn target(state: &ClientShellState, wanted: &BoardTarget) -> Rect {
    state
        .hits
        .board_view
        .targets
        .iter()
        .rev()
        .find(|(_, target)| target == wanted)
        .map(|(rect, _)| *rect)
        .unwrap_or_else(|| panic!("{wanted:?} not on screen"))
}

fn answer(
    state: &mut ClientShellState,
    request: &crate::api::schema::Request,
    result: Result<ResponseResult, ClientShellEndpointError>,
) -> Vec<crate::api::schema::Request> {
    let (repaint, actions) = state.handle_endpoint_result("boot-1", &request.id, result);
    assert!(repaint);
    requests(actions)
}

fn screen(state: &mut ClientShellState, width: u16) -> String {
    let frame = state.compose(width, 34).expect("frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn task(id: &str, title: &str, status: &str) -> TaskSummary {
    TaskSummary {
        id: id.into(),
        title: title.into(),
        status: status.into(),
        priority: Some("high".into()),
        task_type: None,
        assignees: vec!["@developer".into()],
        labels: Vec::new(),
        dependencies: Vec::new(),
        acceptance_criteria_checked: 0,
        acceptance_criteria_total: 1,
        fingerprint: format!("fp-{id}"),
    }
}

fn board() -> ResponseResult {
    let mut second = task("TASK-2", "Build the login form", "In Progress");
    second.dependencies = vec!["TASK-1".into()];
    ResponseResult::TaskList {
        board: TaskBoardInfo {
            root: "/repo".into(),
            statuses: vec![
                "To Do".into(),
                "In Progress".into(),
                "Review".into(),
                "Done".into(),
            ],
            tasks: vec![task("TASK-1", "Design the login API", "To Do"), second],
        },
    }
}

fn detail(id: &str) -> ResponseResult {
    let ResponseResult::TaskList { board } = board() else {
        unreachable!()
    };
    let summary = board
        .tasks
        .into_iter()
        .find(|task| task.id == id)
        .expect("a task");
    ResponseResult::TaskDetail {
        task: TaskDetail {
            summary,
            path: "backlog/tasks/x.md".into(),
            description: Some("Wire the form to the API.".into()),
            acceptance_criteria: vec![TaskCheckItem {
                index: 1,
                text: "Form posts credentials".into(),
                checked: false,
            }],
            implementation_plan: None,
            implementation_notes: None,
            final_summary: None,
            comments: vec![TaskComment {
                author: Some("@reviewer".into()),
                created_at: None,
                body: "Looks good so far".into(),
            }],
        },
    }
}

fn loaded() -> ClientShellState {
    let mut state = state();
    let opened = open_by_tab(&mut state);
    let list = opened
        .iter()
        .find(|request| matches!(&request.method, Method::TaskList(target) if target.workspace_id.as_deref() == Some("ws_1")))
        .expect("the list is asked for");
    answer(&mut state, list, Ok(board()));
    state
}

#[test]
fn the_board_tab_opens_a_grouped_list_in_place_of_the_panes() {
    let mut state = loaded();
    let text = screen(&mut state, 150);
    assert!(text.contains("▦ Board  client-shell"), "{text}");
    assert!(
        text.contains("▦ board 2"),
        "the tab counts open tasks: {text}"
    );
    assert!(text.contains("▾ To Do 1"), "{text}");
    assert!(text.contains("▾ Review 0"), "{text}");
    assert!(text.contains("Design the login API"), "{text}");
    assert!(text.contains("TASK-1"), "the deps column: {text}");
    assert!(
        !text.contains("LIVE"),
        "the panes are hidden behind the Board"
    );
    assert!(!state.focused_pane_receives_keys());

    // Folding a group hides its rows.
    let group = target(&state, &BoardTarget::Group("To Do".into()));
    click(&mut state, group);
    let text = screen(&mut state, 150);
    assert!(text.contains("▸ To Do 1"), "{text}");
    assert!(!text.contains("Design the login API"), "{text}");

    // A real tab brings the panes back.
    let tab = state.hits.tabs[0].0;
    click(&mut state, tab);
    assert!(state.board_view.is_none());
    assert!(screen(&mut state, 150).contains("LIVE"));
}

#[test]
fn a_row_opens_docked_details_or_a_popup_when_narrow() {
    let mut state = loaded();
    screen(&mut state, 150);
    let row = target(&state, &BoardTarget::Card("TASK-2".into()));
    let get = requests(click_card(&mut state, row).actions);
    let get = get
        .iter()
        .find(|request| matches!(&request.method, Method::TaskGet(params) if params.id == "TASK-2"))
        .expect("details are asked for");
    let text = screen(&mut state, 150);
    assert!(text.contains("Build the login form"), "{text}");
    assert!(text.contains("loading…"), "{text}");
    answer(&mut state, get, Ok(detail("TASK-2")));

    let text = screen(&mut state, 150);
    assert!(text.contains("Wire the form to the API."), "{text}");
    assert!(text.contains("[ ] Form posts credentials"), "{text}");
    assert!(text.contains("Looks good so far"), "{text}");
    assert!(
        !text.contains(" task TASK-2 "),
        "docked, not a popup: {text}"
    );
    // The dependency links to its task.
    target(&state, &BoardTarget::Task("TASK-1".into()));

    let text = screen(&mut state, 90);
    assert!(text.contains(" task TASK-2 "), "narrow: a popup: {text}");
    assert!(text.contains("Wire the form to the API."), "{text}");

    // Esc closes the details, then the Board.
    state.handle_input_bytes(b"\x1b");
    assert!(state
        .board_view
        .as_ref()
        .is_some_and(|view| !view.details_open));
    state.handle_input_bytes(b"\x1b");
    assert!(state.board_view.is_none());
}

#[test]
fn search_and_filters_narrow_the_list() {
    let mut state = loaded();
    screen(&mut state, 150);
    state.handle_input_bytes(b"/");
    state.handle_input_bytes(b"form");
    let text = screen(&mut state, 150);
    assert!(text.contains("Build the login form"), "{text}");
    assert!(!text.contains("Design the login API"), "{text}");
    state.handle_input_bytes(b"\x1b");
    assert!(screen(&mut state, 150).contains("Design the login API"));

    let status = target(
        &state,
        &BoardTarget::Filter(crate::client::shell::board_view::BoardFilter::Status),
    );
    click(&mut state, status);
    let text = screen(&mut state, 150);
    assert!(text.contains("status: To Do"), "{text}");
    assert!(!text.contains("In Progress 1"), "{text}");
}

#[test]
fn a_space_without_backlog_offers_to_initialize_it() {
    let mut state = state();
    let opened = open_by_tab(&mut state);
    answer(
        &mut state,
        &opened[0],
        Err(ClientShellEndpointError {
            code: Some("backlog_not_initialized".into()),
            message: "this folder has no Backlog.md project".into(),
        }),
    );
    let text = screen(&mut state, 150);
    assert!(
        text.contains("Backlog.md is not set up in this space"),
        "{text}"
    );
    let init = target(&state, &BoardTarget::Init);
    let sent = requests(click(&mut state, init).actions);
    assert!(
        matches!(&sent[0].method, Method::TaskInit(target) if target.workspace_id.as_deref() == Some("ws_1")),
        "{sent:?}"
    );
    assert!(screen(&mut state, 150).contains("setting up…"));
    answer(&mut state, &sent[0], Ok(board()));
    assert!(screen(&mut state, 150).contains("▾ To Do 1"));
}

#[test]
fn the_chosen_view_is_remembered_per_project() {
    let mut state = loaded();
    screen(&mut state, 150);
    let kanban = target(&state, &BoardTarget::Mode(BoardMode::Kanban));
    click(&mut state, kanban);
    assert!(state.board_kanban_roots.contains("/repo"));

    // Reopened, the project comes back in kanban.
    state.handle_input_bytes(b"\x1b");
    let opened = open_by_tab(&mut state);
    answer(&mut state, &opened[0], Ok(board()));
    assert_eq!(
        state.board_view.as_ref().map(|view| view.mode),
        Some(BoardMode::Kanban)
    );
}

#[test]
fn missing_backlog_shows_install_help_and_retries() {
    let mut state = state();
    let opened = open_by_tab(&mut state);
    answer(
        &mut state,
        &opened[0],
        Err(ClientShellEndpointError {
            code: Some("backlog_not_installed".into()),
            message: "not installed".into(),
        }),
    );
    let text = screen(&mut state, 150);
    assert!(text.contains("Backlog.md is not installed"), "{text}");
    let retry = target(&state, &BoardTarget::Retry);
    let sent = requests(click(&mut state, retry).actions);
    assert!(matches!(&sent[0].method, Method::TaskList(_)));
}

fn kanban() -> ClientShellState {
    let mut state = loaded();
    screen(&mut state, 150);
    let kanban = target(&state, &BoardTarget::Mode(BoardMode::Kanban));
    click(&mut state, kanban);
    state
}

#[test]
fn kanban_shows_a_column_per_status_with_boxed_cards() {
    let mut state = kanban();
    let text = screen(&mut state, 150);
    for column in [" To Do  1 ", " In Progress  1 ", " Review  0 ", " Done  0 "] {
        assert!(text.contains(column), "{column}: {text}");
    }
    assert!(text.contains("Design the login"), "{text}");
    assert!(text.contains("● Developer"), "{text}");
    assert!(text.contains("✓0/1"), "{text}");
    assert!(text.contains("No tasks"), "{text}");
}

#[test]
fn dragging_a_card_moves_it_at_once_and_back_when_the_save_fails() {
    let mut state = kanban();
    screen(&mut state, 150);
    let card = target(&state, &BoardTarget::Card("TASK-1".into()));
    let to = zone(&state, "Review");
    let sent = drag(&mut state, card, to);
    let update = sent
        .iter()
        .find(|request| {
            matches!(&request.method, Method::TaskUpdate(params)
            if params.id == "TASK-1"
                && params.status.as_deref() == Some("Review")
                && params.fingerprint.as_deref() == Some("fp-TASK-1"))
        })
        .expect("the move is saved with the fingerprint the Board showed");
    assert_eq!(
        status_of(&state, "TASK-1"),
        "Review",
        "moved before the answer"
    );
    assert!(screen(&mut state, 150).contains("saving…"));

    answer(
        &mut state,
        update,
        Err(ClientShellEndpointError {
            code: Some("invalid_request".into()),
            message: "Invalid status".into(),
        }),
    );
    assert_eq!(
        status_of(&state, "TASK-1"),
        "To Do",
        "a refused move goes back"
    );
    assert!(screen(&mut state, 150).contains("TASK-1 was not moved: Invalid status"));
}

#[test]
fn a_saved_move_takes_the_new_fingerprint_and_a_conflict_reloads() {
    let mut state = kanban();
    screen(&mut state, 150);
    let card = target(&state, &BoardTarget::Card("TASK-1".into()));
    let to = zone(&state, "Done");
    let sent = drag(&mut state, card, to);
    let ResponseResult::TaskDetail { mut task } = detail("TASK-1") else {
        unreachable!()
    };
    task.summary.status = "Done".into();
    task.summary.fingerprint = "fp-new".into();
    answer(
        &mut state,
        &sent[0],
        Ok(ResponseResult::TaskDetail { task }),
    );
    let view = state.board_view.as_ref().expect("open");
    assert_eq!(
        view.task("TASK-1").map(|task| task.fingerprint.as_str()),
        Some("fp-new")
    );
    assert!(view.pending_moves.is_empty());

    screen(&mut state, 150);
    let card = target(&state, &BoardTarget::Card("TASK-2".into()));
    let to = zone(&state, "Review");
    let sent = drag(&mut state, card, to);
    let reload = answer(
        &mut state,
        &sent[0],
        Err(ClientShellEndpointError {
            code: Some("task_conflict".into()),
            message: "changed".into(),
        }),
    );
    assert_eq!(status_of(&state, "TASK-2"), "In Progress");
    assert!(
        reload
            .iter()
            .any(|request| matches!(request.method, Method::TaskList(_))),
        "a conflict reads the list again: {reload:?}"
    );
    assert!(screen(&mut state, 150).contains("changed since the Board loaded it"));
}

#[test]
fn a_list_row_drags_into_another_group() {
    let mut state = loaded();
    screen(&mut state, 150);
    let row = target(&state, &BoardTarget::Card("TASK-2".into()));
    let to = zone(&state, "To Do");
    let sent = drag(&mut state, row, to);
    assert!(sent.iter().any(|request| matches!(&request.method,
        Method::TaskUpdate(params) if params.id == "TASK-2" && params.status.as_deref() == Some("To Do"))));
    assert!(screen(&mut state, 150).contains("▾ To Do 2"));
}

#[test]
fn a_column_plus_names_and_creates_a_task_there() {
    let mut state = kanban();
    screen(&mut state, 150);
    let plus = target(&state, &BoardTarget::NewTask("Review".into()));
    click(&mut state, plus);
    assert!(screen(&mut state, 150).contains("title · enter to add"));
    state.handle_input_bytes(b"Check the tokens");
    let sent = requests(state.handle_input_bytes(b"\r").actions);
    let create = sent
        .iter()
        .find(|request| {
            matches!(&request.method, Method::TaskCreate(params)
            if params.title == "Check the tokens" && params.status.as_deref() == Some("Review"))
        })
        .expect("the task is created in its column");
    let ResponseResult::TaskDetail { mut task } = detail("TASK-1") else {
        unreachable!()
    };
    task.summary.id = "TASK-3".into();
    task.summary.title = "Check the tokens".into();
    task.summary.status = "Review".into();
    answer(&mut state, create, Ok(ResponseResult::TaskDetail { task }));
    let view = state.board_view.as_ref().expect("open");
    assert_eq!(view.selected.as_deref(), Some("TASK-3"));
    assert!(screen(&mut state, 150).contains(" Review  1 "));
}
