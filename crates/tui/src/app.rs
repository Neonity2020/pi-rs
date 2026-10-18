use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::wrap::display_width;

/// An image waiting in the input box. The bytes stay in the app layer until
/// the user submits; the TUI only needs the metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub media_type: String,
    /// Base64, no data-URL prefix.
    pub data: String,
    pub bytes: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl ImageAttachment {
    pub fn label(&self) -> String {
        let mut label = self
            .media_type
            .strip_prefix("image/")
            .unwrap_or(&self.media_type)
            .to_ascii_uppercase();
        if let (Some(width), Some(height)) = (self.width, self.height) {
            label.push_str(&format!(" {width}×{height}"));
        }
        label.push_str(" · ");
        label.push_str(&human_bytes(self.bytes));
        label
    }
}

fn human_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= 10.0 * KB {
        format!("{:.0} KB", bytes / KB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{} B", bytes as usize)
    }
}

/// Events pushed from the app layer into the UI.
#[derive(Debug, Clone)]
pub enum UiEvent {
    User {
        text: String,
        /// Image labels for the transcript, not the image bytes.
        images: Vec<String>,
    },
    ImageAttached(ImageAttachment),
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

/// One submitted prompt: text plus any images attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

/// What the key handler asks the app layer to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Submit(Submission),
    /// Ctrl+V: read an image from the clipboard (I/O stays in the app layer).
    PasteImage,
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

/// One tool call inside a collapsed thinking block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEntry {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub status: ToolStatus,
    pub output: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub enum Item {
    User {
        text: String,
        images: Vec<String>,
    },
    /// Reasoning and tool calls from one stretch of agent work. Collapsed by
    /// default; a click on the header (or Ctrl+T) expands it.
    Thinking {
        id: u64,
        reasoning: String,
        tools: Vec<ToolEntry>,
        streaming: bool,
    },
    Assistant {
        text: String,
        streaming: bool,
    },
    Info(String),
    Error(String),
}

/// A rendered thinking header, used for mouse hit-testing.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ThinkingHit {
    /// Line index in the full transcript (before scrolling).
    pub line: usize,
    pub id: u64,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct App {
    pub items: Vec<Item>,
    pub input: Input,
    /// Images attached to the prompt being edited.
    pub images: Vec<ImageAttachment>,
    /// Present while the mask takes over the input box.
    pub login: Option<LoginPrompt>,
    /// Whether an API key is available. Drives the input-box hint.
    pub logged_in: bool,
    /// Experimental lab mode (`/lab`). Session-scoped and off by default.
    pub lab: bool,
    pub busy: bool,
    /// Global default for thinking blocks. Collapsed by default; Ctrl+T
    /// toggles this and clears per-block overrides.
    pub show_thinking: bool,
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
    /// Per-block expansion override, keyed by `Item::Thinking::id`.
    thinking_overrides: HashMap<u64, bool>,
    next_thinking_id: u64,
    /// Filled by the renderer for mouse hit-testing.
    pub(crate) thinking_hits: Vec<ThinkingHit>,
    pub(crate) transcript_top: u16,
}

impl App {
    pub fn new(status: StatusInfo) -> Self {
        Self {
            items: Vec::new(),
            input: Input::default(),
            images: Vec::new(),
            login: None,
            logged_in: true,
            lab: false,
            busy: false,
            show_thinking: false,
            status,
            scroll: 0,
            follow: true,
            viewport: 20,
            max_scroll: 0,
            spinner: 0,
            version: 0,
            thinking_overrides: HashMap::new(),
            next_thinking_id: 1,
            thinking_hits: Vec::new(),
            transcript_top: 0,
        }
    }

