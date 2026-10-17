use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::wrap::display_width;

/// Events pushed from the app layer into the UI.
#[derive(Debug, Clone)]
pub enum UiEvent {
    User(String),
    Info(String),
    Error(String),
    AssistantDelta(String),
    ReasoningDelta(String),
    /// The final, authoritative text of the current assistant message.
    AssistantDone {
        text: String,
    },
    ToolStart {
        id: String,
        name: String,
        summary: String,
    },
    ToolDone {
        id: String,
        ok: bool,
        output: String,
        duration_ms: u64,
    },
    Busy(bool),
    Status(StatusInfo),
    /// A `/login` attempt finished: close the prompt, report, and (on
    /// success) mark the session as authenticated.
    LoginDone {
        ok: bool,
        message: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct StatusInfo {
    pub model: String,
    pub dir: String,
    pub session: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost: String,
}

impl StatusInfo {
    /// Prompt-cache hit rate as a rounded percentage. `None` until the first
    /// usage report, so an empty bar does not claim "0%".
    pub fn cache_hit_rate(&self) -> Option<u64> {
        if self.input_tokens == 0 {
            return None;
        }
        let cached = u128::from(self.cached_tokens.min(self.input_tokens));
        let input = u128::from(self.input_tokens);
        Some(((cached * 100 + input / 2) / input) as u64)
    }
}

/// What the key handler asks the app layer to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Submit(String),
    /// The user pressed Enter in the login prompt with a non-empty key.
    Login(String),
    Interrupt,
    Quit,
}

/// Masked API-key input shown in place of the normal prompt.
#[derive(Debug, Clone)]
pub struct LoginPrompt {
    pub provider: String,
    pub input: Input,
    pub busy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
}

#[derive(Debug, Clone)]
pub enum Item {
    User(String),
    Assistant {
        text: String,
        reasoning: String,
        streaming: bool,
    },
    Tool {
        id: String,
        name: String,
        summary: String,
        status: ToolStatus,
        output: String,
        duration_ms: u64,
    },
    Info(String),
    Error(String),
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct App {
    pub items: Vec<Item>,
    pub input: Input,
    /// Present while the mask takes over the input box.
    pub login: Option<LoginPrompt>,
    /// Whether an API key is available. Drives the input-box hint.
    pub logged_in: bool,
    pub busy: bool,
    pub status: StatusInfo,
    pub scroll: usize,
    pub follow: bool,
    /// Viewport height of the transcript, refreshed on every render.
    pub viewport: u16,
    /// Largest valid `scroll`, refreshed on every render.
    pub max_scroll: usize,
    pub spinner: usize,
    /// Bumped whenever the rendered transcript may be stale.
    pub version: u64,
}

impl App {
    pub fn new(status: StatusInfo) -> Self {
        Self {
            items: Vec::new(),
            input: Input::default(),
            login: None,
            logged_in: true,
            busy: false,
            status,
            scroll: 0,
            follow: true,
            viewport: 20,
            max_scroll: 0,
            spinner: 0,
            version: 0,
        }
    }

    pub fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::User(text) => {
                self.finish_streaming();
                self.items.push(Item::User(text));
            }
            UiEvent::Info(text) => self.items.push(Item::Info(text)),
            UiEvent::Error(text) => self.items.push(Item::Error(text)),
            UiEvent::AssistantDelta(delta) => {
                let (text, _) = self.streaming_fields();
                text.push_str(&delta);
            }
            UiEvent::ReasoningDelta(delta) => {
                let (_, reasoning) = self.streaming_fields();
                reasoning.push_str(&delta);
            }
            UiEvent::AssistantDone { text } => {
                if let Some(Item::Assistant {
                    text: current,
                    streaming,
                    ..
                }) = self.items.last_mut()
                {
                    if *streaming {
                        *current = text;
                        *streaming = false;
                    } else if !text.is_empty() {
                        self.items.push(Item::Assistant {
                            text,
                            reasoning: String::new(),
                            streaming: false,
                        });
                    }
                } else if !text.is_empty() {
                    self.items.push(Item::Assistant {
                        text,
                        reasoning: String::new(),
                        streaming: false,
                    });
                }
                self.prune_empty_streaming();
            }
            UiEvent::ToolStart { id, name, summary } => {
                self.finish_streaming();
                self.items.push(Item::Tool {
                    id,
                    name,
                    summary,
                    status: ToolStatus::Running,
                    output: String::new(),
                    duration_ms: 0,
                });
            }
            UiEvent::ToolDone {
                id,
                ok,
                output,
                duration_ms,
            } => {
                for item in self.items.iter_mut().rev() {
                    if let Item::Tool {
                        id: tool_id,
                        status,
                        output: tool_output,
                        duration_ms: tool_ms,
                        ..
                    } = item
                    {
                        if *tool_id == id {
                            *status = if ok {
                                ToolStatus::Ok
                            } else {
                                ToolStatus::Failed
                            };
                            *tool_output = output;
                            *tool_ms = duration_ms;
                            break;
                        }
                    }
                }
            }
            UiEvent::Busy(busy) => self.busy = busy,
            UiEvent::Status(status) => self.status = status,
            UiEvent::LoginDone { ok, message } => {
                self.login = None;
                if ok {
                    self.logged_in = true;
                    self.items.push(Item::Info(message));
                } else {
                    self.items.push(Item::Error(message));
                }
            }
        }
        self.version = self.version.wrapping_add(1);
    }

