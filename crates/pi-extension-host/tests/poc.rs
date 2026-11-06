//! Extension-host PoC success criteria (experiment doc §4): each test maps to
//! one criterion and runs a real upstream extension through the bridge.
//! Skips with a note when no Node >= 22.6 runtime is present.

use std::path::PathBuf;
use std::time::Duration;

use pi_extension_host::{ExtensionHost, HostConfig, SpawnError};
use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(15);

/// Non-erasable TS fixture (enum): only loads through the jiti rung of the
/// spawn ladder.
const NON_ERASABLE_TS: &str = include_str!("fixtures/non-erasable.ts");
const FAKE_JITI_PACKAGE_JSON: &str = include_str!("fixtures/fake-jiti/package.json");
const FAKE_JITI_INDEX: &str = include_str!("fixtures/fake-jiti/index.cjs");

fn fixture(name: &str) -> PathBuf {
	PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("tests")
		.join("fixtures")
		.join(name)
}

fn config(name: &str) -> HostConfig {
	HostConfig {
		extension: fixture(name),
		..Default::default()
	}
}

/// Returns Err(()) when the runtime is missing/too old — the test should skip.
async fn spawn_or_skip(config: HostConfig) -> Result<ExtensionHost, ()> {
	match ExtensionHost::spawn(config).await {
		Ok(host) => Ok(host),
		Err(err) if err.stage == "runtime" => {
			eprintln!("skipping PoC test: {}", err.message);
			Err(())
		}
		Err(err) => panic!("spawn failed: {err}"),
	}
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_tool_round_trip_and_schema_registration() {
	let host = match spawn_or_skip(config("hello.ts")).await {
		Ok(host) => host,
		Err(()) => return,
	};

	// Criterion: registerTool exposes the schema as serialized JSON.
	let registrations = host.registrations().await;
	assert_eq!(registrations.len(), 1, "hello registers exactly one tool");
	assert_eq!(registrations[0].capability, "tool");
	assert_eq!(registrations[0].name.as_deref(), Some("hello"));
	let definition = registrations[0]
		.definition
		.clone()
		.expect("tool definition");
	assert_eq!(definition["name"], json!("hello"));
	assert_eq!(
		definition["parameters"]["properties"]["name"]["type"],
		json!("string"),
		"Type shim produces a JSON-schema object"
	);

	// Criterion: tool.execute round trip with real extension code.
	let result = host
		.call(
			"tool.execute",
			json!({"name": "hello", "toolCallId": "t1", "args": {"name": "Pi"}}),
			TIMEOUT,
		)
		.await
		.expect("tool execute succeeds");
	assert_eq!(
		result["content"][0]["text"],
		json!("Hello, Pi!"),
		"execute output matches the upstream extension body"
	);
	assert_eq!(result["details"]["greeted"], json!("Pi"));
	host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn session_name_command_sets_and_reads_the_name() {
	let host = match spawn_or_skip(config("session-name.ts")).await {
		Ok(host) => host,
		Err(()) => return,
	};
	let registrations = host.registrations().await;
	assert!(
		registrations
			.iter()
			.any(|r| r.capability == "command" && r.name.as_deref() == Some("session-name")),
		"command registered"
	);

	// Set path: setSessionName is a sync API — it fires a one-way action
	// frame (no round trip); the notify dialog is the only host call.
	let run = host.call(
		"command.execute",
		json!({"name": "session-name", "args": "My Session"}),
		TIMEOUT,
	);
	tokio::pin!(run);
	let mut notified = None;
	loop {
		tokio::select! {
			result = &mut run => {
				result.expect("command execute succeeds");
				break;
			}
			call = host.next_host_call(TIMEOUT) => {
				let call = call.expect("host call arrives");
				assert_eq!(call.method, "ui.notify", "sync setters are actions, not calls");
				notified = Some(call.params["message"].clone());
				host.answer(call.id, Value::Null).await.unwrap();
			}
		}
	}
	while let Some(call) = host.next_host_call(Duration::from_millis(200)).await {
		if call.method == "ui.notify" {
			notified = Some(call.params["message"].clone());
		}
	}
	assert_eq!(notified, Some(json!("Session named: My Session")));

	// The sync setter landed on the Rust side as a one-way action frame.
	let actions = host.actions().await;
	assert!(
		actions.iter().any(
			|a| a.method == "session.setSessionName" && a.params["name"] == json!("My Session")
		),
		"setSessionName action recorded: {actions:?}"
	);

	// Read path: getSessionName is sync too — the mirrored cache answers
	// "My Session" without any round trip.
	let run = host.call(
		"command.execute",
		json!({"name": "session-name", "args": ""}),
		TIMEOUT,
	);
	tokio::pin!(run);
	let mut notified = None;
	loop {
		tokio::select! {
			result = &mut run => {
				result.expect("command execute succeeds");
				break;
			}
			call = host.next_host_call(TIMEOUT) => {
				let call = call.expect("host call arrives");
				assert_eq!(call.method, "ui.notify");
				notified = Some(call.params["message"].clone());
				host.answer(call.id, Value::Null).await.unwrap();
			}
		}
	}
	assert_eq!(notified, Some(json!("Session: My Session")));
	host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_extension_writes_escapes_relayed_to_stderr() {
	let host = match spawn_or_skip(config("notify.ts")).await {
		Ok(host) => host,
		Err(()) => return,
	};
	let registrations = host.registrations().await;
	assert!(
		registrations
			.iter()
			.any(|r| r.capability == "event" && r.event.as_deref() == Some("agent_settled")),
		"agent_settled listener registered"
	);

	let outcomes = host
		.dispatch_event("agent_settled", json!({}), TIMEOUT)
		.await
		.expect("event dispatch succeeds");
	assert_eq!(outcomes.len(), 1);
	assert!(outcomes[0].is_null(), "notify handler returns nothing");

	// The extension wrote an OSC 777 escape to process.stdout; the host
	// relays non-frame stdout to stderr, so the escape lands there intact.
	std::thread::sleep(Duration::from_millis(100));
	let tail = host.stderr_tail().await;
	assert!(
		tail.contains("\u{1b}]777;notify;Pi;Ready for input\u{7}"),
		"OSC 777 notification relayed via stderr, got: {tail:?}"
	);
	host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_gate_dialog_decides_the_block() {
	let host = match spawn_or_skip(config("permission-gate.ts")).await {
		Ok(host) => host,
		Err(()) => return,
	};

	let dispatch = host.dispatch_event(
		"tool_call",
		json!({"toolName": "bash", "input": {"command": "rm -rf /tmp/important"}}),
		TIMEOUT,
	);
	tokio::pin!(dispatch);
	let mut answered = false;
	let outcomes = loop {
		tokio::select! {
			result = &mut dispatch => break result.expect("event dispatch succeeds"),
			call = host.next_host_call(TIMEOUT) => {
				let call = call.expect("ui.select arrives");
				assert_eq!(call.method, "ui.select");
				assert!(call.params["title"].as_str().unwrap().contains("Dangerous command"));
				assert_eq!(call.params["options"], json!(["Yes", "No"]));
				answered = true;
				host.answer(call.id, json!("No")).await.unwrap();
			}
		}
	};
	assert!(answered, "the dialog round trip happened");
	assert_eq!(outcomes.len(), 1);
	assert_eq!(outcomes[0]["block"], json!(true));
	assert_eq!(outcomes[0]["reason"], json!("Blocked by user"));

	// "Yes" lets the tool call through — the handler returns undefined, so
	// the only outcome is null (no block object). The second dialog still
	// needs its answer, so dispatch with an answerer attached.
	let dispatch = host.dispatch_event(
		"tool_call",
		json!({"toolName": "bash", "input": {"command": "rm -rf /tmp/important"}}),
		TIMEOUT,
	);
	tokio::pin!(dispatch);
	let outcomes = loop {
		tokio::select! {
			result = &mut dispatch => break result.expect("second dispatch succeeds"),
			call = host.next_host_call(TIMEOUT) => {
				let call = call.expect("ui.select arrives");
				assert_eq!(call.method, "ui.select");
				host.answer(call.id, json!("Yes")).await.unwrap();
			}
		}
	};
	assert_eq!(outcomes.len(), 1);
	assert!(
		outcomes[0].is_null(),
		"allowing the command returns no block"
	);
	host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn flags_flow_through_register_and_getflag() {
	let config = HostConfig {
		flags: json!({"poc-mode": "fast"}),
		..config("flags.ts")
	};
	let host = match spawn_or_skip(config).await {
		Ok(host) => host,
		Err(()) => return,
	};
	let registrations = host.registrations().await;
	assert!(
		registrations
			.iter()
			.any(|r| r.capability == "flag" && r.name.as_deref() == Some("poc-mode")),
		"flag registered"
	);

	let run = host.call(
		"command.execute",
		json!({"name": "poc-flag", "args": ""}),
		TIMEOUT,
	);
	tokio::pin!(run);
	let mut notified = None;
	loop {
		tokio::select! {
			result = &mut run => {
				result.expect("command execute succeeds");
				break;
			}
			call = host.next_host_call(TIMEOUT) => {
				let call = call.expect("notify arrives");
				assert_eq!(call.method, "ui.notify");
				notified = Some(call.params["message"].clone());
				host.answer(call.id, Value::Null).await.unwrap();
			}
		}
	}
	assert_eq!(notified, Some(json!("poc-mode=fast")));

	// session_start dispatch hits the registered listener.
	let outcomes = host
		.dispatch_event("session_start", json!({}), TIMEOUT)
		.await
		.expect("session_start dispatch");
	assert_eq!(outcomes.len(), 1);
	assert!(outcomes[0].is_null());
	host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn non_erasable_ts_extension_loads_through_the_jiti_ladder() {
	// Isolated copy so the fake node_modules never lands in the source tree
	// (the no-jiti sibling test relies on nothing resolving "jiti" above the
	// checked-in fixture).
	let temp = std::env::temp_dir().join(format!(
		"pi-ext-host-jiti-test-{}-{}",
		std::process::id(),
		std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_nanos()
	));
	std::fs::create_dir_all(temp.join("node_modules").join("jiti")).expect("create fake jiti tree");
	std::fs::write(temp.join("extension.ts"), NON_ERASABLE_TS).expect("write fixture copy");
	std::fs::write(
		temp.join("node_modules").join("jiti").join("package.json"),
		FAKE_JITI_PACKAGE_JSON,
	)
	.expect("write fake jiti package.json");
	std::fs::write(
		temp.join("node_modules").join("jiti").join("index.cjs"),
		FAKE_JITI_INDEX,
	)
	.expect("write fake jiti index.cjs");

	let config = HostConfig {
		extension: temp.join("extension.ts"),
		cwd: temp.clone(),
		..Default::default()
	};
	let host = match spawn_or_skip(config).await {
		Ok(host) => host,
		Err(()) => return,
	};

	// Spawn succeeding proves the ladder reached the jiti rung: the enum
	// cannot load through native strip-only typing (rungs 1-2 fail with
	// "TypeScript enum is not supported in strip-only mode").
	let registrations = host.registrations().await;
	assert!(
		registrations
			.iter()
			.any(|r| r.capability == "tool" && r.name.as_deref() == Some("color")),
		"enum-backed tool registered: {registrations:?}"
	);
	assert!(
		registrations
			.iter()
			.any(|r| r.capability == "event" && r.event.as_deref() == Some("session_start")),
		"event registration frames are unchanged in jiti mode: {registrations:?}"
	);

	// Same call/return protocol as the strip-loaded fixtures, with enum
	// member values that only exist after a real TS transform.
	let result = host
		.call(
			"tool.execute",
			json!({"name": "color", "toolCallId": "t1", "args": {"which": "green"}}),
			TIMEOUT,
		)
		.await
		.expect("tool execute succeeds on the jiti-loaded module");
	assert_eq!(result["content"][0]["text"], json!("color=green"));
	assert_eq!(result["details"]["color"], json!("green"));

	let outcomes = host
		.dispatch_event("session_start", json!({}), TIMEOUT)
		.await
		.expect("event dispatch succeeds on the jiti-loaded module");
	assert_eq!(outcomes.len(), 1);
	assert!(outcomes[0].is_null());
	let actions = host.actions().await;
	assert!(
		actions
			.iter()
			.any(|a| a.method == "session.appendEntry" && a.params["data"]["red"] == json!("red")),
		"appendEntry action carries the enum value: {actions:?}"
	);

	host.shutdown().await;
	let _ = std::fs::remove_dir_all(&temp);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_erasable_ts_extension_without_jiti_reports_a_readable_strip_error() {
	match ExtensionHost::spawn(config("non-erasable.ts")).await {
		Ok(_) => panic!("strip-only loading must refuse a TS enum"),
		Err(SpawnError { stage, message }) if stage == "runtime" => {
			eprintln!("skipping PoC test: {message}");
		}
		Err(SpawnError { stage, message }) => {
			assert_eq!(
				stage, "import",
				"without jiti the raw strip failure keeps the import stage: {stage} {message}"
			);
			assert!(
				message.to_lowercase().contains("enum"),
				"error names the unsupported enum syntax: {message}"
			);
			assert!(
				message.contains("jiti"),
				"exhausted-ladder hint points at installing jiti: {message}"
			);
		}
	}
}

#[tokio::test(flavor = "multi_thread")]
async fn factory_throw_reports_a_hard_error() {
	let err = match ExtensionHost::spawn(config("factory-throw.ts")).await {
		Ok(_) => panic!("factory throw must fail the spawn"),
		Err(SpawnError { stage, message }) if stage == "runtime" => {
			eprintln!("skipping PoC test: {message}");
			return;
		}
		Err(err) => err,
	};
	assert_eq!(err.stage, "factory");
	assert!(
		err.message.contains("factory exploded on purpose"),
		"the fixture's own error surfaces: {err}"
	);
}

#[tokio::test(flavor = "multi_thread")]
async fn handler_throw_is_contained_and_host_stays_alive() {
	let host = match spawn_or_skip(config("handler-throw.ts")).await {
		Ok(host) => host,
		Err(()) => return,
	};

	let err = host
		.call(
			"command.execute",
			json!({"name": "boom", "args": ""}),
			TIMEOUT,
		)
		.await
		.expect_err("the throwing command reports an error");
	assert!(
		err.contains("boom"),
		"error text carries the fixture message: {err}"
	);

	// The same host process still serves other commands.
	let run = host.call(
		"command.execute",
		json!({"name": "alive", "args": ""}),
		TIMEOUT,
	);
	tokio::pin!(run);
	loop {
		tokio::select! {
			result = &mut run => {
				result.expect("host alive after handler failure");
				break;
			}
			call = host.next_host_call(TIMEOUT) => {
				let call = call.expect("notify arrives");
				assert_eq!(call.method, "ui.notify");
				host.answer(call.id, Value::Null).await.unwrap();
			}
		}
	}
	host.shutdown().await;
}
