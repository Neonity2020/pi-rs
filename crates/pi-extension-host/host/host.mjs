// Extension-host PoC bridge (zero-dependency Node script). Loads one upstream
// extension file, forwards its ExtensionAPI calls over JSONL on stdout, and
// receives events/calls from the Rust side on stdin. Protocol and success
// criteria: docs/operations/extensions-host-experiment.md §4.
//
// Frames (all JSONL):
//   host → rust: {"type":"register","capability":...} | {"type":"ready"} |
//                {"type":"error","stage":...,"message":...} |
//                {"type":"return","id":N,"ok":bool,"result"|"error"} |
//                {"type":"call","id":M,"method":...,"params":...}
//   rust → host: {"type":"event","id":N,"event":...,"payload":...} |
//                {"type":"call","id":N,"method":...,"params":...} |
//                {"type":"return","id":M,"ok":bool,"result"|"error"} |
//                {"type":"shutdown"}
import process from "node:process";
import { mkdirSync, writeFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { register } from "node:module";
import { pathToFileURL } from "node:url";
import readline from "node:readline";
import { execFile } from "node:child_process";

const extensionPath = process.argv[2];
const flags = JSON.parse(process.env.PI_EXT_FLAGS ?? "{}");

// The protocol owns stdout. Extension code writes to stdout in-process
// (upstream terminal escapes such as OSC 777) — relay those to stderr so the
// Rust side can forward them without corrupting the frame stream.
const protoWrite = process.stdout.write.bind(process.stdout);
process.stdout.write = (chunk, ...rest) => process.stderr.write(chunk, ...rest);
const frame = (obj) => protoWrite(`${JSON.stringify(obj)}\n`);

// --- shims ----------------------------------------------------------------
const TYPEBOX_SHIM = `
function withDescription(schema, opts) {
	if (opts && typeof opts === "object" && opts.description !== undefined) {
		schema.description = opts.description;
	}
	return schema;
}
export const Type = {
	Object(properties, opts) {
		const required = Object.entries(properties ?? {})
			.filter(([, v]) => !(v && v.optional))
			.map(([k]) => k);
		return withDescription({ type: "object", properties: properties ?? {}, required }, opts);
	},
	String: (opts) => withDescription({ type: "string" }, opts),
	Number: (opts) => withDescription({ type: "number" }, opts),
	Boolean: (opts) => withDescription({ type: "boolean" }, opts),
	Array: (items, opts) => withDescription({ type: "array", items }, opts),
	Optional: (schema) => ({ ...schema, optional: true }),
};
`;

const PI_TUI_SHIM = `
// TUI component-level APIs are a declared bridge limitation (no component
// channel across the process boundary). Import succeeds; using a component
// throws with an actionable message.
const unavailable = (name) => () => {
	throw new Error(\`pi-tui component "\${name}" is not available through the process bridge (declared limitation)\`);
};
export const Editor = unavailable("Editor");
export const Text = unavailable("Text");
export const Container = unavailable("Container");
export const Spacer = unavailable("Spacer");
export const SettingsList = unavailable("SettingsList");
export const visibleWidth = unavailable("visibleWidth");
export const wrapTextWithAnsi = unavailable("wrapTextWithAnsi");
export function matchesKey() {
	return unavailable("matchesKey")();
}
`;

const shimDir = path.join(os.tmpdir(), `pi-ext-host-shims-${process.pid}`);
mkdirSync(shimDir, { recursive: true });
const writeFile = (name, body) => writeFileSync(path.join(shimDir, name), body);
writeFile("pi-coding-agent.mjs", "export function defineTool(tool) {\n\treturn tool;\n}\n");
writeFile("pi-ai.mjs", TYPEBOX_SHIM);
writeFile("typebox.mjs", TYPEBOX_SHIM);
writeFile("pi-tui.mjs", PI_TUI_SHIM);
process.env.PI_EXT_SHIM_DIR = shimDir;
process.env.PI_EXT_SHIM_MAP = JSON.stringify({
	"@earendil-works/pi-coding-agent": "pi-coding-agent.mjs",
	"@earendil-works/pi-ai": "pi-ai.mjs",
	"@earendil-works/pi-tui": "pi-tui.mjs",
	typebox: "typebox.mjs",
});
register(new URL("./hooks.mjs", import.meta.url));

// --- host → rust calls -----------------------------------------------------
let seq = 1;
const pendingHostCalls = new Map();
function callHost(method, params) {
	const id = seq++;
	return new Promise((resolve, reject) => {
		pendingHostCalls.set(id, { resolve, reject, method });
		frame({ type: "call", id, method, params });
	});
}

// Upstream marks several ExtensionAPI methods synchronous (types.ts:1712-1804:
// getFlag, appendEntry, set/getSessionName, get/setActiveTools, getAllTools,
// get/setThinkingLevel). A sync method cannot await a stdio round trip, so the
// host mirrors state locally and fires one-way `action` frames that the Rust
// side applies; async APIs (ui.*, sendMessage, exec) use id-matched calls.
const sessionState = {
	name: null,
	activeTools: [],
	allTools: [],
	thinkingLevel: null,
};

const fire = (method, params) => frame({ type: "action", method, params });

// --- ExtensionAPI surface ---------------------------------------------------
const registrations = [];
const tools = new Map();
const commands = new Map();
const events = [];
const flagsSeen = new Map();

function unsupportedApi(api) {
	return (...args) => {
		frame({ type: "register", capability: "unsupported", api });
		void args;
		throw new Error(`${api} is not available through the process bridge (declared limitation)`);
	};
}

const pi = {
	registerTool(definition) {
		tools.set(definition.name, definition);
		registrations.push({ capability: "tool", name: definition.name });
		frame({
			type: "register",
			capability: "tool",
			name: definition.name,
			definition: {
				name: definition.name,
				label: definition.label,
				description: definition.description,
				parameters: definition.parameters ?? null,
				executionMode: definition.executionMode,
			},
		});
	},
	registerCommand(name, definition) {
		commands.set(name, definition);
		registrations.push({ capability: "command", name });
		frame({ type: "register", capability: "command", name, description: definition.description });
	},
	registerFlag(name, definition) {
		registrations.push({ capability: "flag", name });
		frame({ type: "register", capability: "flag", name, definition: { type: definition.type, description: definition.description } });
	},
	on(event, handler) {
		events.push({ event, handler });
		registrations.push({ capability: "event", event });
		frame({ type: "register", capability: "event", event });
	},
	getFlag(name) {
		const value = flags[name];
		flagsSeen.set(name, value ?? null);
		return value;
	},
	setSessionName(name) {
		sessionState.name = name;
		fire("session.setSessionName", { name });
	},
	getSessionName: () => sessionState.name,
	appendEntry(customType, data) {
		fire("session.appendEntry", { customType, data });
	},
	sendUserMessage: (text) => callHost("session.sendUserMessage", { text }),
	sendMessage: (text) => callHost("session.sendMessage", { text }),
	setActiveTools(list) {
		sessionState.activeTools = [...list];
		fire("tools.setActive", { tools: list });
	},
	getActiveTools: () => [...sessionState.activeTools],
	getAllTools: () => [...sessionState.allTools],
	setModel: (model) => callHost("session.setModel", { model }),
	setThinkingLevel(level) {
		sessionState.thinkingLevel = level;
		fire("session.setThinkingLevel", { level });
	},
	getThinkingLevel: () => sessionState.thinkingLevel,
	exec: (cmd, args, opts) =>
		new Promise((resolve) => {
			const child = execFile(cmd, args ?? [], { timeout: opts?.timeout ?? 30_000 }, (err, stdout, stderr) => {
				resolve({
					ok: !err,
					code: child.exitCode ?? (err?.code ?? null),
					killed: Boolean(err?.killed),
					stdout: String(stdout ?? ""),
					stderr: String(stderr ?? ""),
					error: err && !err.killed && child.exitCode === null ? String(err.message) : undefined,
				});
			});
		}),
	registerProvider: unsupportedApi("registerProvider"),
	registerMessageRenderer: unsupportedApi("registerMessageRenderer"),
	registerEntryRenderer: unsupportedApi("registerEntryRenderer"),
	registerShortcut: unsupportedApi("registerShortcut"),
};

function makeCtx(extra) {
	return {
		mode: "bridge",
		hasUI: process.env.PI_EXT_HAS_UI !== "0",
		cwd: process.env.PI_EXT_CWD ?? process.cwd(),
		ui: {
			notify: (message, level) => callHost("ui.notify", { message, level: level ?? "info" }),
			select: (title, options) => callHost("ui.select", { title, options }),
			confirm: (title) => callHost("ui.confirm", { title }),
			input: (title, placeholder) => callHost("ui.input", { title, placeholder }),
			setEditorText: (text) => callHost("ui.setEditorText", { text }),
			setStatus: (text) => callHost("ui.setStatus", { text }),
			setTitle: (text) => callHost("ui.setTitle", { text }),
		},
		sessionManager: {
			getCwd: () => process.env.PI_EXT_CWD ?? process.cwd(),
		},
		...extra,
	};
}

// --- load + factory ---------------------------------------------------------
let loaded;
try {
	loaded = await import(pathToFileURL(extensionPath).href);
} catch (error) {
	frame({ type: "error", stage: "import", message: String(error?.message ?? error) });
	process.exit(1);
}
if (typeof loaded.default !== "function") {
	frame({ type: "error", stage: "factory", message: "extension has no default factory function" });
	process.exit(1);
}
try {
	await loaded.default(pi);
} catch (error) {
	frame({ type: "error", stage: "factory", message: String(error?.message ?? error) });
	process.exit(1);
}
frame({ type: "ready" });

// --- stdin dispatch ----------------------------------------------------------
async function runTool(name, params) {
	const definition = tools.get(name);
	if (!definition) throw new Error(`unknown tool "${name}"`);
	const ctx = makeCtx({
		signal: undefined,
		abort: () => {},
		hasPendingMessages: () => false,
	});
	return await definition.execute(params.toolCallId, params.args, undefined, undefined, ctx);
}

async function runCommand(name, args) {
	const definition = commands.get(name);
	if (!definition) throw new Error(`unknown command "${name}"`);
	return await definition.handler(args ?? "", makeCtx());
}

async function runEvent(name, payload) {
	const matching = events.filter((entry) => entry.event === name);
	const outcomes = [];
	for (const entry of matching) {
		try {
			outcomes.push((await entry.handler(payload, makeCtx())) ?? null);
		} catch (error) {
			outcomes.push({ __error: String(error?.message ?? error) });
		}
	}
	return outcomes;
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
	if (!line.trim()) return;
	let message;
	try {
		message = JSON.parse(line);
	} catch {
		return;
	}
	(async () => {
		switch (message.type) {
			case "event": {
				try {
					const result = await runEvent(message.event, message.payload);
					frame({ type: "return", id: message.id, ok: true, result });
				} catch (error) {
					frame({ type: "return", id: message.id, ok: false, error: String(error?.message ?? error) });
				}
				break;
			}
			case "call": {
				try {
					const result =
						message.method === "tool.execute"
							? await runTool(message.params.name, message.params)
							: message.method === "command.execute"
								? await runCommand(message.params.name, message.params.args)
								: undefined;
					frame({ type: "return", id: message.id, ok: true, result: result ?? null });
				} catch (error) {
					frame({ type: "return", id: message.id, ok: false, error: String(error?.message ?? error) });
				}
				break;
			}
			case "return": {
				const pending = pendingHostCalls.get(message.id);
				if (pending) {
					pendingHostCalls.delete(message.id);
					if (message.ok) pending.resolve(message.result);
					else pending.reject(new Error(message.error ?? "host call failed"));
				}
				break;
			}
			case "shutdown": {
				rl.close();
				process.exit(0);
				break;
			}
			default:
				break;
		}
	})();
});
rl.on("close", () => process.exit(0));

process.on("exit", () => {
	try {
		rmSync(shimDir, { recursive: true, force: true });
	} catch {}
});