    pub fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::User { text, images } => {
                self.finish_streaming();
                self.prune_empty_assistant();
                self.items.push(Item::User { text, images });
            }
            UiEvent::ImageAttached(image) => self.images.push(image),
            UiEvent::Info(text) => self.items.push(Item::Info(text)),
            UiEvent::Error(text) => self.items.push(Item::Error(text)),
            UiEvent::AssistantDelta(delta) => {
                // The first answer token means reasoning for this stretch is
                // over, even though the assistant message is still streaming.
                self.finish_thinking();
                let item = self.ensure_assistant();
                if let Item::Assistant { text, .. } = item {
                    text.push_str(&delta);
                }
            }
            UiEvent::ReasoningDelta(delta) => {
                if !delta.is_empty() {
                    let item = self.ensure_thinking(true);
                    if let Item::Thinking { reasoning, .. } = item {
                        reasoning.push_str(&delta);
                    }
                }
            }
            UiEvent::AssistantDone { text } => {
                let mut committed = false;
                if let Some(Item::Assistant {
                    text: current,
                    streaming,
                }) = self.items.last_mut()
                {
                    if *streaming {
                        *current = text.clone();
                        *streaming = false;
                        committed = true;
                    }
                }
                if !committed && !text.is_empty() {
                    self.items.push(Item::Assistant {
                        text,
                        streaming: false,
                    });
                }
                self.finish_streaming();
                self.prune_empty_assistant();
            }
            UiEvent::ToolStart { id, name, summary } => {
                // Text and tool calls can arrive in the same assistant
                // message; close the text segment and fold the call into the
                // current thinking block. Running a tool is not "thinking":
                // only a live reasoning delta marks the block as streaming.
                if matches!(self.items.last(), Some(Item::Assistant { .. })) {
                    self.finish_streaming();
                    self.prune_empty_assistant();
                }
                let item = self.ensure_thinking(false);
                if let Item::Thinking { tools, .. } = item {
                    tools.push(ToolEntry {
                        id,
                        name,
                        summary,
                        status: ToolStatus::Running,
                        output: String::new(),
                        duration_ms: 0,
                    });
                }
            }
            UiEvent::ToolDone {
                id,
                ok,
                output,
                duration_ms,
            } => {
                'outer: for item in self.items.iter_mut().rev() {
                    if let Item::Thinking { tools, .. } = item {
                        for tool in tools.iter_mut().rev() {
                            if tool.id == id {
                                tool.status = if ok {
                                    ToolStatus::Ok
                                } else {
                                    ToolStatus::Failed
                                };
                                tool.output = output;
                                tool.duration_ms = duration_ms;
                                break 'outer;
                            }
                        }
                    }
                }
            }
            UiEvent::Busy(busy) => {
                if !busy {
                    self.finish_streaming();
                    self.prune_empty_assistant();
                }
                self.busy = busy;
            }
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

    fn ensure_thinking(&mut self, streaming: bool) -> &mut Item {
        if !matches!(self.items.last(), Some(Item::Thinking { .. })) {
            let id = self.next_thinking_id;
            self.next_thinking_id = self.next_thinking_id.wrapping_add(1);
            self.items.push(Item::Thinking {
                id,
                reasoning: String::new(),
                tools: Vec::new(),
                streaming,
            });
        }
        match self.items.last_mut() {
            Some(item @ Item::Thinking { .. }) => {
                if let Item::Thinking {
                    streaming: active, ..
                } = item
                {
                    *active = streaming;
                }
                item
            }
            _ => unreachable!("thinking item present"),
        }
    }

    fn ensure_assistant(&mut self) -> &mut Item {
        if !matches!(
            self.items.last(),
            Some(Item::Assistant {
                streaming: true,
                ..
            })
        ) {
            self.items.push(Item::Assistant {
                text: String::new(),
                streaming: true,
            });
        }
        match self.items.last_mut() {
            Some(item @ Item::Assistant { .. }) => item,
            _ => unreachable!("assistant item present"),
        }
    }

    /// Close the most recent thinking block once its reasoning has stopped
    /// (the first answer token, a tool call, or the end of the turn).
    fn finish_thinking(&mut self) {
        if let Some(Item::Thinking { streaming, .. }) = self.items.last_mut() {
            *streaming = false;
        }
    }

    /// Close every in-progress thinking/text block (e.g. before a new user
    /// message or after the turn finishes). Historical blocks are already
    /// closed, so a full scan is cheap and safe.
    fn finish_streaming(&mut self) {
        for item in &mut self.items {
            match item {
                Item::Thinking { streaming, .. } | Item::Assistant { streaming, .. } => {
                    *streaming = false;
                }
                _ => {}
            }
        }
    }

    fn prune_empty_assistant(&mut self) {
        self.items.retain(|item| {
            !matches!(
                item,
                Item::Assistant {
                    text,
                    streaming: false
                } if text.is_empty()
            )
        });
    }

    /// Current expansion state for one thinking block. Per-block clicks win
    /// over the global Ctrl+T default.
    pub(crate) fn thinking_expanded(&self, id: u64) -> bool {
        self.thinking_overrides
            .get(&id)
            .copied()
            .unwrap_or(self.show_thinking)
    }

