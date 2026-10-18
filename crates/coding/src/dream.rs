//! Experimental `/dream`: one isolated model turn that consolidates the two
//! long-term memory files and forgets what no longer earns its keep.
//!
//! The pass is model-driven but deliberately narrow: it gets a dedicated
//! system prompt, only the file tools, and no conversation history. Its
//! internal messages are shown in the TUI but not written to the session;
//! the memory files remain the only persisted state. `lab` mode gates it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent::{Agent, Cancel, Message, Model, Outcome, Provider, Sink, Tool};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;

use crate::memory::{self, Counts};
use crate::sinks::Shared;
use crate::ui::{TuiSink, UiEvent};
use crate::util;

const USER_PROMPT: &str = "Dream now. Consolidate both long-term memory files.";

/// The tool subset a dream is allowed to use. The main agent's selection is
/// respected: if the user launched with `--tools read`, dream cannot write.
pub fn tools(tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    tools
        .iter()
        .filter(|tool| matches!(tool.name(), "read" | "write" | "edit"))
        .cloned()
        .collect()
}

/// Whether the available tools can both inspect and rewrite memory.
pub fn can_run(tools: &[Arc<dyn Tool>]) -> bool {
    tools.iter().any(|tool| tool.name() == "read")
        && tools.iter().any(|tool| tool.name() == "write")
}

/// System prompt for the isolated dream agent. Kept out of the base prompt:
/// this is a maintenance task, not a coding persona.
pub fn system_prompt(home: &Path, cwd: &Path) -> String {
    format!(
        "You are pi-rs in dream mode. This is a maintenance pass over the user's \
long-term memory, not a coding task.

Global memory file: {global}
Project memory file: {project}
Today: {today}

Read both files with the read tool. Then consolidate them with write or edit:
- Merge duplicate and near-duplicate notes into one note. Keep the earliest date.
- Keep durable preferences, project conventions, decisions, pitfalls, and commands \
that will matter in future sessions.
- Forget stale status, one-off facts, trivia, secrets, and anything already stated \
in AGENTS.md or derivable from the repository.
- Preserve one note per line as `- (YYYY-MM-DD) text`; each note stays under \
{max} characters.
- Touch only those two files. Do not run commands. Do not ask questions.
- Actually update the files with the tools; do not just print the new contents.

When finished, reply with a short report of what you merged, what you forgot, and why.",
        global = memory::global_path(home).display(),
        project = memory::project_path(home, cwd).display(),
        today = &util::iso_now()[..10],
        max = memory::ENTRY_MAX_CHARS,
    )
}

/// One-line result for the TUI. Counts are parsed notes, so a merge and a
/// deletion both show up as "forgotten or merged".
pub fn report(before: Counts, after: Counts) -> String {
    let absorbed = before.total().saturating_sub(after.total());
    let tail = if absorbed == 0 {
        "nothing forgotten".to_string()
    } else {
        format!(
            "{absorbed} {} forgotten or merged",
            if absorbed == 1 { "note" } else { "notes" }
        )
    };
    format!(
        "✦ dream complete · global {}→{} · project {}→{} · {tail}",
        before.global, after.global, before.project, after.project
    )
}

/// Build the isolated agent and run exactly one maintenance turn.
pub async fn run(
    provider: Arc<dyn Provider>,
    model: Model,
    tools: Vec<Arc<dyn Tool>>,
    home: &Path,
    cwd: &Path,
    sink: &mut dyn Sink,
    cancel: Cancel,
) -> Result<Outcome, agent::Error> {
    let mut dream = Agent::new(provider, model, system_prompt(home, cwd), tools, Vec::new());
    dream
        .run_turn(Message::user(USER_PROMPT), sink, cancel)
        .await
}

