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
        parent_id: None,
        milestone: None,
        references: Vec::new(),
        description_preview: None,
        acceptance_criteria_checked: 0,
        acceptance_criteria_total: 1,
        pane_id: None,
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
            stamp: String::new(),
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
fn kanban_shows_a_column_per_status_with_cards() {
    let mut state = kanban();
    let text = screen(&mut state, 150);
    for column in ["To Do  1 ", "In Progress  1 ", "Review  0 ", "Done  0 "] {
        assert!(text.contains(column), "{column}: {text}");
    }
    assert!(text.contains("Design the login"), "{text}");
    assert!(text.contains("● Developer"), "{text}");
    assert!(text.contains("✓ 0/1"), "{text}");
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

/// The list with TASK-2's details open and loaded.
fn details_open() -> ClientShellState {
    let mut state = loaded();
    screen(&mut state, 150);
    let row = target(&state, &BoardTarget::Card("TASK-2".into()));
    let get = requests(click_card(&mut state, row).actions);
    answer(&mut state, &get[0], Ok(detail("TASK-2")));
    screen(&mut state, 150);
    state
}

fn update_params(
    requests: &[crate::api::schema::Request],
) -> &crate::api::schema::TaskUpdateParams {
    requests
        .iter()
        .find_map(|request| match &request.method {
            Method::TaskUpdate(params) => Some(params),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no update in {requests:?}"))
}

#[test]
fn editing_the_title_shows_at_once_and_comes_back_when_refused() {
    let mut state = details_open();
    let title = target(
        &state,
        &BoardTarget::Field(crate::client::shell::board_view::BoardField::Title),
    );
    click(&mut state, title);
    state.handle_input_bytes(b" v2");
    let sent = requests(state.handle_input_bytes(b"\r").actions);
    let params = update_params(&sent);
    assert_eq!(params.title.as_deref(), Some("Build the login form v2"));
    assert_eq!(params.fingerprint.as_deref(), Some("fp-TASK-2"));
    assert!(screen(&mut state, 150).contains("Build the login form v2"));

    // A second edit waits for the first one's new fingerprint.
    let priority = target(&state, &BoardTarget::CyclePriority);
    assert!(requests(click(&mut state, priority).actions).is_empty());
    assert!(screen(&mut state, 150).contains("TASK-2 is still saving"));

    answer(
        &mut state,
        &sent[0],
        Err(ClientShellEndpointError {
            code: Some("invalid_request".into()),
            message: "title refused".into(),
        }),
    );
    let text = screen(&mut state, 150);
    assert!(!text.contains("login form v2"), "{text}");
    assert!(
        text.contains("TASK-2 was not saved: title refused"),
        "{text}"
    );
}

#[test]
fn details_fields_tick_cycle_and_comment() {
    let mut state = details_open();
    let tick = target(&state, &BoardTarget::ToggleAc(1));
    let sent = requests(click(&mut state, tick).actions);
    assert_eq!(update_params(&sent).check_ac, vec![1]);
    assert!(screen(&mut state, 150).contains("[x] Form posts credentials"));
    answer(&mut state, &sent[0], Ok(detail("TASK-2")));

    let priority = target(&state, &BoardTarget::CyclePriority);
    let sent = requests(click(&mut state, priority).actions);
    assert_eq!(update_params(&sent).priority.as_deref(), Some("medium"));
    answer(&mut state, &sent[0], Ok(detail("TASK-2")));

    screen(&mut state, 150);
    let status = target(&state, &BoardTarget::CycleStatus);
    let sent = requests(click(&mut state, status).actions);
    assert_eq!(update_params(&sent).status.as_deref(), Some("Review"));
    answer(&mut state, &sent[0], Ok(detail("TASK-2")));

    screen(&mut state, 150);
    let assignee = target(
        &state,
        &BoardTarget::Field(crate::client::shell::board_view::BoardField::Assignee),
    );
    click(&mut state, assignee);
    // Ctrl+U clears the field; names get their @.
    state.handle_input_bytes(b"\x15tester, @qa");
    let sent = requests(state.handle_input_bytes(b"\r").actions);
    assert_eq!(
        update_params(&sent).assignees,
        Some(vec!["@tester".to_owned(), "@qa".to_owned()])
    );
    answer(&mut state, &sent[0], Ok(detail("TASK-2")));

    screen(&mut state, 150);
    let comment = target(
        &state,
        &BoardTarget::Field(crate::client::shell::board_view::BoardField::Comment),
    );
    click(&mut state, comment);
    state.handle_input_bytes(b"Ready for review");
    let sent = requests(state.handle_input_bytes(b"\r").actions);
    let params = update_params(&sent);
    assert_eq!(params.comment.as_deref(), Some("Ready for review"));
    assert!(params
        .comment_author
        .as_deref()
        .is_some_and(|author| author.starts_with('@')));
    assert!(screen(&mut state, 150).contains("Ready for review"));
}

#[test]
fn new_task_button_and_n_key_start_a_draft_in_the_first_status() {
    let mut state = loaded();
    screen(&mut state, 150);
    assert!(screen(&mut state, 150).contains(" + New task "));
    state.handle_input_bytes(b"n");
    let draft = state
        .board_view
        .as_ref()
        .and_then(|view| view.draft.as_ref())
        .map(|draft| draft.status.clone());
    assert_eq!(draft.as_deref(), Some("To Do"));
    state.handle_input_bytes(b"\x1b");
    assert!(state
        .board_view
        .as_ref()
        .is_some_and(|view| view.draft.is_none()));
}

#[test]
fn a_multi_line_description_is_not_flattened() {
    let mut state = loaded();
    screen(&mut state, 150);
    let row = target(&state, &BoardTarget::Card("TASK-2".into()));
    let get = requests(click_card(&mut state, row).actions);
    let ResponseResult::TaskDetail { mut task } = detail("TASK-2") else {
        unreachable!()
    };
    task.description = Some("line one\nline two".into());
    answer(&mut state, &get[0], Ok(ResponseResult::TaskDetail { task }));
    screen(&mut state, 150);
    let description = target(
        &state,
        &BoardTarget::Field(crate::client::shell::board_view::BoardField::Description),
    );
    assert!(requests(click(&mut state, description).actions).is_empty());
    assert!(state
        .board_view
        .as_ref()
        .is_some_and(|view| view.editing.is_none()));
    assert!(screen(&mut state, 150).contains("edited in the task file"));
}

/// What the Board's timer sends `secs` from now.
fn tick(state: &mut ClientShellState, secs: u64) -> Vec<crate::api::schema::Request> {
    requests(
        state
            .tick_board_view(std::time::Instant::now() + std::time::Duration::from_secs(secs))
            .actions,
    )
}

#[test]
fn an_open_board_rereads_the_list_only_when_its_stamp_changes() {
    let mut state = loaded();
    assert!(tick(&mut state, 0).is_empty(), "not before the interval");
    let polled = tick(&mut state, 3);
    assert!(
        matches!(&polled[..], [request] if matches!(&request.method, Method::TaskStamp(target) if target.workspace_id.as_deref() == Some("ws_1"))),
        "{polled:?}"
    );
    assert!(tick(&mut state, 6).is_empty(), "one check at a time");
    // The board() fixture's stamp is empty: the same answer reads nothing.
    let same = answer(
        &mut state,
        &polled[0],
        Ok(ResponseResult::TaskStamp {
            stamp: String::new(),
        }),
    );
    assert!(same.is_empty(), "{same:?}");

    let polled = tick(&mut state, 9);
    let reread = answer(
        &mut state,
        &polled[0],
        Ok(ResponseResult::TaskStamp {
            stamp: "changed".into(),
        }),
    );
    assert!(
        reread
            .iter()
            .any(|request| matches!(request.method, Method::TaskList(_))),
        "{reread:?}"
    );

    // Closed, the Board asks nothing.
    state.handle_input_bytes(b"\x1b");
    assert!(state.board_view.is_none());
    assert!(tick(&mut state, 60).is_empty());
}

#[test]
fn a_space_set_up_elsewhere_shows_its_board() {
    let mut state = state();
    let opened = open_by_tab(&mut state);
    answer(
        &mut state,
        &opened[0],
        Err(ClientShellEndpointError {
            code: Some("backlog_not_initialized".into()),
            message: "no project".into(),
        }),
    );
    let polled = tick(&mut state, 3);
    let reread = answer(
        &mut state,
        &polled[0],
        Ok(ResponseResult::TaskStamp {
            stamp: "new".into(),
        }),
    );
    let list = reread
        .iter()
        .find(|request| matches!(request.method, Method::TaskList(_)))
        .expect("the list is read once the project exists");
    answer(&mut state, list, Ok(board()));
    assert!(screen(&mut state, 150).contains("▾ To Do 1"));
}

fn rich_board() -> ResponseResult {
    let mut tasks = Vec::new();
    let mut add = |id: &str,
                   title: &str,
                   status: &str,
                   kind: Option<&str>,
                   parent: Option<&str>,
                   prio: Option<&str>| {
        let mut t = task(id, title, status);
        t.task_type = kind.map(str::to_owned);
        t.parent_id = parent.map(str::to_owned);
        t.priority = prio.map(str::to_owned);
        t.acceptance_criteria_total = 0;
        if parent.is_some() {
            t.assignees.clear();
        }
        tasks.push(t);
    };
    add(
        "PAY-101",
        "Create Payment API",
        "To Do",
        Some("feature"),
        None,
        Some("high"),
    );
    for (i, t) in [
        "Design API schema",
        "Implement endpoint",
        "Add validation",
        "Write unit tests",
        "Load test",
    ]
    .iter()
    .enumerate()
    {
        add(
            &format!("PAY-101.{}", i + 1),
            t,
            "To Do",
            None,
            Some("PAY-101"),
            Some("medium"),
        );
    }
    add(
        "PAY-103",
        "Handle duplicate payments",
        "To Do",
        Some("bug"),
        None,
        Some("medium"),
    );
    add(
        "PAY-105",
        "Frontend Payment Form",
        "In Progress",
        Some("feature"),
        None,
        Some("high"),
    );
    add(
        "PAY-105.1",
        "Design form layout",
        "Done",
        None,
        Some("PAY-105"),
        Some("high"),
    );
    add(
        "PAY-105.2",
        "Implement form UI",
        "In Progress",
        None,
        Some("PAY-105"),
        Some("medium"),
    );
    add(
        "PAY-105.3",
        "Add form validation",
        "To Do",
        None,
        Some("PAY-105"),
        Some("medium"),
    );
    add(
        "PAY-107",
        "Payment Method Selection",
        "Review",
        Some("feature"),
        None,
        Some("medium"),
    );
    tasks[0].description_preview =
        Some("Implement the main payment creation endpoint with validation and idempotency".into());
    tasks[7].description_preview = Some("Implement payment creation UI".into());
    tasks[11].references = vec!["https://github.com/o/r/pull/42".into()];
    tasks[11].acceptance_criteria_total = 4;
    tasks[11].acceptance_criteria_checked = 3;
    ResponseResult::TaskList {
        board: TaskBoardInfo {
            root: "/repo".into(),
            statuses: vec![
                "To Do".into(),
                "In Progress".into(),
                "Review".into(),
                "Done".into(),
            ],
            tasks,
            stamp: String::new(),
        },
    }
}

fn rich() -> ClientShellState {
    let mut state = state();
    let opened = open_by_tab(&mut state);
    answer(&mut state, &opened[0], Ok(rich_board()));
    screen(&mut state, 160);
    state
}

#[test]
fn list_shows_subtasks_as_a_tree_under_their_parent_and_folds_them() {
    let mut state = rich();
    let text = screen(&mut state, 160);
    assert!(
        text.contains("▾ PAY-101      Create Payment API (5)"),
        "{text}"
    );
    assert!(text.contains("├─ PAY-101.1"), "{text}");
    assert!(text.contains("└─ PAY-101.5"), "{text}");
    // A subtask stays under its parent whatever its own status.
    assert!(text.contains("├─ PAY-105.1"), "{text}");
    assert!(
        text.contains("▾ To Do 2"),
        "only parents count in a group: {text}"
    );
    assert!(text.contains(" Feature "), "{text}");

    let fold = target(&state, &BoardTarget::FoldTask("PAY-101".into()));
    click(&mut state, fold);
    let text = screen(&mut state, 160);
    assert!(text.contains("▸ PAY-101"), "{text}");
    assert!(!text.contains("PAY-101.1"), "{text}");
}

#[test]
fn kanban_cards_carry_their_subtasks_and_pull_request() {
    let mut state = rich();
    let kanban = target(&state, &BoardTarget::Mode(BoardMode::Kanban));
    click(&mut state, kanban);
    let text = screen(&mut state, 160);
    assert!(
        text.contains("To Do  2 "),
        "subtasks are not cards of their own: {text}"
    );
    assert!(text.contains("FEATURE  PAY-101"), "{text}");
    assert!(text.contains("101.1  Design"), "{text}");
    assert!(text.contains("+1 more"), "{text}");
    assert!(text.contains("◫ 0/5"), "{text}");
    assert!(
        text.contains("Implement the main payment"),
        "the description shows: {text}"
    );
    assert!(text.contains("⎇ PR #42"), "{text}");

    // A subtask's pill moves it to the next status.
    let pill = target(&state, &BoardTarget::NextStatus("PAY-101.1".into()));
    let sent = requests(click(&mut state, pill).actions);
    let params = update_params(&sent);
    assert_eq!(params.id, "PAY-101.1");
    assert_eq!(params.status.as_deref(), Some("In Progress"));

    // The pull request opens in the browser.
    let link = target(
        &state,
        &BoardTarget::OpenUrl("https://github.com/o/r/pull/42".into()),
    );
    let opened = click(&mut state, link);
    assert!(opened.actions.iter().any(|action| matches!(action,
        ClientShellAction::OpenSafeWebUrl(url) if url == "https://github.com/o/r/pull/42")));
}

#[test]
fn a_task_shows_its_agents_live_state_and_goes_to_its_pane() {
    let mut state = state();
    let mut projected = snapshot();
    projected.agents = vec![ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("claude".into()),
        display_agent: None,
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Blocked,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }];
    state.set_snapshot(Box::new(projected));
    let opened = open_by_tab(&mut state);
    let list = opened
        .iter()
        .find(|request| matches!(&request.method, Method::TaskList(_)))
        .expect("the list is asked for");
    let ResponseResult::TaskList { mut board } = board() else {
        unreachable!()
    };
    board.tasks[1].pane_id = Some("pane_1".into());
    // A pane that has closed since shows no state.
    board.tasks[0].pane_id = Some("pane_gone".into());
    answer(&mut state, list, Ok(ResponseResult::TaskList { board }));
    screen(&mut state, 150);

    assert!(
        !state.hits.board_view.targets.iter().any(
            |(_, target)| matches!(target, BoardTarget::FocusPane(pane) if pane == "pane_gone")
        ),
        "a closed pane is not offered"
    );
    let dot = target(&state, &BoardTarget::FocusPane("pane_1".into()));
    let frame = state.compose(150, 34).expect("frame");
    let cell = &frame.cells[usize::from(dot.y) * usize::from(frame.width) + usize::from(dot.x)];
    assert_eq!(cell.symbol, "●");
    assert_eq!(
        cell.fg,
        crate::protocol::color_to_u32(state.config.palette.red),
        "a blocked agent's dot is red"
    );

    let sent = requests(click(&mut state, dot).actions);
    assert!(
        state.board_view.is_none(),
        "the Board gives way to the pane"
    );
    assert!(sent.iter().any(|request| matches!(&request.method,
        Method::PaneFocus(target) if target.pane_id == "pane_1")));
}
