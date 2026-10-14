use super::*;
use crate::api::schema::{
    GitChangeKind, GitCommitFile, GitCommitInfo, GitFileChange, GitFileDiff, GitHunk,
    GitStatusInfo, Method, ResponseResult,
};
use crate::client::shell::git_view::GitViewTarget;

fn open_view() -> (ClientShellState, Vec<crate::api::schema::Request>) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::GitView),
        &mut outcome,
    );
    assert!(outcome.repaint);
    (state, requests(outcome.actions))
}

fn requests(actions: Vec<ClientShellAction>) -> Vec<crate::api::schema::Request> {
    actions
        .into_iter()
        .filter_map(|action| match action {
            ClientShellAction::Endpoint { request, .. } => Some(*request),
            _ => None,
        })
        .collect()
}

fn answer(
    state: &mut ClientShellState,
    request: &crate::api::schema::Request,
    result: ResponseResult,
) -> Vec<crate::api::schema::Request> {
    let (repaint, actions) = state.handle_endpoint_result("boot-1", &request.id, Ok(result));
    assert!(repaint);
    requests(actions)
}

fn find(
    requests: &[crate::api::schema::Request],
    matches: impl Fn(&Method) -> bool,
) -> &crate::api::schema::Request {
    requests
        .iter()
        .find(|request| matches(&request.method))
        .unwrap_or_else(|| panic!("no such request in {requests:#?}"))
}

