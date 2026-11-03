/**
 * session_start logging fixture: notifies `start:<reason>` so tests can
 * observe the re-fired session_start events across session replacement.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.on("session_start", async (event, ctx) => {
		ctx.ui.notify(`start:${event.reason}`, "info");
	});
}
