// Normalization rules for differential artifacts. The goal: strip fields that
// are allowed to differ between implementations (ids, timestamps, durations,
// system prompts) while keeping every semantically meaningful field (roles,
// content, tool calls, usage, stop reasons, event order).
//
// The canonical view is deliberately implementation-neutral: both sides'
// session files and captured requests are projected into the same shape, and
// the comparator only ever sees canonical views.

const ID_KEYS = new Set([
	"id",
	"sessionId",
	"entryId",
	"messageId",
	"parentEntryId",
	"callId",
	"toolCallId",
]);
// keys whose exact value may legitimately differ (implementation metadata)
const DROP_KEYS = new Set([
	"timestamp",
	"duration",
	"durationMs",
	"cost",
	"version",
	"cwd",
	"gitBranch",
	"api",
	// usage detail keys that depend on provider-side caching behavior only
	// when the server does not send them; we keep token counts.
]);

export function canonicalize(value) {
	if (Array.isArray(value)) return value.map(canonicalize);
	if (value && typeof value === "object") {
		const out = {};
		for (const [k, v] of Object.entries(value)) {
			if (DROP_KEYS.has(k)) continue;
			if (ID_KEYS.has(k)) continue;
			out[k] = canonicalize(v);
		}
		return out;
	}
	return value;
}

// ---- session.jsonl → canonical message list -------------------------------
// Each line is an entry; non-message entries (compaction, context edits,
// headers) are projected to {type} only, so structure and order still compare
// but per-implementation metadata does not.

export function canonicalSession(text) {
	const out = [];
	for (const line of text.split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		let entry;
		try {
			entry = JSON.parse(trimmed);
		} catch {
			out.push({ type: "<unparseable>" });
			continue;
		}
		const canonical = canonicalEntry(entry);
		if (canonical !== null) out.push(canonical);
	}
	return out;
}

function canonicalEntry(entry) {
	const type = entry.type || (entry.role ? "message" : "<unknown>");
	if (type === "header") return { type: "header" };
	if (type === "message" && entry.message) {
		// System messages are an implementation detail at P0: the TS pi
		// persists them (with sections), pi-rs re-seeds the system prompt on
		// resume instead (recorded divergence). Drop them entirely so
		// transcript alignment is not shifted by their presence; the
		// persistence divergence is tracked in the compat matrix.
		if (entry.message.role === "system") return null;
		return { type: "message", message: canonicalMessage(entry.message) };
	}
	if (entry.message && (entry.message.role === "user" || entry.message.role === "assistant")) {
		return { type: "message", message: canonicalMessage(entry.message) };
	}
	return { type };
}

export function canonicalMessage(message) {
	const role = message.role;
	const out = { role };
	const content = canonicalContent(message.content);
	if (content !== undefined) out.content = content;
	if (Array.isArray(message.toolCalls) && message.toolCalls.length > 0) {
		out.toolCalls = message.toolCalls
			.map((c) => ({
				name: c.name,
				arguments: sortKeys(parseMaybeJson(c.arguments ?? c.argumentsJson)),
			}))
			.sort((a, b) => a.name.localeCompare(b.name) || JSON.stringify(a.arguments).localeCompare(JSON.stringify(b.arguments)));
	}
	if (message.stopReason !== undefined) out.stopReason = message.stopReason;
	if (message.usage) {
		const usage = canonicalize(message.usage);
		// The TS pi always serializes reasoning tokens (0 when unreported);
		// pi-rs omits the field when the provider sends none. Missing = 0.
		if (usage.reasoning === undefined) usage.reasoning = 0;
		out.usage = usage;
	}
	if (message.providerId !== undefined) out.providerId = message.providerId;
	if (message.modelId !== undefined) out.modelId = message.modelId;
	return out;
}

function canonicalContent(content) {
	if (content === undefined || content === null) return undefined;
	if (typeof content === "string") return content;
	if (Array.isArray(content)) {
		// content blocks: keep text blocks and tool-call/tool-result blocks in order
		return content.map((block) => {
			if (typeof block === "string") return block;
			const b = { ...block };
			for (const k of Object.keys(b)) {
				if (ID_KEYS.has(k) || DROP_KEYS.has(k)) delete b[k];
			}
			if (typeof b.arguments === "string") {
				try {
					b.arguments = sortKeys(JSON.parse(b.arguments));
				} catch {
					/* keep raw */
				}
			}
			if (typeof b.input === "string") {
				try {
					b.input = sortKeys(JSON.parse(b.input));
				} catch {
					/* keep raw */
				}
			}
			return canonicalize(b);
		});
	}
	return canonicalize(content);
}