fn screen(state: &mut ClientShellState) -> String {
    let frame = state.compose(150, 34).expect("frame");
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

fn file(path: &str, kind: GitChangeKind, staged: bool, unstaged: bool) -> GitFileChange {
    GitFileChange {
        path: path.into(),
        orig_path: None,
        kind,
        staged,
        unstaged,
    }
}

fn status(files: Vec<GitFileChange>, head: &str) -> ResponseResult {
    ResponseResult::GitStatus {
        status: GitStatusInfo {
            root: "/repo".into(),
            branch: Some("main".into()),
            head: Some(head.into()),
            upstream: Some("origin/main".into()),
            ahead: 1,
            behind: 2,
            files,
            truncated: false,
            operation: None,
        },
    }
}

/// A status reporting background operation `id`, still running or finished.
fn with_operation(
    result: ResponseResult,
    id: u64,
    kind: crate::api::schema::GitOperationKind,
    running: bool,
    error: Option<&str>,
) -> ResponseResult {
    let ResponseResult::GitStatus { mut status } = result else {
        unreachable!("a status");
    };
    status.operation = Some(crate::api::schema::GitOperationInfo {
        id,
        kind,
        running,
        error: error.map(str::to_owned),
    });
    ResponseResult::GitStatus { status }
}

/// Polls the status (as the timer would) and answers it.
fn poll(state: &mut ClientShellState, result: ResponseResult) -> Vec<crate::api::schema::Request> {
    let polled = requests(
        state
            .tick_git_view(std::time::Instant::now() + std::time::Duration::from_secs(5))
            .actions,
    );
    let status_request = find(&polled, |method| matches!(method, Method::GitStatus(_))).clone();
    answer(state, &status_request, result)
}

fn two_files() -> Vec<GitFileChange> {
    vec![
        file("src/app.rs", GitChangeKind::Modified, true, false),
        file("notes.md", GitChangeKind::Untracked, false, true),
    ]
}

fn commit(id: &str, subject: &str, pushed: bool) -> GitCommitInfo {
    GitCommitInfo {
        commit: id.into(),
        short_commit: id[..4].into(),
        author: "Ada".into(),
        time: 1_700_000_000,
        subject: subject.into(),
        body: String::new(),
        merge: false,
        pushed,
        files_changed: 1,
        insertions: 2,
        deletions: 1,
    }
}

/// Opens the view and answers its status, history and first diff like a server would.
fn loaded_view() -> (ClientShellState, Vec<crate::api::schema::Request>) {
    let (mut state, opened) = open_view();
    let status_request = find(&opened, |method| matches!(method, Method::GitStatus(_)));
    let follow_up = answer(&mut state, status_request, status(two_files(), "aaaa1111"));
    let diff_request = find(&follow_up, |method| matches!(method, Method::GitDiff(_)));
    let mut rest = answer(
        &mut state,
        diff_request,
        ResponseResult::GitDiff {
            diff: GitFileDiff::default(),
        },
    );
    let log_request = find(&opened, |method| matches!(method, Method::GitLog(_)));
    rest.extend(answer(
        &mut state,
        log_request,
        ResponseResult::GitLog {
            commits: vec![commit("aaaa1111", "fix: first", false)],
            has_more: false,
        },
    ));
    (state, rest)
}

#[test]
fn opening_reads_the_focused_space_and_shows_its_changes_and_diff() {
    let (mut state, opened) = open_view();
    let status_request = find(
        &opened,
        |method| matches!(method, Method::GitStatus(target) if target.workspace_id.as_deref() == Some("ws_1")),
    );
    find(&opened, |method| matches!(method, Method::GitLog(_)));
    assert!(screen(&mut state).contains("loading"));

    let follow_up = answer(&mut state, status_request, status(two_files(), "aaaa1111"));
    // The first file is selected and its diff asked for.
    let diff_request = find(
        &follow_up,
        |method| matches!(method, Method::GitDiff(params) if params.path == "src/app.rs" && params.commit.is_none()),
    );
    let text = screen(&mut state);
    assert!(text.contains("git · client-shell"), "{text}");
    assert!(text.contains("⎇ main"), "{text}");
    assert!(text.contains("Changes 2"), "{text}");
    assert!(
        text.contains("app.rs") && text.contains("notes.md"),
        "{text}"
    );
    assert!(text.contains("✓ Commit 1 file to main"), "{text}");
    assert!(
        text.contains("± git · client-shell 2"),
        "the tab bar names the view: {text}"
    );
    assert!(
        !text.contains("LIVE"),
        "the panes are hidden behind the view"
    );

    answer(
        &mut state,
        diff_request,
        ResponseResult::GitDiff {
            diff: GitFileDiff {
                path: "src/app.rs".into(),
                staged_hunks: vec![GitHunk {
                    header: "@@ -1 +1 @@ fn main()".into(),
                    old_start: 1,
                    new_start: 1,
                    lines: vec!["-old line".into(), "+new line".into()],
                }],
                ..GitFileDiff::default()
            },
        },
    );
    let text = screen(&mut state);
    assert!(text.contains("@@ -1 +1 @@ fn main()"), "{text}");
    assert!(
        text.contains("new line") && text.contains("old line"),
        "{text}"
    );
    assert!(text.contains("− unstage hunk"), "{text}");
}

#[test]
fn staging_is_one_request_at_a_time_and_discard_asks_twice() {
    let (mut state, _) = loaded_view();
    // Down to the untracked file, then stage it.
    state.handle_input_bytes(b"j");
    let staged = requests(state.handle_input_bytes(b" ").actions);
    let stage = find(
        &staged,
        |method| matches!(method, Method::GitStage(params) if params.paths == ["notes.md"] && params.hunk.is_none()),
    );
    assert!(
        requests(state.handle_input_bytes(b" ").actions).is_empty(),
        "no second operation while one runs"
    );
    answer(
        &mut state,
        stage,
        status(
            vec![
                file("src/app.rs", GitChangeKind::Modified, true, false),
                file("notes.md", GitChangeKind::Added, true, false),
            ],
            "aaaa1111",
        ),
    );
    let text = screen(&mut state);
    assert!(text.contains("✓ Commit 2 files to main"), "{text}");

    // A fully staged file unstages; a new change can be discarded after a second press.
    let unstage = requests(state.handle_input_bytes(b"s").actions);
    let unstage = find(&unstage, |method| matches!(method, Method::GitUnstage(_)));
    answer(&mut state, unstage, status(two_files(), "aaaa1111"));
    assert!(requests(state.handle_input_bytes(b"d").actions).is_empty());
    assert!(screen(&mut state).contains("again to delete notes.md"));
    let discarded = requests(state.handle_input_bytes(b"d").actions);
    find(
        &discarded,
        |method| matches!(method, Method::GitDiscard(params) if params.paths == ["notes.md"]),
    );
}

#[test]
fn commit_needs_a_summary_then_offers_undo_and_can_push_after() {
    let (mut state, _) = loaded_view();
    state.handle_input_bytes(b"c");
    assert!(
        requests(state.handle_input_bytes(b"\r").actions).is_empty(),
        "no commit without a summary"
    );
    assert!(screen(&mut state).contains("write a summary first"));
    state.handle_input_bytes(b"fix: thing");
    let committed = requests(state.handle_input_bytes(b"\r").actions);
    let commit_request = find(
        &committed,
        |method| matches!(method, Method::GitCommit(params) if params.message == "fix: thing" && !params.all),
    );
    use crate::api::schema::GitOperationKind::{Commit, Push};
    // The server answers at once with the commit running; the view polls until it ends.
    answer(
        &mut state,
        commit_request,
        with_operation(status(two_files(), "aaaa1111"), 7, Commit, true, None),
    );
    let text = screen(&mut state);
    assert!(text.contains("committing…"), "{text}");
    assert!(
        requests(state.handle_input_bytes(b"c").actions).is_empty()
            && requests(state.handle_input_bytes(b"\r").actions).is_empty(),
        "no second commit while one runs"
    );
    state.handle_input_bytes(b"\x1b");
    poll(
        &mut state,
        with_operation(status(Vec::new(), "bbbb2222"), 7, Commit, false, None),
    );
    let text = screen(&mut state);
    assert!(text.contains("↶ undo"), "{text}");
    assert!(text.contains("nothing to commit"), "{text}");

    let undone = requests(state.handle_input_bytes(b"u").actions);
    let uncommit = find(
        &undone,
        |method| matches!(method, Method::GitUncommit(params) if params.commit == "bbbb2222"),
    );
    answer(&mut state, uncommit, status(two_files(), "aaaa1111"));
    let view = state.git_view.as_ref().expect("view");
    assert_eq!(
        view.summary.as_str(),
        "fix: thing",
        "the message comes back"
    );

    // alt+Enter commits, then pushes once the commit has finished.
    state.handle_input_bytes(b"c");
    let committed = requests(state.handle_input_bytes(b"\x1b\r").actions);
    let commit_request = find(&committed, |method| matches!(method, Method::GitCommit(_)));
    let after = answer(
        &mut state,
        commit_request,
        with_operation(status(two_files(), "aaaa1111"), 8, Commit, true, None),
    );
    assert!(!after
        .iter()
        .any(|request| matches!(request.method, Method::GitPush(_))));
    let after = poll(
        &mut state,
        with_operation(status(Vec::new(), "cccc3333"), 8, Commit, false, None),
    );
    let push_request = find(&after, |method| matches!(method, Method::GitPush(_))).clone();
    answer(
        &mut state,
        &push_request,
        with_operation(status(Vec::new(), "cccc3333"), 9, Push, true, None),
    );
    assert!(screen(&mut state).contains("pushing…"));
}

#[test]
fn a_failed_or_lost_operation_frees_the_view_and_says_why() {
    use crate::api::schema::GitOperationKind::Push;
    let (mut state, _) = loaded_view();
    let pushed = requests(state.handle_input_bytes(b"p").actions);
    let push_request = find(
        &pushed,
        |method| matches!(method, Method::GitPush(target) if target.root.as_deref() == Some("/repo")),
    );
    answer(
        &mut state,
        push_request,
        with_operation(status(two_files(), "aaaa1111"), 3, Push, true, None),
    );
    poll(
        &mut state,
        with_operation(
            status(two_files(), "aaaa1111"),
            3,
            Push,
            false,
            Some("! [rejected] main -> main (fetch first)"),
        ),
    );
    let text = screen(&mut state);
    assert!(text.contains("[rejected]"), "{text}");
    assert!(state
        .git_view
        .as_ref()
        .is_some_and(|view| view.busy.is_none()));

    // A server that forgot the operation (it restarted) does not leave the view stuck.
    let pushed = requests(state.handle_input_bytes(b"p").actions);
    let push_request = find(&pushed, |method| matches!(method, Method::GitPush(_)));
    answer(
        &mut state,
        push_request,
        with_operation(status(two_files(), "aaaa1111"), 4, Push, true, None),
    );
    poll(&mut state, status(two_files(), "aaaa1111"));
    assert!(screen(&mut state).contains("lost track of the operation"));
    assert!(state
        .git_view
        .as_ref()
        .is_some_and(|view| view.busy.is_none()));
}

#[test]
fn history_lists_commits_then_a_commits_files_and_one_files_diff() {
    let (mut state, _) = loaded_view();
    let switched = requests(state.handle_input_bytes(b"\t").actions);
    assert_eq!(
        state.git_view.as_ref().map(|view| view.tab),
        Some(crate::client::shell::git_view::GitViewTab::History),
        "{switched:#?}"
    );
    let show = find(
        &switched,
        |method| matches!(method, Method::GitShow(params) if params.commit == "aaaa1111"),
    );
    let follow_up = answer(
        &mut state,
        show,
        ResponseResult::GitCommitFiles {
            commit: "aaaa1111".into(),
            files: vec![GitCommitFile {
                path: "src/app.rs".into(),
                orig_path: None,
                kind: GitChangeKind::Modified,
                insertions: 2,
                deletions: 1,
                binary: false,
            }],
        },
    );
    find(
        &follow_up,
        |method| matches!(method, Method::GitDiff(params) if params.commit.as_deref() == Some("aaaa1111")),
    );
    let text = screen(&mut state);
    assert!(text.contains("fix: first"), "{text}");
    assert!(text.contains("↑ not pushed"), "{text}");
    assert!(text.contains("Ada"), "{text}");
    assert!(text.contains("History ↑1"), "{text}");
}

#[test]
fn the_view_follows_the_focused_space_and_drops_stale_answers() {
    let (mut state, opened) = open_view();
    let stale = find(&opened, |method| matches!(method, Method::GitStatus(_))).clone();
    let mut moved = snapshot();
    moved.revision = 2;
    moved.workspaces.push(ClientShellWorkspace {
        workspace_id: "ws_2".into(),
        label: "other".into(),
        focused: true,
        ..moved.workspaces[0].clone()
    });
    moved.workspaces[0].focused = false;
    moved.tabs.push(ClientShellTab {
        tab_id: "tab_2".into(),
        workspace_id: "ws_2".into(),
        ..moved.tabs[0].clone()
    });
    moved.focused_workspace_id = Some("ws_2".into());
    moved.focused_tab_id = Some("tab_2".into());
    state.set_snapshot(Box::new(moved.clone()));
    assert_eq!(
        state
            .git_view
            .as_ref()
            .map(|view| view.workspace_id.as_str()),
        Some("ws_2")
    );

    // The old space's answer arrives late and changes nothing.
    let (_, actions) =
        state.handle_endpoint_result("boot-1", &stale.id, Ok(status(two_files(), "aaaa1111")));
    assert!(actions.is_empty());
    assert!(state
        .git_view
        .as_ref()
        .is_some_and(|view| view.status.is_none()));
    let ticked = requests(state.tick_git_view(std::time::Instant::now()).actions);
    find(
        &ticked,
        |method| matches!(method, Method::GitStatus(target) if target.workspace_id.as_deref() == Some("ws_2")),
    );

    // Choosing another of the space's tabs brings its panes back.
    let mut other_tab = moved;
    other_tab.revision = 3;
    other_tab.tabs.push(ClientShellTab {
        tab_id: "tab_3".into(),
        workspace_id: "ws_2".into(),
        ..other_tab.tabs[0].clone()
    });
    other_tab.focused_tab_id = Some("tab_3".into());
    state.set_snapshot(Box::new(other_tab));
    assert!(state.git_view.is_none());
}

#[test]
fn keys_and_text_never_reach_the_hidden_pane() {
    let (mut state, _) = loaded_view();
    for input in [&b"x"[..], b"q", b"\x1b[1;5A"] {
        let outcome = state.handle_input_bytes(input);
        assert!(
            !outcome
                .requests
                .iter()
                .any(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. })),
            "{input:?} reached the pane"
        );
    }
    // Esc closes the view; keys reach the pane again.
    state.handle_input_bytes(b"\x1b");
    assert!(state.git_view.is_none());
    let typed = state.handle_input_bytes(b"x");
    assert!(typed
        .requests
        .iter()
        .any(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. })));
}

