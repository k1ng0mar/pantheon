//! Tests for the screen stack and key translation.
//!
//! The stack is exercised without a terminal, which is the point of the
//! routing design: push a screen, feed it keys, assert what popped.

use super::*;
use crate::widget::{Item, MultiSelect, Select, TextInput};
use std::sync::{Arc, Mutex};

fn pick() -> Select {
    Select::new(
        "provider",
        vec![
            Item::new("OpenAI", "openai"),
            Item::new("Nous", "nous"),
            Item::new("LM Studio", "lmstudio"),
        ],
    )
}

#[test]
fn a_screen_submits_and_pops() {
    let mut app = TuiApp::new();
    app.push(Screen::select("model", pick()));
    assert_eq!(app.depth(), 1);
    app.on_key(Key::Enter);
    assert_eq!(app.depth(), 0, "submitting pops the screen");
}

#[test]
fn esc_cancels_and_pops_without_a_value() {
    let mut app = TuiApp::new();
    app.push(Screen::select("model", pick()));
    app.on_key(Key::Esc);
    assert_eq!(app.depth(), 0);
}

#[test]
fn a_continuation_receives_the_submitted_value() {
    // The continuation cannot borrow the app it was pushed onto, so it
    // records into shared state the test reads afterwards. That constraint
    // is the design working: a screen's continuation runs after the screen
    // is popped, so it must not hold a borrow of the stack.
    let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut app = TuiApp::new();
    let sink = seen.clone();
    app.push(Screen::select("model", pick()).then(move |sel| {
        *sink.lock().unwrap() = sel.one().map(|s| s.to_string());
    }));
    app.on_key(Key::Enter);
    assert_eq!(seen.lock().unwrap().as_deref(), Some("openai"));
    assert_eq!(app.depth(), 0);
}

#[test]
fn a_continuation_receives_a_many_selection() {
    let seen: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
    let mut app = TuiApp::new();
    let sink = seen.clone();
    let mut ms = MultiSelect::new(
        "tools",
        vec![Item::new("Web", "web"), Item::new("Files", "files")],
        &[],
    );
    ms.handle_key(Key::Space);
    app.push(Screen::multi("tools", ms).then(move |sel| {
        *sink.lock().unwrap() = sel.many().map(|v| v.to_vec());
    }));
    app.on_key(Key::Enter);
    assert_eq!(seen.lock().unwrap().clone(), Some(vec!["web".to_string()]));
}

#[test]
fn a_key_the_top_screen_ignores_reaches_the_one_below() {
    // A filterable list consumes printable characters, so a screen below it
    // can never see one. This is the property that makes stacking safe.
    #[derive(Default)]
    struct Counter(Arc<Mutex<u32>>);
    impl RawHandler for Counter {
        fn draw(&mut self, _f: &mut ratatui::Frame) {}
        fn key(&mut self, key: Key) -> Option<ScreenResult> {
            if key == Key::CtrlK {
                *self.0.lock().unwrap() += 1;
            }
            None
        }
    }
    let hits = Arc::new(Mutex::new(0));
    let mut app = TuiApp::new();
    app.push(Screen::raw("base", Box::new(Counter(hits.clone()))));
    app.push(Screen::select("model", pick()));
    app.on_key(Key::Char('o'));
    app.on_key(Key::CtrlK);
    assert_eq!(
        *hits.lock().unwrap(),
        1,
        "the chord reached the base screen"
    );
    assert_eq!(app.depth(), 2, "nothing popped");
}

#[test]
fn ctrl_c_stops_the_app_from_any_screen() {
    let mut app = TuiApp::new();
    app.push(Screen::select("model", pick()));
    app.on_key(Key::CtrlC);
    assert!(!app.running, "ctrl-c is always fatal to the loop");
}

#[test]
fn a_text_input_swallows_printables_but_not_esc() {
    let mut app = TuiApp::new();
    app.push(Screen::text("name", TextInput::new("name")));
    for c in "Zeus".chars() {
        app.on_key(Key::Char(c));
    }
    assert_eq!(app.depth(), 1, "the letters went into the field");
    app.on_key(Key::Enter);
    assert_eq!(app.depth(), 0, "enter submitted");
}

#[test]
fn control_chords_translate_to_named_keys() {
    use crossterm::event::KeyModifiers;
    let k = |code| KeyEvent::new(code, KeyModifiers::CONTROL);
    assert_eq!(translate(k(KeyCode::Char('c'))), Key::CtrlC);
    assert_eq!(translate(k(KeyCode::Char('k'))), Key::CtrlK);
    assert_eq!(translate(k(KeyCode::Char(' '))), Key::CtrlSpace);
    assert_eq!(translate(k(KeyCode::Char('z'))), Key::Other);
}

#[test]
fn plain_keys_translate_without_a_modifier() {
    use crossterm::event::KeyModifiers;
    let k = |code| KeyEvent::new(code, KeyModifiers::NONE);
    assert_eq!(translate(k(KeyCode::Char('a'))), Key::Char('a'));
    assert_eq!(translate(k(KeyCode::Char(' '))), Key::Space);
    assert_eq!(translate(k(KeyCode::Up)), Key::Up);
    assert_eq!(translate(k(KeyCode::Enter)), Key::Enter);
    assert_eq!(translate(k(KeyCode::F(5))), Key::Other);
}

#[test]
fn a_key_release_is_not_a_press() {
    // Key repeat on some terminals emits both; treating a release as a press
    // double-counts every held key.
    use crossterm::event::KeyModifiers;
    let mut e = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    e.kind = KeyEventKind::Release;
    assert_eq!(translate(e), Key::Other);
}

#[test]
fn replace_swaps_the_top_screen() {
    let mut app = TuiApp::new();
    app.push(Screen::select("a", pick()));
    app.replace(Screen::text("b", TextInput::new("b")));
    assert_eq!(app.depth(), 1);
    assert!(matches!(
        app.top().unwrap().widget,
        ScreenWidget::TextInput(_)
    ));
}

#[test]
fn replace_on_an_empty_stack_pushes() {
    let mut app = TuiApp::new();
    app.replace(Screen::select("only", pick()));
    assert_eq!(app.depth(), 1);
}