function parseMaybeJson(value) {
	if (typeof value !== "string") return value ?? null;
	try {
		return JSON.parse(value);
	} catch {
		return { "<raw>": value };
	}
}

function sortKeys(value) {
	if (Array.isArray(value)) return value.map(sortKeys);
	if (value && typeof value === "object") {
		const out = {};
		for (const k of Object.keys(value).sort()) out[k] = sortKeys(value[k]);
		return out;
	}
	return value;
}

// ---- --mode json stdout → canonical event list -----------------------------
// Both sides stream session events as JSONL after the leading session header
// line (print-mode.ts json branch / pi-rs print.rs Json mode). The canonical
// view keeps the event-type sequence and semantic payloads, dropping volatile
// metadata (ids, timestamps, cwd, cost). Two deliberate collapsions:
// - consecutive `tool_execution_update` frames collapse to one marker (tools
//   may chunk streamed output differently between implementations);
// - `message_update` keeps the stripped assistantMessageEvent (no `partial`)
//   and the cumulative usage, exactly like the wire shape.

export function canonicalJsonEvents(text) {
	const events = [];
	for (const line of text.split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		let e;
		try {
			e = JSON.parse(trimmed);
		} catch {
			events.push({ type: "<unparseable>" });
			continue;
		}
		if (e.type === "session") {
			events.push({ type: "session", version: e.version });
			continue;
		}
		if (e.type === "agent_end" && Array.isArray(e.messages)) {
			const end = {
				type: "agent_end",
				// The TS transcript includes the system message; pi-rs re-seeds
				// the system prompt on resume (same rule as the session view).
				messages: e.messages.filter((m) => m.role !== "system").map(canonicalMessage),
			};
			if (e.willRetry !== undefined) end.willRetry = e.willRetry;
			events.push(end);
			continue;
		}
		// System messages are an implementation detail (the TS pi announces
		// and persists them; pi-rs re-seeds the system prompt on resume) —
		// same rule as the session-view canonicalization above.
		if ((e.type === "message_start" || e.type === "message_end") && e.message?.role === "system") {
			continue;
		}
		if ((e.type === "message_start" || e.type === "message_end") && e.message) {
			if (e.type === "message_start") {
				// The start snapshot races the stream: upstream spreads a live
				// message reference, so what content it shows depends on how
				// far the provider parser ran before the consumer resumed
				// (locally it shows the first chunk's tool call; pi-rs
				// snapshots per event and shows none). The authoritative
				// content lands at message_end.
				events.push({
					type: "message_start",
					message: { role: e.message.role },
				});
				continue;
			}
			events.push({ type: e.type, message: canonicalMessage(e.message) });
			continue;
		}
		if (e.type === "turn_end" && e.message) {
			const end = { type: "turn_end", message: canonicalMessage(e.message) };
			if (e.toolResults !== undefined) end.toolResults = canonicalize(e.toolResults);
			events.push(end);
			continue;
		}
		if (e.type === "message_update" && e.assistantMessageEvent) {
			// `usage` on streaming updates is a timing artifact of the local
			// replay: upstream's provider stream hands the consumer a live
			// message reference, so when the scripted SSE arrives in one TCP
			// segment the parser drains before the consumer spreads it and
			// every delta already shows the final usage. Over a real network
			// (and in pi-rs, which snapshots per event) usage lands with the
			// terminal chunk on both sides. Compare deltas, not mid-stream
			// usage; message_end / agent_end carry the authoritative totals.
			events.push({
				type: "message_update",
				assistantMessageEvent: canonicalize(e.assistantMessageEvent),
			});
			continue;
		}
		if (e.type === "tool_execution_update") {
			collapseInto(events, { type: "tool_execution_update" }, (prev) => prev.type === "tool_execution_update");
			continue;
		}
		events.push(canonicalize(e));
	}
	return events;
}

/** Push an event unless the previous event is the same collapsed kind. */
function collapseInto(events, event, sameAsPrevious) {
	const prev = events[events.length - 1];
	if (prev && sameAsPrevious(prev)) return;
	events.push(event);
}

// ---- captured provider requests → canonical request list ------------------
// System prompts are implementation-defined at P0 (they legitimately differ);
// everything else in the message transcript must match.

export function canonicalRequest(text) {
	const body = JSON.parse(text);
	return {
		model: body.model,
		stream: body.stream === true,
		toolNames: (body.tools || [])
			.map((t) => t?.function?.name || t?.name)
			.filter(Boolean)
			.sort(),
		messages: (body.messages || []).map((m) => {
			const role = m.role === "tool" ? "toolResult" : m.role;
			if (role === "system") return { role };
			return { role, content: canonicalContent(m.content) };
		}),
	};
}