    /// Replace the input box with a masked key prompt.
    pub fn begin_login(&mut self, provider: impl Into<String>) {
        self.login = Some(LoginPrompt {
            provider: provider.into(),
            input: Input::default(),
            busy: false,
        });
        self.version = self.version.wrapping_add(1);
    }

    fn streaming_fields(&mut self) -> (&mut String, &mut String) {
        let needs_new = !matches!(
            self.items.last(),
            Some(Item::Assistant {
                streaming: true,
                ..
            })
        );
        if needs_new {
            self.items.push(Item::Assistant {
                text: String::new(),
                reasoning: String::new(),
                streaming: true,
            });
        }
        match self.items.last_mut() {
            Some(Item::Assistant {
                text, reasoning, ..
            }) => (text, reasoning),
            _ => unreachable!("streaming item present"),
        }
    }

    /// Close an in-progress streaming item (e.g. before a new user message).
    fn finish_streaming(&mut self) {
        if let Some(Item::Assistant { streaming, .. }) = self.items.last_mut() {
            *streaming = false;
        }
        self.prune_empty_streaming();
    }

    fn prune_empty_streaming(&mut self) {
        if let Some(Item::Assistant {
            text,
            reasoning,
            streaming: false,
        }) = self.items.last()
        {
            if text.is_empty() && reasoning.is_empty() {
                self.items.pop();
            }
        }
    }

    pub fn tick(&mut self) {
        if self.busy {
            self.spinner = (self.spinner + 1) % SPINNER.len();
        }
    }

    pub fn spinner_char(&self) -> &'static str {
        if self.busy {
            SPINNER[self.spinner]
        } else {
            "●"
        }
    }

    /// Mouse-wheel scrolling of the transcript. Negative = up, positive = down.
    pub fn on_scroll(&mut self, delta: i32) {
        const WHEEL_LINES: usize = 3;
        if delta < 0 {
            self.follow = false;
            self.scroll = self
                .scroll
                .saturating_sub(delta.unsigned_abs() as usize * WHEEL_LINES);
        } else if !self.follow {
            self.scroll = self
                .scroll
                .saturating_add(delta as usize * WHEEL_LINES)
                .min(self.max_scroll);
            // Reaching the bottom resumes following new output.
            if self.scroll >= self.max_scroll {
                self.follow = true;
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if self.login.is_some() {
            return self.on_login_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    Action::Interrupt
                } else if !self.input.text.is_empty() {
                    self.input.clear();
                    Action::None
                } else {
                    Action::Quit
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.text.is_empty() {
                    Action::Quit
                } else {
                    Action::None
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(c);
                Action::None
            }
            KeyCode::Backspace => {
                self.input.backspace();
                Action::None
            }
            KeyCode::Delete => {
                self.input.delete();
                Action::None
            }
            KeyCode::Left => {
                self.input.left();
                Action::None
            }
            KeyCode::Right => {
                self.input.right();
                Action::None
            }
            KeyCode::Home => {
                self.input.home();
                Action::None
            }
            KeyCode::End => {
                self.input.end();
                Action::None
            }
            KeyCode::Up => {
                self.input.history_prev();
                Action::None
            }
            KeyCode::Down => {
                self.input.history_next();
                Action::None
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self
                    .scroll
                    .saturating_sub(self.viewport.max(1) as usize / 2 + 1);
                Action::None
            }
            KeyCode::PageDown => {
                self.scroll = self
                    .scroll
                    .saturating_add(self.viewport.max(1) as usize / 2 + 1)
                    .min(self.max_scroll);
                self.follow = self.scroll >= self.max_scroll;
                Action::None
            }
            KeyCode::Enter => {
                if self.busy {
                    return Action::None;
                }
                let text = self.input.take();
                if text.trim().is_empty() {
                    Action::None
                } else {
                    Action::Submit(text)
                }
            }
            _ => Action::None,
        }
    }

    /// Keys while the masked login prompt is open. The prompt owns every
    /// printable key; Esc cancels, Enter submits (unless a check is running).
    fn on_login_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let busy = self.login.as_ref().is_some_and(|login| login.busy);
        let cancel =
            matches!(key.code, KeyCode::Esc) || (matches!(key.code, KeyCode::Char('c')) && ctrl);
        if cancel {
            if !busy {
                self.login = None;
                self.version = self.version.wrapping_add(1);
            }
            return Action::None;
        }
        if busy {
            return Action::None;
        }
        let Some(login) = self.login.as_mut() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Char(c) if !ctrl => {
                login.input.insert(c);
                Action::None
            }
            KeyCode::Backspace => {
                login.input.backspace();
                Action::None
            }
            KeyCode::Delete => {
                login.input.delete();
                Action::None
            }
            KeyCode::Left => {
                login.input.left();
                Action::None
            }
            KeyCode::Right => {
                login.input.right();
                Action::None
            }
            KeyCode::Home => {
                login.input.home();
                Action::None
            }
            KeyCode::End => {
                login.input.end();
                Action::None
            }
            KeyCode::Enter => {
                let text = login.input.text.trim().to_string();
                if text.is_empty() {
                    Action::None
                } else {
                    Action::Login(text)
                }
            }
            _ => Action::None,
        }
    }
}

