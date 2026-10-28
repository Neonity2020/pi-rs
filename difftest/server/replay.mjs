// Replay server for differential testing: serves scripted OpenAI-compatible
// SSE responses from a scenario JSON, records every request it receives.
// Zero-dependency Node. Usage:
//   node server/replay.mjs --scenario <path> --port <n> --artifact-dir <dir>
import { createServer } from "node:http";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const args = process.argv.slice(2);
function argOf(name) {
	const i = args.indexOf(name);
	return i >= 0 ? args[i + 1] : undefined;
}
const scenarioPath = argOf("--scenario");
const port = Number(argOf("--port") || 0);
const artifactDir = argOf("--artifact-dir") || "artifacts/replay";

const scenario = JSON.parse(readFileSync(scenarioPath, "utf8"));
mkdirSync(join(artifactDir, "requests"), { recursive: true });

const state = { turn: 0 };
const failures = [];

// OpenAI wire uses role "tool" for tool results; pi normalizes to toolResult.
// Accept either when asserting the last message role.
function canonicalRole(message) {
	if (message.role === "tool") return "toolResult";
	return message.role;
}

const server = createServer((req, res) => {
	if (req.method !== "POST") {
		res.writeHead(404).end();
		return;
	}
	const chunks = [];
	req.on("data", (c) => chunks.push(c));
	req.on("end", () => {
		const raw = Buffer.concat(chunks).toString("utf8");
		const index = state.turn++;
		writeFileSync(join(artifactDir, "requests", String(index).padStart(2, "0") + ".json"), raw);
		let body = {};
		try {
			body = JSON.parse(raw);
		} catch {
			failures.push({ request: index, error: "unparseable body" });
			res.writeHead(400, { "content-type": "application/json" });
			res.end(JSON.stringify({ error: { message: "unparseable body" } }));
			return;
		}

		const turn = scenario.turns[index];
		if (!turn) {
			failures.push({ request: index, error: "no scripted turn left" });
			res.writeHead(500, { "content-type": "application/json" });
			res.end(JSON.stringify({ error: { message: "no scripted turn left" } }));
			return;
		}

		// Request assertions are recorded as failures, not fatal: the comparator
		// reports them per side so both sides' results stay visible.
		const a = turn.assertRequest || {};
		const messages = Array.isArray(body.messages) ? body.messages : [];
		const last = messages[messages.length - 1] || {};
		const checks = [
			["model", body.model, a.model],
			["stream", body.stream, a.stream],
			["lastMessageRole", canonicalRole(last), a.lastMessageRole],
		];
		for (const [name, actual, expected] of checks) {
			if (expected !== undefined && actual !== expected) {
				failures.push({ request: index, assertion: name, expected, actual });
			}
		}

		res.writeHead(200, {
			"content-type": "text/event-stream",
			"cache-control": "no-cache",
			connection: "keep-alive",
		});
		for (const chunk of turn.sse) {
			res.write(`data: ${JSON.stringify(chunk)}\n\n`);
		}
		res.write("data: [DONE]\n\n");
		res.end();
	});
});

server.listen(port, "127.0.0.1", () => {
	const actualPort = server.address().port;
	// the runner polls for this file to know the server is ready
	writeFileSync(join(artifactDir, "port"), String(actualPort));
	console.log(`replay server listening on 127.0.0.1:${actualPort}`);
});

function writeFailures() {
	writeFileSync(join(artifactDir, "server-failures.json"), JSON.stringify(failures, null, "\t"));
}
process.on("SIGTERM", () => {
	writeFailures();
	process.exit(0);
});
process.on("exit", writeFailures);