#[test]
fn clicks_select_files_and_toggle_their_checkbox() {
    let (mut state, _) = loaded_view();
    screen(&mut state);
    let target = |state: &ClientShellState, wanted: GitViewTarget| {
        state
            .hits
            .git_view
            .targets
            .iter()
            .find(|(_, target)| *target == wanted)
            .map(|(rect, _)| *rect)
            .unwrap_or_else(|| panic!("{wanted:?} not on screen"))
    };
    let click = |state: &mut ClientShellState, rect: Rect| {
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::empty(),
        })])
    };
    let row = target(&state, GitViewTarget::File(1));
    click(&mut state, Rect::new(row.x + 5, row.y, 1, 1));
    assert_eq!(
        state
            .git_view
            .as_ref()
            .and_then(|view| view.selected_file.as_deref()),
        Some("notes.md")
    );
    let checkbox = target(&state, GitViewTarget::FileBox(0));
    let toggled = requests(click(&mut state, checkbox).actions);
    find(
        &toggled,
        |method| matches!(method, Method::GitUnstage(params) if params.paths == ["src/app.rs"]),
    );
}

#[test]
fn a_server_without_git_methods_says_so_instead_of_loading() {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state.set_endpoint_methods(Some(vec!["workspace.focus".into()]));
    let mut outcome = ClientShellInput::default();
    state.open_git_view(None, &mut outcome);
    assert!(requests(outcome.actions).is_empty());
    let text = screen(&mut state);
    assert!(text.contains("no git support"), "{text}");
}

