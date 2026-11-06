/**
 * Dialog + prompt-injection fixtures (round 42): `ui.select`/`ui.confirm`
 * round trips through the bridge's dialog registry, and a
 * `pi.sendUserMessage` command for the prompt-injection path.
 */

import { Type } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.registerCommand("ask", {
		description: "Ask via ui.select",
		handler: async (_args, ctx) => {
			const choice = await ctx.ui.select("Pick a flavor", ["vanilla", "chocolate"]);
			ctx.ui.notify(`picked: ${choice ?? "nothing"}`, "info");
		},
	});

	pi.registerCommand("prompt-me", {
		description: "Inject a user prompt",
		handler: async () => {
			pi.sendUserMessage("hello from extension");
		},
	});

	pi.registerTool({
		name: "gate",
		label: "Gate",
		description: "Asks via ui.confirm",
		parameters: Type.Object({}),

		async execute(_toolCallId, _params, _signal, _onUpdate, ctx) {
			const allowed = await ctx.ui.confirm("Allow?");
			return {
				content: [{ type: "text", text: allowed ? "allowed" : "blocked" }],
			};
		},
	});
}
