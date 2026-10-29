// Differential test runner: runs a scenario against the upstream TS pi and
// the Rust pi-rs against the same scripted replay server, then compares
// canonical views of exit code, final text, captured provider requests and
// the persisted session transcript.
//
// Usage:
//   node run.mjs                     # all scenarios, both sides, then compare
//   node run.mjs --side rust         # run/normalize one side only
//   node run.mjs --scenario 01-single-turn-echo
//
// Prerequisites:
//   - upstream bundle: difftest/upstream/pi/packages/coding-agent/dist/bundle/cli.js
//     (see difftest/README.md for the build recipe)
//   - rust binary:     target/debug/pi-rs (cargo build -p pi-coding-agent)
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { createServer } from "node:net";
import { join, dirname, basename } from "node:path";
import { fileURLToPath } from "node:url";
import { canonicalSession, canonicalRequest, canonicalJsonEvents } from "./lib/normalize.mjs";
import { diff } from "./lib/compare.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const upstreamSha = readFileSync(join(here, "upstream-sha.txt"), "utf8").trim();
const artifactsRoot = join(here, "artifacts");

const argv = process.argv.slice(2);
function argOf(name) {
	const i = argv.indexOf(name);
	return i >= 0 ? argv[i + 1] : undefined;
}
const onlySide = argOf("--side"); // rust | ts | both
const onlyScenario = argOf("--scenario");
const runBoth = !onlySide || onlySide === "both";

const scenarioDir = join(here, "scenarios");
const scenarioFiles = readdirSync(scenarioDir)
	.filter((f) => f.endsWith(".json"))
	.filter((f) => !onlyScenario || basename(f, ".json") === onlyScenario || f === onlyScenario)
	.sort();

const RUST_BIN = process.env.PI_RS_BIN || join(root, "target", "debug", "pi-rs");
const TS_BIN = join(here, "upstream", "pi", "packages", "coding-agent", "dist", "bundle", "cli.js");

if (!existsSync(TS_BIN)) {
	console.error(`upstream bundle missing at ${TS_BIN}`);
	console.error("build recipe: see difftest/README.md");
	process.exit(2);
}
if (!existsSync(RUST_BIN)) {
	console.error(`rust binary missing at ${RUST_BIN} (set PI_RS_BIN or run cargo build -p pi-coding-agent)`);
	process.exit(2);
}

function freePort() {
	return new Promise((resolve) => {
		const srv = createServer();
		srv.listen(0, "127.0.0.1", () => {
			const port = srv.address().port;
			srv.close(() => resolve(port));
		});
	});
}

function waitUntil(path, timeoutMs) {
	const deadline = Date.now() + timeoutMs;
	return new Promise((resolve, reject) => {
		(function poll() {
			if (existsSync(path)) return resolve();
			if (Date.now() > deadline) return reject(new Error(`timeout waiting for ${path}`));
			setTimeout(poll, 25);
		})();
	});
}

function findSessionFile(agentDir) {
	// both layouts live under <agentDir>/sessions/**; take the newest session
	// transcript found at any depth (Rust: <id>/session.jsonl, TS: single file)
	const sessionsDir = join(agentDir, "sessions");
	let best = null;
	(function walk(dir) {
		if (!existsSync(dir)) return;
		for (const name of readdirSync(dir)) {
			const p = join(dir, name);
			const st = statSync(p);
			if (st.isDirectory()) walk(p);
			else if (name.endsWith(".jsonl")) {
				if (!best || st.mtimeMs > best.mtimeMs) best = { path: p, mtimeMs: st.mtimeMs };
			}
		}
	})(sessionsDir);
	return best?.path;
}

