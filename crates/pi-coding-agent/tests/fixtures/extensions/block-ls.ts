/**
 * tool_call interception fixture: blocks the `ls` tool with a hasUI-aware
 * reason (the permission-gate fail-closed pattern, pinned to the tool the
 * mock provider actually calls).
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.on("tool_call", async (event, ctx) => {
		if (event.toolName !== "ls") return undefined;
		if (!ctx.hasUI) {
			return { block: true, reason: "ls blocked by extension (no UI)" };
		}
		const choice = await ctx.ui.select("Allow ls?", ["Yes", "No"]);
		if (choice !== "Yes") {
			return { block: true, reason: "ls blocked by user" };
		}
		return undefined;
	});
}