#[test]
fn the_sidebar_badge_shows_uncommitted_files_and_opens_the_view() {
    let mut config = Config::default();
    config.ui.sidebar.agents.nested = true;
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let mut projected = snapshot();
    projected.workspaces[0].git_changes = Some(3);
    projected.workspaces[0].git_ahead_behind = Some((1, 0));
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    let text = screen(&mut state);
    // A narrow sidebar drops the branch name first, keeping the counts.
    assert!(text.contains("↑1 ±3"), "{text}");
    let (badge, workspace_id) = state
        .hits
        .space_git_badges
        .first()
        .cloned()
        .expect("badge hit");
    assert_eq!(workspace_id, "ws_1");
    let opened =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: badge.x + 1,
            row: badge.y,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(state.git_view.is_some());
    find(&requests(opened.actions), |method| {
        matches!(method, Method::GitStatus(_))
    });
}

#[test]
fn a_new_commit_takes_the_selection_only_from_the_newest_commit() {
    let (mut state, _) = loaded_view();
    let log = |commits: Vec<GitCommitInfo>| ResponseResult::GitLog {
        commits,
        has_more: false,
    };
    // Refresh the history with a new commit on top: the newest stays selected.
    state.set_git_view_tab(
        crate::client::shell::git_view::GitViewTab::History,
        &mut ClientShellInput::default(),
    );
    state.refresh_git_view(&mut ClientShellInput::default());
    let pending = state
        .pending_requests
        .iter()
        .find(|(_, pending)| {
            matches!(
                pending.kind,
                PendingEndpointKind::Git {
                    request: crate::client::shell::git_view::GitRequest::Log,
                    ..
                }
            )
        })
        .map(|(id, _)| id.clone())
        .expect("log request");
    state.handle_endpoint_result(
        "boot-1",
        &pending,
        Ok(log(vec![
            commit("cccc3333", "feat: newer", false),
            commit("aaaa1111", "fix: first", false),
        ])),
    );
    let view = state.git_view.as_ref().expect("view");
    assert_eq!(view.selected_commit.as_deref(), Some("cccc3333"));

    // A commit picked further down keeps its place.
    let mut outcome = ClientShellInput::default();
    state.select_git_commit(1, &mut outcome);
    state.refresh_git_view(&mut outcome);
    let pending = state
        .pending_requests
        .iter()
        .find(|(_, pending)| {
            matches!(
                pending.kind,
                PendingEndpointKind::Git {
                    request: crate::client::shell::git_view::GitRequest::Log,
                    ..
                }
            )
        })
        .map(|(id, _)| id.clone())
        .expect("log request");
    state.handle_endpoint_result(
        "boot-1",
        &pending,
        Ok(log(vec![
            commit("dddd4444", "feat: newest", false),
            commit("cccc3333", "feat: newer", false),
            commit("aaaa1111", "fix: first", false),
        ])),
    );
    let view = state.git_view.as_ref().expect("view");
    assert_eq!(view.selected_commit.as_deref(), Some("aaaa1111"));
}

