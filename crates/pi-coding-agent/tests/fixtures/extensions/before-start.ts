/**
 * before_agent_start counting fixture (round 43, audit P1-B3): notifies once
 * per dispatch so a session-level test can assert the per-run fire rate.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

let count = 0;

export default function (pi: ExtensionAPI) {
	pi.on("before_agent_start", async (event, ctx) => {
		count += 1;
		ctx.ui.notify(`bas:${count}:${event.messageType}:${event.prompt}`, "info");
	});
}