/// A single-line editor with history.
#[derive(Debug, Clone, Default)]
pub struct Input {
    pub text: String,
    /// Cursor position in characters.
    pub cursor: usize,
    history: Vec<String>,
    /// Position while browsing history (`None` = editing the live input).
    history_pos: Option<usize>,
    stash: String,
}

impl Input {
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.history_pos = None;
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        text
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_index(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
        self.history_pos = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.byte_index(self.cursor - 1);
        self.text.remove(at);
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.chars().count() {
            return;
        }
        let at = self.byte_index(self.cursor);
        self.text.remove(at);
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.text.chars().count() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.stash = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(pos) => pos - 1,
        };
        self.history_pos = Some(pos);
        self.text = self.history[pos].clone();
        self.end();
    }

    pub fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(pos) if pos + 1 < self.history.len() => {
                self.history_pos = Some(pos + 1);
                self.text = self.history[pos + 1].clone();
                self.end();
            }
            Some(_) => {
                self.history_pos = None;
                self.text = std::mem::take(&mut self.stash);
                self.end();
            }
        }
    }

    /// Cursor column in display cells.
    pub fn cursor_column(&self) -> usize {
        display_width(&self.text[..self.byte_index(self.cursor)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn editing() {
        let mut input = Input::default();
        input.insert('h');
        input.insert('i');
        assert_eq!(input.text, "hi");
        input.left();
        input.insert('!');
        assert_eq!(input.text, "h!i");
        input.backspace();
        assert_eq!(input.text, "hi");
        input.home();
        input.delete();
        assert_eq!(input.text, "i");
    }

    #[test]
    fn history_roundtrip() {
        let mut input = Input::default();
        input.insert('a');
        let _ = input.take();
        input.insert('b');
        let _ = input.take();
        input.history_prev();
        assert_eq!(input.text, "b");
        input.history_prev();
        assert_eq!(input.text, "a");
        input.history_next();
        assert_eq!(input.text, "b");
        input.history_next();
        assert_eq!(input.text, "");
    }

    #[test]
    fn cache_hit_rate_is_rounded_and_handles_empty_usage() {
        let mut status = StatusInfo::default();
        assert_eq!(status.cache_hit_rate(), None);

        status.input_tokens = 1_000;
        status.cached_tokens = 750;
        assert_eq!(status.cache_hit_rate(), Some(75));

        status.input_tokens = 3;
        status.cached_tokens = 1;
        assert_eq!(status.cache_hit_rate(), Some(33));

        status.input_tokens = 1_000;
        status.cached_tokens = 1;
        assert_eq!(status.cache_hit_rate(), Some(0));

        status.cached_tokens = 999;
        assert_eq!(status.cache_hit_rate(), Some(100));

        // A provider reporting more cached tokens than input must not exceed 100%.
        status.cached_tokens = 2_000;
        assert_eq!(status.cache_hit_rate(), Some(100));
    }

    #[test]
    fn wheel_scrolls_transcript_and_resumes_follow_at_bottom() {
        let mut app = App::new(StatusInfo::default());
        app.max_scroll = 10;
        app.scroll = 10;
        assert!(app.follow);

        // Wheel up detaches from the tail.
        app.on_scroll(-1);
        assert!(!app.follow);
        assert_eq!(app.scroll, 7);

        // Wheel up past the top clamps at 0 and leaves the input untouched.
        app.on_scroll(-100);
        assert_eq!(app.scroll, 0);
        assert!(app.input.text.is_empty());

        // Wheel down returns to the bottom and follows again.
        app.on_scroll(100);
        assert_eq!(app.scroll, 10);
        assert!(app.follow);
    }

    #[test]
    fn wheel_never_moves_the_input_history() {
        let mut app = App::new(StatusInfo::default());
        app.input.insert('a');
        let _ = app.input.take();
        app.apply(UiEvent::User("older".into()));
        app.max_scroll = 5;
        app.scroll = 5;

        app.on_scroll(-1);
        assert_eq!(app.input.text, "");
        assert_eq!(app.scroll, 2);
    }

    #[test]
    fn ctrl_c_interrupts_when_busy_and_quits_when_idle() {
        let mut app = App::new(StatusInfo::default());
        app.busy = true;
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Interrupt
        );
        app.busy = false;
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn enter_submits_and_clears_input() {
        let mut app = App::new(StatusInfo::default());
        for c in "hello".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("hello".into())
        );
        assert!(app.input.text.is_empty());
    }

    #[test]
    fn streaming_is_committed_by_final_text() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::AssistantDelta("hel".into()));
        app.apply(UiEvent::AssistantDelta("lo".into()));
        app.apply(UiEvent::AssistantDone {
            text: "hello!".into(),
        });
        match &app.items[0] {
            Item::Assistant {
                text, streaming, ..
            } => {
                assert_eq!(text, "hello!");
                assert!(!streaming);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_output_is_attached_by_id() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ToolStart {
            id: "c1".into(),
            name: "bash".into(),
            summary: "ls".into(),
        });
        app.apply(UiEvent::ToolDone {
            id: "c1".into(),
            ok: false,
            output: "boom".into(),
            duration_ms: 5,
        });
        match &app.items[0] {
            Item::Tool { status, output, .. } => {
                assert_eq!(*status, ToolStatus::Failed);
                assert_eq!(output, "boom");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn submitted_user_message_becomes_a_transcript_item() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::User("hello".into()));
        assert!(matches!(&app.items[0], Item::User(text) if text == "hello"));
        // A second submission must not be swallowed either.
        app.apply(UiEvent::User("again".into()));
        assert_eq!(app.items.len(), 2);
    }

    #[test]
    fn login_prompt_collects_a_key_and_closes_on_result() {
        let mut app = App::new(StatusInfo::default());
        app.begin_login("DeepSeek");
        for c in "sk-test".chars() {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), Action::None);
        }
        // Normal prompt keeps its own (empty) input.
        assert!(app.input.text.is_empty());
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Login("sk-test".into())
        );

        // While the key is being checked, Enter does nothing.
        app.login.as_mut().unwrap().busy = true;
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);

        app.apply(UiEvent::LoginDone {
            ok: true,
            message: "logged in".into(),
        });
        assert!(app.login.is_none());
        assert!(app.logged_in);
        assert!(matches!(app.items.last(), Some(Item::Info(text)) if text == "logged in"));
    }

    #[test]
    fn esc_cancels_login_and_a_failure_keeps_you_logged_out() {
        let mut app = App::new(StatusInfo::default());
        app.logged_in = false;
        app.begin_login("DeepSeek");
        app.on_key(key(KeyCode::Esc));
        assert!(app.login.is_none());

        app.begin_login("DeepSeek");
        app.apply(UiEvent::LoginDone {
            ok: false,
            message: "rejected".into(),
        });
        assert!(!app.logged_in);
        assert!(matches!(app.items.last(), Some(Item::Error(text)) if text == "rejected"));
    }
}