#[test]
fn plain_keys_sent_as_text_still_drive_the_list() {
    let (mut state, _) = loaded_view();
    // A host reporting every key (for an agent that asked for it) sends "j" as text.
    let outcome = state.handle_raw_events(vec![RawInputEvent::Text(
        crate::input::TextCommit::new("j".to_owned()),
    )]);
    assert!(outcome.requests.is_empty());
    assert_eq!(
        state
            .git_view
            .as_ref()
            .and_then(|view| view.selected_file.as_deref()),
        Some("notes.md")
    );
    let staged = requests(
        state
            .handle_raw_events(vec![RawInputEvent::Text(crate::input::TextCommit::new(
                " ".to_owned(),
            ))])
            .actions,
    );
    find(&staged, |method| matches!(method, Method::GitStage(_)));
    assert!(
        !state.focused_pane_receives_keys(),
        "the hidden pane's keyboard mode is released while the view is open"
    );
}

#[test]
fn the_view_takes_keys_even_over_a_pane_in_copy_mode() {
    let (mut state, _) = loaded_view();
    state.mode = ClientShellMode::Copy;
    state.handle_input_bytes(b"j");
    assert_eq!(
        state
            .git_view
            .as_ref()
            .and_then(|view| view.selected_file.as_deref()),
        Some("notes.md")
    );
}

#[test]
fn a_running_operation_is_followed_even_with_the_window_in_the_background() {
    use crate::api::schema::GitOperationKind::Push;
    let (mut state, _) = loaded_view();
    let pushed = requests(state.handle_input_bytes(b"p").actions);
    let push_request = find(&pushed, |method| matches!(method, Method::GitPush(_)));
    answer(
        &mut state,
        push_request,
        with_operation(status(two_files(), "aaaa1111"), 5, Push, true, None),
    );
    state.outer_focused = Some(false);
    poll(
        &mut state,
        with_operation(status(two_files(), "aaaa1111"), 5, Push, false, None),
    );
    assert!(state
        .git_view
        .as_ref()
        .is_some_and(|view| view.busy.is_none()));
    // With nothing running, the routine refresh waits for the window to come back.
    let idle = state.tick_git_view(std::time::Instant::now() + std::time::Duration::from_secs(10));
    assert!(requests(idle.actions).is_empty());
}