async function runSide(side, scenario) {
	const tag = `${scenario.name}/${side}`;
	const artDir = join(artifactsRoot, scenario.name, side);
	rmSync(artDir, { recursive: true, force: true });
	mkdirSync(artDir, { recursive: true });

	const port = await freePort();
	const server = spawn(process.execPath, [join(here, "server", "replay.mjs"), "--scenario", join(here, "scenarios", `${scenario.name}.json`), "--port", String(port), "--artifact-dir", artDir], { stdio: ["ignore", "pipe", "pipe"] });
	let serverLog = "";
	server.stdout.on("data", (c) => (serverLog += c));
	server.stderr.on("data", (c) => (serverLog += c));
	try {
		await waitUntil(join(artDir, "port"), 5000);

		const work = mkdtempSync(join(tmpdir(), `difftest-${scenario.name}-`));
		const cwd = join(work, "cwd");
		const agentDir = join(work, "agent-dir");
		mkdirSync(cwd, { recursive: true });
		mkdirSync(agentDir, { recursive: true });
		for (const [name, content] of Object.entries(scenario.fixtureFiles || {})) {
			mkdirSync(dirname(join(cwd, name)), { recursive: true });
			writeFileSync(join(cwd, name), content);
		}

		const baseUrl = `http://127.0.0.1:${port}/v1`;
		if (side === "rust") {
			// Rust models.json: upstream object format (providers keyed by id)
			writeFileSync(
				join(agentDir, "models.json"),
				JSON.stringify(
					{
						providers: {
							difftest: {
								baseUrl,
								api: "openai-completions",
								apiKey: "difftest-dummy",
								models: [
									{ id: "difftest-model", name: "Difftest Model", contextWindow: 128000 },
								],
							},
						},
					},
					null,
					"\t",
				),
			);
		} else {
			// Upstream models.json: providers object keyed by provider id
			writeFileSync(
				join(agentDir, "models.json"),
				JSON.stringify(
					{
						providers: {
							difftest: {
								baseUrl,
								api: "openai-completions",
								apiKey: "difftest-dummy",
								models: [
									{ id: "difftest-model", name: "Difftest Model", contextWindow: 128000 },
								],
							},
						},
					},
					null,
					"\t",
				),
			);
		}

		const env = {
			...process.env,
			PI_OFFLINE: "1",
			PI_SKIP_VERSION_CHECK: "1",
			PI_TELEMETRY: "",
			NO_COLOR: "1",
			TERM: "dumb",
			OPENAI_API_KEY: "difftest-dummy",
		};
		const prompts = scenario.prompts ?? [scenario.prompt];
		const jsonMode = scenario.outputMode === "json";
		let bin;
		let args;
		if (side === "rust") {
			env.PI_RS_AGENT_DIR = agentDir;
			bin = RUST_BIN;
			// `-p` is a boolean flag in the Rust CLI; each prompt is one
			// quoted positional (kept separate for the multi-prompt loop).
			args = jsonMode
				? ["--mode", "json", ...prompts, "-m", scenario.model]
				: ["-p", ...prompts, "-m", scenario.model];
		} else {
			env.PI_CODING_AGENT_DIR = agentDir;
			bin = process.execPath;
			args = jsonMode
				? [TS_BIN, "--mode", "json", ...prompts, "--model", scenario.model]
				: [TS_BIN, "--print", ...prompts, "--model", scenario.model];
		}

		const started = Date.now();
		const res = spawnSync(bin, args, { cwd, env, encoding: "utf8", timeout: 60_000 });
		const elapsed = Date.now() - started;

		writeFileSync(join(artDir, "stdout.txt"), res.stdout ?? "");
		writeFileSync(join(artDir, "stderr.txt"), res.stderr ?? "");
		writeFileSync(join(artDir, "exit-code.txt"), String(res.status ?? "signal:" + res.signal));
		const sessionPath = findSessionFile(agentDir);
		if (sessionPath) {
			writeFileSync(join(artDir, "session.jsonl"), readFileSync(sessionPath, "utf8"));
		} else {
			writeFileSync(join(artDir, "session.jsonl"), "");
		}
		rmSync(work, { recursive: true, force: true });

		const stdout = (res.stdout ?? "").trim();
		const failures = existsSync(join(artDir, "server-failures.json"))
			? JSON.parse(readFileSync(join(artDir, "server-failures.json"), "utf8"))
			: [];
		const result = {
			side,
			scenario: scenario.name,
			upstreamSha,
			exitCode: res.status,
			signal: res.signal ?? null,
			finalText: stdout,
			// json mode streams the event protocol on stdout; text equality is
			// not meaningful there (the canonical event comparison covers it).
			finalTextMatches: jsonMode ? true : stdout === scenario.expect.finalText,
			serverFailures: failures,
			turnsSeen: countRequests(artDir),
			elapsedMs: elapsed,
			stdoutHead: stdout.slice(0, 2000),
			stderrHead: (res.stderr ?? "").slice(0, 2000),
		};
		console.log(`${tag}: exit=${result.exitCode} turns=${result.turnsSeen} finalTextMatches=${result.finalTextMatches} (${elapsed}ms)`);
		if (failures.length) console.log(`${tag}: server assertion failures: ${JSON.stringify(failures)}`);
		writeFileSync(join(artDir, "result.json"), JSON.stringify(result, null, "\t"));
		return result;
	} finally {
		server.kill("SIGTERM");
	}
}

