//! Slash-command metadata and prefix completion.
//!
//! This module is pure data: it knows the names and one-line help, not what
//! the commands do. The coding layer owns execution; the TUI uses the table
//! to render the completion menu and `/help`.

/// One slash command as shown in `/help` and the completion menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// The literal prefix to type, e.g. `/remember global`. Matching and
    /// completion use this, so subcommands stay reachable.
    pub name: &'static str,
    /// Argument hint, e.g. `<text>`; empty for commands that take none.
    pub hint: &'static str,
    pub help: &'static str,
    /// Experimental command, gated behind `/lab`.
    pub lab: bool,
}

impl Command {
    /// `name hint` (or just `name`), aligned in `/help`.
    pub fn usage(&self) -> String {
        if self.hint.is_empty() {
            self.name.to_string()
        } else {
            format!("{} {}", self.name, self.hint)
        }
    }

    /// What Tab inserts: the name, plus a trailing space when the command
    /// still expects arguments.
    pub fn insertion(&self) -> String {
        if self.hint.is_empty() {
            self.name.to_string()
        } else {
            format!("{} ", self.name)
        }
    }
}

/// Every slash command, in help order.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "/login",
        hint: "",
        help: "sign in and save your API key to auth.json",
        lab: false,
    },
    Command {
        name: "/logout",
        hint: "",
        help: "remove the saved API key",
        lab: false,
    },
    Command {
        name: "/compact",
        hint: "",
        help: "summarize older messages to free context",
        lab: false,
    },
    Command {
        name: "/model",
        hint: "<id>",
        help: "show the available models, or switch to one",
        lab: false,
    },
    Command {
        name: "/remember",
        hint: "<text>",
        help: "add a note to this project's long-term memory",
        lab: false,
    },
    Command {
        name: "/remember global",
        hint: "<text>",
        help: "add a note for all projects",
        lab: false,
    },
    Command {
        name: "/forget",
        hint: "<text>",
        help: "remove matching notes from this project's memory",
        lab: false,
    },
    Command {
        name: "/forget global",
        hint: "<text>",
        help: "remove matching notes for all projects",
        lab: false,
    },
    Command {
        name: "/memory",
        hint: "",
        help: "show memory file paths and counts",
        lab: false,
    },
    Command {
        name: "/lab",
        hint: "",
        help: "toggle experimental lab mode",
        lab: false,
    },
    Command {
        name: "/dream",
        hint: "",
        help: "consolidate and forget long-term memory",
        lab: true,
    },
    Command {
        name: "/help",
        hint: "",
        help: "show this",
        lab: false,
    },
];

/// The prefix being completed: the whole input while it is a single token
/// that starts with `/`. Any whitespace or non-slash first character means
/// the user is past command completion.
pub fn token(text: &str) -> Option<&str> {
    if !text.starts_with('/') || text.chars().any(char::is_whitespace) {
        return None;
    }
    Some(text)
}

/// Commands whose name starts with `prefix`, in table order.
pub fn matches(prefix: &str) -> Vec<&'static Command> {
    if !prefix.starts_with('/') {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(prefix))
        .collect()
}

/// Open completion menu: the matching commands plus the highlighted row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub matches: Vec<&'static Command>,
    pub selected: usize,
}

impl Completion {
    pub fn selected(&self) -> &'static Command {
        self.matches[self.selected.min(self.matches.len() - 1)]
    }

    /// Move down, wrapping to the first entry.
    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    /// Move up, wrapping to the last entry.
    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
        }
    }
}

/// `/help` body, generated from [`COMMANDS`] so the menu and the text cannot
/// drift apart.
pub fn help_text() -> String {
    let width = COMMANDS
        .iter()
        .map(|command| command.usage().chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let mut text = String::new();
    for command in COMMANDS {
        text.push_str(&format!(
            "{:<width$}{}\n",
            command.usage(),
            command.help,
            width = width
        ));
    }
    text.push_str(
        "\nCtrl+V   attach an image from the clipboard (Esc removes the last one)\n\
         Ctrl+T   expand or collapse the agent's thinking",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_only_matches_a_bare_slash_prefix() {
        assert_eq!(token("/"), Some("/"));
        assert_eq!(token("/log"), Some("/log"));
        assert_eq!(token("/login x"), None);
        assert_eq!(token("/login "), None);
        assert_eq!(token("hello"), None);
        assert_eq!(token(" /login"), None);
        assert_eq!(token(""), None);
    }

    #[test]
    fn matches_filter_by_prefix_and_keep_help_order() {
        let names: Vec<_> = matches("/log").iter().map(|command| command.name).collect();
        assert_eq!(names, ["/login", "/logout"]);

        let remember: Vec<_> = matches("/remember")
            .iter()
            .map(|command| command.usage())
            .collect();
        assert_eq!(remember, ["/remember <text>", "/remember global <text>"]);

        let forget: Vec<_> = matches("/forget")
            .iter()
            .map(|command| command.usage())
            .collect();
        assert_eq!(forget, ["/forget <text>", "/forget global <text>"]);

        assert!(matches("/nope").is_empty());
        assert!(matches("log").is_empty());
    }

    #[test]
    fn insertion_adds_a_space_only_when_arguments_follow() {
        let login = matches("/login").remove(0);
        assert_eq!(login.insertion(), "/login");
        let remember = matches("/remember").remove(0);
        assert_eq!(remember.insertion(), "/remember ");
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut completion = Completion {
            matches: matches("/log"),
            selected: 0,
        };
        assert_eq!(completion.selected().name, "/login");
        completion.next();
        assert_eq!(completion.selected().name, "/logout");
        completion.next();
        assert_eq!(completion.selected().name, "/login");
        completion.prev();
        assert_eq!(completion.selected().name, "/logout");
    }

    #[test]
    fn help_lists_every_command_and_the_keys() {
        let help = help_text();
        for command in COMMANDS {
            assert!(help.contains(command.name), "missing {}", command.name);
            assert!(
                help.contains(command.help),
                "missing help for {}",
                command.name
            );
        }
        assert!(help.contains("Ctrl+V"));
        assert!(help.contains("Ctrl+T"));
        // Usage and help are aligned on the same line.
        let width = COMMANDS
            .iter()
            .map(|command| command.usage().chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        for command in COMMANDS {
            let line = format!("{:<width$}{}", command.usage(), command.help, width = width);
            assert!(help.contains(&line), "missing line: {line}");
        }
    }
}
