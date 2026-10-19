use super::*;
use crate::api::schema::{Method, NoteFile, NoteVault, ResponseResult};
use crate::client::shell::notes_markdown::DocTarget;
use crate::client::shell::notes_view::NotesTarget;
use crate::input::TerminalKey;

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

fn click_at(state: &mut ClientShellState, at: (u16, u16)) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: at.0,
        row: at.1,
        modifiers: KeyModifiers::empty(),
    })])
}

fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    click_at(state, (rect.x + 1, rect.y))
}

fn key(state: &mut ClientShellState, code: KeyCode, modifiers: KeyModifiers) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(TerminalKey::new(code, modifiers))])
}

fn paste(state: &mut ClientShellState, text: &str) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Paste(text.into())])
}

fn target(state: &ClientShellState, wanted: &NotesTarget) -> Rect {
    state
        .hits
        .notes_view
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

fn screen(state: &mut ClientShellState, width: u16, height: u16) -> String {
    let frame = state.compose(width, height).expect("frame");
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

fn tick(state: &mut ClientShellState, millis: u64) -> Vec<crate::api::schema::Request> {
    requests(
        state
            .tick_notes_view(std::time::Instant::now() + std::time::Duration::from_millis(millis))
            .actions,
    )
}

fn note(path: &str, content: &str) -> NoteFile {
    NoteFile {
        path: path.into(),
        content: content.into(),
        fingerprint: format!("fp-{path}"),
        modified_ms: 0,
    }
}

fn vault(notes: Vec<NoteFile>) -> NoteVault {
    NoteVault {
        root: "/home/me/.local/share/herdr/notes/herdr".into(),
        project: "herdr".into(),
        stamp: "s1".into(),
        notes,
        truncated: false,
    }
}

fn sample() -> Vec<NoteFile> {
    vec![
        note(
            "scratch.md",
            "# Scratch\n\n- [ ] try [[File tree]] variant B\n- [x] ship board cards\n- idea: notes per #project\n\nsee [[Plan]] and @src/ui.rs",
        ),
        note(
            "File tree.md",
            "# File tree\n\nTerax-style explorer, see [[scratch]].\n\n## Open questions\n- [ ] root follows the pane?\n\n> Terax deletes for good.\n\n#feature #ui",
        ),
        note("daily/2026-10-19.md", "# 2026-10-19\n\n- started [[File tree]]"),
    ]
}

/// Opens Notes by its tab and answers the list with `notes`.
fn opened(notes: Vec<NoteFile>) -> ClientShellState {
    let mut state = state();
    screen(&mut state, 160, 40);
    let tab = state.hits.notes_tab;
    assert!(tab.width > 0, "the Notes tab is in the tab bar");
    let sent = requests(click(&mut state, tab).actions);
    assert_eq!(sent.len(), 1);
    assert!(matches!(sent[0].method, Method::NoteList(_)));
    answer(
        &mut state,
        &sent[0],
        Ok(ResponseResult::NoteList {
            vault: vault(notes),
        }),
    );
    state
}

fn write_of(request: &crate::api::schema::Request) -> &crate::api::schema::NoteWriteParams {
    match &request.method {
        Method::NoteWrite(params) => params,
        other => panic!("expected a note write, got {other:?}"),
    }
}

fn saved(path: &str, fingerprint: &str) -> Result<ResponseResult, ClientShellEndpointError> {
    Ok(ResponseResult::NoteSaved {
        note: NoteFile {
            path: path.into(),
            content: String::new(),
            fingerprint: fingerprint.into(),
            modified_ms: 1,
        },
        stamp: "s2".into(),
    })
}

#[test]
fn the_tab_opens_on_the_scratch_note_with_list_tags_and_backlinks() {
    let mut state = opened(sample());
    let shown = screen(&mut state, 160, 40);
    if std::env::var_os("HERDR_PRINT_NOTES").is_some() {
        eprintln!("{shown}");
    }
    for expected in [
        "✎ Notes",
        "3 notes",
        "/.local/share/herdr/notes/herdr",
        "NOTES",
        "daily/",
        "TAGS",
        "#feature",
        "Scratch",
        "☐ try File tree variant B",
        "☑ ship board cards",
        "BACKLINKS  1",
        "← File tree",
        "OUTLINE",
    ] {
        assert!(
            shown.contains(expected),
            "{expected:?} missing from:\n{shown}"
        );
    }
    // Read mode hides the syntax.
    assert!(!shown.contains("[[File tree]]"));
    key(&mut state, KeyCode::Enter, KeyModifiers::empty());
    let editing = screen(&mut state, 160, 40);
    assert!(
        editing.contains("- [ ] try [[File tree]] variant B"),
        "{editing}"
    );
    key(&mut state, KeyCode::Char('o'), KeyModifiers::CONTROL);
    paste(&mut state, "fi");
    let switcher = screen(&mut state, 160, 40);
    assert!(switcher.contains("Find a note"), "{switcher}");
    if std::env::var_os("HERDR_PRINT_NOTES").is_some() {
        eprintln!("{editing}\n{switcher}");
    }
}

#[test]
fn typing_saves_after_a_pause_and_a_conflict_asks_what_to_keep() {
    let mut state = opened(sample());
    screen(&mut state, 160, 40);
    key(&mut state, KeyCode::Enter, KeyModifiers::empty());
    assert!(state
        .notes_view
        .as_ref()
        .is_some_and(|view| view.editor.is_some()));
    paste(&mut state, "hello ");
    assert!(tick(&mut state, 100).is_empty(), "no save while typing");
    let sent = tick(&mut state, 1000);
    assert_eq!(sent.len(), 1);
    let write = write_of(&sent[0]);
    assert_eq!(write.path, "scratch.md");
    // Writing from the keyboard carries on at the end of the note.
    assert!(
        write.content.ends_with("@src/ui.rshello "),
        "{}",
        write.content
    );
    assert_eq!(write.base_fingerprint.as_deref(), Some("fp-scratch.md"));
    answer(&mut state, &sent[0], saved("scratch.md", "fp2"));
    assert!(screen(&mut state, 160, 40).contains("✓ saved"));

    paste(&mut state, "again ");
    let sent = tick(&mut state, 1000);
    assert_eq!(write_of(&sent[0]).base_fingerprint.as_deref(), Some("fp2"));
    answer(
        &mut state,
        &sent[0],
        Err(ClientShellEndpointError {
            code: Some("note_conflict".into()),
            message: "changed".into(),
        }),
    );
    let shown = screen(&mut state, 160, 40);
    assert!(shown.contains("Changed on disk"), "{shown}");
    assert!(shown.contains("Keep mine"));
}

#[test]
fn a_link_to_a_missing_note_creates_it_and_opens_it_for_writing() {
    let mut state = opened(sample());
    screen(&mut state, 160, 40);
    let doc = state.hits.notes_view.doc;
    let (row, x) = {
        let view = state.notes_view.as_ref().expect("notes open");
        view.doc_rows
            .iter()
            .enumerate()
            .find_map(|(index, row)| {
                let mut x = 0u16;
                for piece in &row.pieces {
                    if piece.target == Some(DocTarget::Link("Plan".into())) {
                        return Some((index, x));
                    }
                    x += unicode_width::UnicodeWidthStr::width(piece.text.as_str()) as u16;
                }
                None
            })
            .expect("the Plan link")
    };
    let sent = requests(click_at(&mut state, (doc.x + x, doc.y + row as u16)).actions);
    let write = write_of(&sent[0]);
    assert_eq!(write.path, "Plan.md");
    assert_eq!(write.base_fingerprint, None);
    assert_eq!(write.content, "# Plan\n\n");
    answer(&mut state, &sent[0], saved("Plan.md", "fp-plan"));
    let view = state.notes_view.as_ref().expect("notes open");
    assert_eq!(view.open.as_deref(), Some("Plan.md"));
    assert!(view.editor.is_some());
}

#[test]
fn clicking_a_box_ticks_it_and_saves() {
    let mut state = opened(sample());
    screen(&mut state, 160, 40);
    let doc = state.hits.notes_view.doc;
    let row = state
        .notes_view
        .as_ref()
        .and_then(|view| {
            view.doc_rows.iter().position(|row| {
                row.pieces
                    .first()
                    .is_some_and(|piece| piece.target == Some(DocTarget::Checkbox(2)))
            })
        })
        .expect("the first task");
    let sent = requests(click_at(&mut state, (doc.x, doc.y + row as u16)).actions);
    assert!(write_of(&sent[0])
        .content
        .contains("- [x] try [[File tree]] variant B"));
}

#[test]
fn the_switcher_finds_a_note_and_rename_sends_the_new_path() {
    let mut state = opened(sample());
    screen(&mut state, 160, 40);
    key(&mut state, KeyCode::Char('o'), KeyModifiers::CONTROL);
    paste(&mut state, "ftr");
    let shown = screen(&mut state, 160, 40);
    assert!(shown.contains("Find a note"), "{shown}");
    key(&mut state, KeyCode::Enter, KeyModifiers::empty());
    assert_eq!(
        state
            .notes_view
            .as_ref()
            .and_then(|view| view.open.clone())
            .as_deref(),
        Some("File tree.md")
    );
    screen(&mut state, 160, 40);
    let rename = target(&state, &NotesTarget::Rename);
    click(&mut state, rename);
    for _ in 0.."File tree".len() {
        key(&mut state, KeyCode::Backspace, KeyModifiers::empty());
    }
    paste(&mut state, "ideas/Explorer");
    let sent = requests(key(&mut state, KeyCode::Enter, KeyModifiers::empty()).actions);
    match &sent[0].method {
        Method::NoteRename(params) => {
            assert_eq!(params.from, "File tree.md");
            assert_eq!(params.to, "ideas/Explorer.md");
        }
        other => panic!("expected a rename, got {other:?}"),
    }
}

#[test]
fn an_empty_project_explains_where_notes_live() {
    let mut state = opened(Vec::new());
    let shown = screen(&mut state, 160, 40);
    assert!(shown.contains("No notes for herdr yet"), "{shown}");
    assert!(shown.contains("Obsidian"));
    let new = target(&state, &NotesTarget::New);
    let sent = requests(click(&mut state, new).actions);
    assert!(sent.is_empty(), "New asks for a name first");
    assert!(screen(&mut state, 160, 40).contains("New note"));
}

#[test]
fn a_narrow_window_drops_the_side_columns_first() {
    let mut state = opened(sample());
    let wide = screen(&mut state, 160, 40);
    assert!(wide.contains("BACKLINKS") && wide.contains("NOTES"));
    let medium = screen(&mut state, 110, 40);
    assert!(
        !medium.contains("BACKLINKS") && medium.contains("NOTES"),
        "{medium}"
    );
    let narrow = screen(&mut state, 80, 40);
    assert!(
        !narrow.contains("NOTES") && narrow.contains("Scratch"),
        "{narrow}"
    );
}

#[test]
fn a_rename_waits_for_the_save_on_its_way() {
    let mut state = opened(sample());
    screen(&mut state, 160, 40);
    key(&mut state, KeyCode::Enter, KeyModifiers::empty());
    paste(&mut state, "more");
    let save = tick(&mut state, 1000);
    assert_eq!(save.len(), 1, "the save is on its way");
    // Back to reading, then rename.
    key(&mut state, KeyCode::Esc, KeyModifiers::empty());
    key(&mut state, KeyCode::Char('r'), KeyModifiers::empty());
    for _ in 0.."scratch".len() {
        key(&mut state, KeyCode::Backspace, KeyModifiers::empty());
    }
    paste(&mut state, "inbox");
    let sent = requests(key(&mut state, KeyCode::Enter, KeyModifiers::empty()).actions);
    assert!(
        sent.iter()
            .all(|request| !matches!(request.method, Method::NoteRename(_))),
        "no rename while the save is out"
    );
    let after = answer(&mut state, &save[0], saved("scratch.md", "fp2"));
    assert!(after.iter().any(|request| matches!(
        &request.method,
        Method::NoteRename(params) if params.from == "scratch.md" && params.to == "inbox.md"
    )));
}

#[test]
fn a_pasted_image_is_saved_and_embedded_at_the_cursor() {
    let mut state = opened(sample());
    screen(&mut state, 160, 40);
    key(&mut state, KeyCode::Enter, KeyModifiers::empty());
    assert!(state.notes_editor_focused());
    let mut outcome = ClientShellInput::default();
    let png = b"\x89PNG\r\n\x1a\nimage".to_vec();
    state.paste_into_note(
        None,
        || {
            Some(crate::platform::ClipboardImage {
                bytes: png.clone(),
                extension: "png",
            })
        },
        &mut outcome,
    );
    let sent = requests(outcome.actions);
    let params = match &sent[0].method {
        Method::NoteAttach(params) => params,
        other => panic!("expected an attach, got {other:?}"),
    };
    assert_eq!(params.extension, "png");
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&params.data)
            .expect("base64"),
        png
    );
    answer(
        &mut state,
        &sent[0],
        Ok(ResponseResult::NoteAttached {
            path: "attachments/Pasted image 20261019120000.png".into(),
        }),
    );
    let text = state
        .notes_view
        .as_ref()
        .and_then(|view| view.open_text())
        .expect("note text");
    assert!(
        text.ends_with("@src/ui.rs ![[attachments/Pasted image 20261019120000.png]]"),
        "{text}"
    );
    // Back in read mode it shows as the image's name.
    key(&mut state, KeyCode::Esc, KeyModifiers::empty());
    assert!(screen(&mut state, 160, 40).contains("▣ Pasted image 20261019120000.png"));
}

#[test]
fn clipboard_text_wins_over_an_image() {
    let mut state = opened(sample());
    screen(&mut state, 160, 40);
    key(&mut state, KeyCode::Enter, KeyModifiers::empty());
    let mut outcome = ClientShellInput::default();
    state.paste_into_note(
        Some("words".into()),
        || panic!("the image is not read when there is text"),
        &mut outcome,
    );
    assert!(requests(outcome.actions).is_empty());
    assert!(state
        .notes_view
        .as_ref()
        .and_then(|view| view.open_text())
        .is_some_and(|text| text.ends_with("words")));
}