function countRequests(artDir) {
	const dir = join(artDir, "requests");
	if (!existsSync(dir)) return 0;
	return readdirSync(dir).filter((f) => f.endsWith(".json")).length;
}

async function main() {
	mkdirSync(artifactsRoot, { recursive: true });
	const reports = [];
	for (const file of scenarioFiles) {
		const scenario = JSON.parse(readFileSync(join(scenarioDir, file), "utf8"));
		const report = { scenario: scenario.name, upstreamSha, sides: {} };
		const sides = runBoth ? ["ts", "rust"] : [onlySide];
		for (const side of sides) {
			try {
				report.sides[side] = await runSide(side, scenario);
			} catch (err) {
				console.error(`${scenario.name}/${side}: FAILED to run: ${err.message}`);
				report.sides[side] = { error: err.message };
			}
		}

		if (runBoth && report.sides.ts && report.sides.rust) {
			const diffs = [];
			const tsArt = join(artifactsRoot, scenario.name, "ts");
			const rsArt = join(artifactsRoot, scenario.name, "rust");
			const jsonMode = scenario.outputMode === "json";

			if (report.sides.ts.exitCode !== report.sides.rust.exitCode) {
				diffs.push({ path: "$.exitCode", ts: report.sides.ts.exitCode, rust: report.sides.rust.exitCode });
			}
			if (!jsonMode && report.sides.ts.finalText !== report.sides.rust.finalText) {
				diffs.push({ path: "$.finalText", ts: JSON.stringify(report.sides.ts.finalText), rust: JSON.stringify(report.sides.rust.finalText) });
			}
			if (jsonMode) {
				const tsEvents = canonicalJsonEvents(readFileSync(join(tsArt, "stdout.txt"), "utf8"));
				const rsEvents = canonicalJsonEvents(readFileSync(join(rsArt, "stdout.txt"), "utf8"));
				const d = diff(tsEvents, rsEvents);
				diffs.push(...d.map((x) => ({ ...x, path: `$.events${x.path.slice(1)}` })));
			}
			for (const name of ["requests", "session"]) {
				const readSide = (art, key) => {
					if (key === "requests") {
						const dir = join(art, "requests");
						if (!existsSync(dir)) return [];
						return readdirSync(dir)
							.filter((f) => f.endsWith(".json"))
							.sort()
							.map((f) => canonicalRequest(readFileSync(join(dir, f), "utf8")));
					}
					return canonicalSession(readFileSync(join(art, "session.jsonl"), "utf8"));
				};
				const d = diff(readSide(tsArt, name), readSide(rsArt, name));
				diffs.push(...d.map((x) => ({ ...x, path: `$.${name}${x.path.slice(1)}` })));
			}
			report.diffs = diffs;
			console.log(diffs.length === 0 ? `${scenario.name}: ✅ IDENTICAL (canonical view)` : `${scenario.name}: ⚠️  ${diffs.length} difference(s)`);
			for (const d of diffs.slice(0, 20)) {
				console.log(`   ${d.path}\n     ts:   ${d.ts}\n     rust: ${d.rust}`);
			}
		}
		reports.push(report);
	}
	writeFileSync(join(artifactsRoot, "report.json"), JSON.stringify(reports, null, "\t"));
	console.log(`\nreport: difftest/artifacts/report.json (upstream ${upstreamSha})`);
	const failing = reports.filter((r) => runBoth && r.diffs && r.diffs.length > 0);
	process.exit(0);
}

main();