/// Run the dream in the background. The caller has already verified that
/// memory is non-empty and that the required tools are available.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    agent: Arc<Mutex<Agent>>,
    tools: Vec<Arc<dyn Tool>>,
    shared: Shared,
    cwd: PathBuf,
    ui: UnboundedSender<UiEvent>,
    cancel: Cancel,
    before: Counts,
) {
    tokio::spawn(async move {
        let (provider, model): (Arc<dyn Provider>, Model) = {
            let agent = agent.lock().await;
            (agent.provider(), agent.model().clone())
        };
        let home = util::home_dir();

        let _ = ui.send(UiEvent::Info(
            "✦ dreaming over long-term memory — merging, keeping, forgetting".into(),
        ));
        let mut sink = TuiSink::ephemeral(shared.clone(), ui.clone());
        let outcome = run(provider, model, tools, &home, &cwd, &mut sink, cancel).await;

        match outcome {
            Ok(Outcome::Completed) => {
                let after = memory::counts(&home, &cwd);
                let _ = ui.send(UiEvent::Info(report(before, after)));
            }
            Ok(Outcome::Aborted) => {
                let _ = ui.send(UiEvent::Info("dream interrupted".into()));
            }
            Err(error) => {
                let _ = ui.send(UiEvent::Error(format!("dream failed: {error}")));
            }
        }
        let _ = ui.send(UiEvent::Status(shared.status_info()));
        let _ = ui.send(UiEvent::Busy(false));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_tool(name: &'static str) -> Arc<dyn Tool> {
        struct Dummy(&'static str);

        impl Tool for Dummy {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "test"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn run<'a>(
                &'a self,
                _args: &'a serde_json::Value,
                _cancel: &'a Cancel,
            ) -> agent::BoxFuture<'a, agent::ToolResult> {
                Box::pin(async { agent::ToolResult::ok("") })
            }
        }

        Arc::new(Dummy(name))
    }

    #[test]
    fn dream_tools_are_only_the_file_tools() {
        let all = vec![
            dummy_tool("read"),
            dummy_tool("write"),
            dummy_tool("edit"),
            dummy_tool("bash"),
        ];
        let names: Vec<_> = tools(&all)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(names, ["read", "write", "edit"]);
        assert!(can_run(&tools(&all)));

        let readonly = vec![dummy_tool("read")];
        assert!(!can_run(&readonly));
    }

    #[test]
    fn system_prompt_names_both_files_and_the_line_format() {
        let home = Path::new("/tmp/pi-home");
        let cwd = Path::new("/tmp/project");
        let prompt = system_prompt(home, cwd);
        assert!(prompt.contains("/tmp/pi-home/MEMORY.md"));
        assert!(prompt.contains("/tmp/pi-home/memory/tmp-project.md"));
        assert!(prompt.contains("- (YYYY-MM-DD) text"));
        assert!(prompt.contains(&memory::ENTRY_MAX_CHARS.to_string()));
        assert!(prompt.contains("Do not run commands"));
    }

    #[tokio::test]
    async fn dream_can_rewrite_memory_through_the_file_tools() {
        use llm::mock::ScriptedProvider;
        use llm::{StopReason, StreamEvent, Usage};
        use std::fs;

        let home = crate::testutil::temp_dir("dream-run");
        let cwd = home.join("project");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(memory::global_path(&home), "- (2026-10-17) old note\n").unwrap();
        fs::create_dir_all(home.join("memory")).unwrap();
        fs::write(
            memory::project_path(&home, &cwd),
            "- (2026-10-16) project note\n",
        )
        .unwrap();
        let global = memory::global_path(&home);
        let args = serde_json::to_string(&serde_json::json!({
            "file_path": global,
            "content": "# Long-term memory\n\n- (2026-10-17) merged note\n",
        }))
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            vec![
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "write".into(),
                },
                StreamEvent::ToolCallDelta { index: 0, args },
                StreamEvent::Stop(StopReason::ToolCalls),
            ],
            vec![
                StreamEvent::Text("merged and forgot".into()),
                StreamEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    cached_tokens: 0,
                    reasoning_tokens: 0,
                }),
                StreamEvent::Stop(StopReason::EndTurn),
            ],
        ]));
        let file_tools =
            crate::tools::specified(&["read".into(), "write".into(), "edit".into()]).unwrap();

        let outcome = run(
            provider.clone(),
            Model::unknown("mock", "mock"),
            file_tools,
            &home,
            &cwd,
            &mut (),
            Cancel::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Completed);
        let content = fs::read_to_string(memory::global_path(&home)).unwrap();
        assert!(content.contains("merged note"));
        assert!(!content.contains("old note"));
        let observed = provider.observed();
        assert_eq!(observed.len(), 2);
    }

    #[test]
    fn report_counts_what_disappeared() {
        let before = Counts {
            global: 10,
            project: 20,
        };
        let after = Counts {
            global: 8,
            project: 15,
        };
        let line = report(before, after);
        assert!(line.contains("global 10→8"));
        assert!(line.contains("project 20→15"));
        assert!(line.contains("7 notes forgotten or merged"));

        assert!(report(before, before).contains("nothing forgotten"));
    }
}
