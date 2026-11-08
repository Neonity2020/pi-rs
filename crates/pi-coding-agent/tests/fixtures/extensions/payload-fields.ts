/**
 * Payload-field fixture (audit F1 round): logs the session_start/input/
 * turn_start/turn_end/session_shutdown payload fields as notifications so
 * tests can assert the new fields reach extension handlers.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.on("session_start", async (event, ctx) => {
		ctx.ui.notify(
			`start:${event.reason}:prev=${event.previousSessionFile ?? "none"}`,
			"info",
		);
	});
	pi.on("input", async (event, ctx) => {
		ctx.ui.notify(
			`input:${event.source}:sb=${event.streamingBehavior ?? "none"}`,
			"info",
		);
	});
	pi.on("turn_start", async (event, ctx) => {
		ctx.ui.notify(`turnstart:${event.turnIndex}:${typeof event.timestamp}`, "info");
	});
	pi.on("turn_end", async (event, ctx) => {
		ctx.ui.notify(`turnend:${event.turnIndex}`, "info");
	});
	pi.on("session_shutdown", async (event, ctx) => {
		ctx.ui.notify(`shutdown:${event.reason}`, "info");
	});
}