    /// A left click on a rendered thinking header toggles that block.
    /// Returns true when the click hit a header (and the caller should redraw).
    pub fn on_click(&mut self, _column: u16, row: u16) -> bool {
        if row < self.transcript_top {
            return false;
        }
        let relative = (row as usize).saturating_sub(self.transcript_top as usize);
        if relative >= self.viewport as usize {
            return false;
        }
        let line = self.scroll.saturating_add(relative);
        let Some(hit) = self
            .thinking_hits
            .iter()
            .find(|hit| hit.line == line)
            .copied()
        else {
            return false;
        };
        let expanded = self.thinking_expanded(hit.id);
        self.thinking_overrides.insert(hit.id, !expanded);
        self.version = self.version.wrapping_add(1);
        true
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
                } else if !self.images.is_empty() {
                    self.images.clear();
                    Action::None
                } else {
                    Action::Quit
                }
            }
            KeyCode::Char('v') | KeyCode::Char('V') if ctrl => {
                if self.busy {
                    Action::None
                } else {
                    Action::PasteImage
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.text.is_empty() {
                    Action::Quit
                } else {
                    Action::None
                }
            }
            KeyCode::Char('t') | KeyCode::Char('T') if ctrl => {
                self.show_thinking = !self.show_thinking;
                self.thinking_overrides.clear();
                Action::None
            }
            KeyCode::Esc if !self.images.is_empty() => {
                self.images.pop();
                Action::None
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
                if text.trim().is_empty() && self.images.is_empty() {
                    Action::None
                } else {
                    Action::Submit(Submission {
                        text,
                        images: std::mem::take(&mut self.images),
                    })
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
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        text
    }

    /// Replace the in-memory history (oldest first), e.g. with prompts
    /// persisted from earlier runs. The coding layer owns loading; `tui`
    /// stays free of I/O.
    pub fn seed_history(&mut self, entries: Vec<String>) {
        self.history = entries;
        self.history_pos = None;
        self.stash.clear();
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
    fn seeded_history_is_recalled_with_up_and_down() {
        let mut input = Input::default();
        input.seed_history(vec!["older".into(), "newer".into()]);
        input.insert('x');

        input.history_prev();
        assert_eq!(input.text, "newer");
        input.history_prev();
        assert_eq!(input.text, "older");
        input.history_next();
        assert_eq!(input.text, "newer");
        input.history_next();
        // Back to the prompt that was being edited before browsing.
        assert_eq!(input.text, "x");
    }

    #[test]
    fn submitting_the_same_prompt_twice_keeps_one_entry() {
        let mut input = Input::default();
        for _ in 0..2 {
            input.insert('a');
            let _ = input.take();
        }
        input.history_prev();
        assert_eq!(input.text, "a");
        input.history_prev();
        assert_eq!(input.text, "a");
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
        app.apply(UiEvent::User {
            text: "older".into(),
            images: Vec::new(),
        });
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
            Action::Submit(Submission {
                text: "hello".into(),
                images: Vec::new(),
            })
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
    fn ctrl_t_toggles_thinking_visibility() {
        let mut app = App::new(StatusInfo::default());
        assert!(!app.show_thinking);
        let ctrl_t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.on_key(ctrl_t), Action::None);
        assert!(app.show_thinking);
        assert_eq!(app.on_key(ctrl_t), Action::None);
        assert!(!app.show_thinking);
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
            Item::Thinking { tools, .. } => {
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].status, ToolStatus::Failed);
                assert_eq!(tools[0].output, "boom");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn reasoning_and_tools_share_one_thinking_item() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ReasoningDelta("first plan".into()));
        app.apply(UiEvent::ToolStart {
            id: "c1".into(),
            name: "read".into(),
            summary: "src/lib.rs".into(),
        });
        app.apply(UiEvent::ToolDone {
            id: "c1".into(),
            ok: true,
            output: "file".into(),
            duration_ms: 3,
        });
        app.apply(UiEvent::ReasoningDelta("\nsecond plan".into()));
        app.apply(UiEvent::AssistantDone {
            text: "answer".into(),
        });

        assert_eq!(app.items.len(), 2, "one thinking block plus the answer");
        match &app.items[0] {
            Item::Thinking {
                reasoning,
                tools,
                streaming,
                ..
            } => {
                assert_eq!(reasoning, "first plan\nsecond plan");
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].status, ToolStatus::Ok);
                assert!(!streaming);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            &app.items[1],
            Item::Assistant { text, .. } if text == "answer"
        ));
    }

    #[test]
    fn assistant_delta_ends_the_live_thinking_block() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ReasoningDelta("plan".into()));
        assert!(matches!(
            &app.items[0],
            Item::Thinking {
                streaming: true,
                ..
            }
        ));

        app.apply(UiEvent::AssistantDelta("answer".into()));
        assert!(matches!(
            &app.items[0],
            Item::Thinking {
                streaming: false,
                ..
            }
        ));
        assert!(matches!(
            &app.items[1],
            Item::Assistant {
                text,
                streaming: true,
            } if text == "answer"
        ));
    }

    #[test]
    fn running_a_tool_does_not_reopen_thinking() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ReasoningDelta("plan".into()));
        app.apply(UiEvent::ToolStart {
            id: "c1".into(),
            name: "read".into(),
            summary: "src/lib.rs".into(),
        });
        assert!(matches!(
            &app.items[0],
            Item::Thinking {
                streaming: false,
                ..
            }
        ));

        // A later reasoning stretch in the same turn makes it live again.
        app.apply(UiEvent::ReasoningDelta("more".into()));
        assert!(matches!(
            &app.items[0],
            Item::Thinking {
                streaming: true,
                ..
            }
        ));
    }

    #[test]
    fn clicking_a_thinking_header_toggles_only_that_block() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ReasoningDelta("one".into()));
        let first = match &app.items[0] {
            Item::Thinking { id, .. } => *id,
            other => panic!("unexpected: {other:?}"),
        };
        app.apply(UiEvent::AssistantDone {
            text: "answer".into(),
        });
        app.apply(UiEvent::ReasoningDelta("two".into()));
        let second = match &app.items[2] {
            Item::Thinking { id, .. } => *id,
            other => panic!("unexpected: {other:?}"),
        };

        app.transcript_top = 0;
        app.viewport = 10;
        app.thinking_hits = vec![ThinkingHit { line: 0, id: first }];
        assert!(app.on_click(0, 0));
        assert!(app.thinking_expanded(first));
        assert!(!app.thinking_expanded(second));

        assert!(app.on_click(0, 0));
        assert!(!app.thinking_expanded(first));
    }

    #[test]
    fn submitted_user_message_becomes_a_transcript_item() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::User {
            text: "hello".into(),
            images: Vec::new(),
        });
        assert!(matches!(&app.items[0], Item::User { text, .. } if text == "hello"));
        // A second submission must not be swallowed either.
        app.apply(UiEvent::User {
            text: "again".into(),
            images: vec!["PNG · 1.0 KB".into()],
        });
        assert_eq!(app.items.len(), 2);
        assert!(matches!(&app.items[1], Item::User { images, .. } if images.len() == 1));
    }

    fn attachment() -> ImageAttachment {
        ImageAttachment {
            media_type: "image/png".into(),
            data: "AAAA".into(),
            bytes: 1536,
            width: Some(640),
            height: Some(480),
        }
    }

    #[test]
    fn ctrl_v_asks_the_app_layer_for_a_clipboard_image() {
        let mut app = App::new(StatusInfo::default());
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            Action::PasteImage
        );

        // While busy the paste is ignored instead of queueing an image.
        app.busy = true;
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            Action::None
        );
    }

    #[test]
    fn images_submit_with_or_without_text_and_are_cleared() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ImageAttached(attachment()));
        assert_eq!(app.images.len(), 1);
        assert_eq!(app.images[0].label(), "PNG 640×480 · 1.5 KB");

        // An image-only turn is a valid submission.
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit(Submission {
                text: String::new(),
                images: vec![attachment()],
            })
        );
        assert!(app.images.is_empty());

        app.apply(UiEvent::ImageAttached(attachment()));
        app.on_key(key(KeyCode::Char('h')));
        app.on_key(key(KeyCode::Char('i')));
        match app.on_key(key(KeyCode::Enter)) {
            Action::Submit(submission) => {
                assert_eq!(submission.text, "hi");
                assert_eq!(submission.images.len(), 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(app.images.is_empty());
    }

    #[test]
    fn esc_and_ctrl_c_drop_pending_images() {
        let mut app = App::new(StatusInfo::default());
        app.apply(UiEvent::ImageAttached(attachment()));
        app.apply(UiEvent::ImageAttached(attachment()));
        assert_eq!(app.images.len(), 2);

        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.images.len(), 1);

        // Ctrl+C clears the input first, then images, then quits.
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::None
        );
        assert!(app.input.text.is_empty());
        assert_eq!(app.images.len(), 1);
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::None
        );
        assert!(app.images.is_empty());
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
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
