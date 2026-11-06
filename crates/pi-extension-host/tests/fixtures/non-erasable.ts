/**
 * Non-erasable TS syntax fixture: a TS `enum` is not erasable, so Node's
 * strip-only type loader refuses this file ("TypeScript enum is not supported
 * in strip-only mode"). It can only load through the jiti fallback of the
 * spawn ladder — upstream loads every extension through jiti for exactly
 * this reason (loader.ts loadExtensionModule).
 */

import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";

enum Color {
	Red = "red",
	Green = "green",
}

const colorTool = defineTool({
	name: "color",
	label: "Color",
	description: "Reports one of the enum-backed colors",
	parameters: Type.Object({
		which: Type.String({ description: "red or green" }),
	}),

	async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
		const color = params.which === "green" ? Color.Green : Color.Red;
		return {
			content: [{ type: "text", text: `color=${color}` }],
			details: { color },
		};
	},
});

export default function (pi: ExtensionAPI) {
	pi.registerTool(colorTool);
	pi.on("session_start", async () => {
		pi.appendEntry("color-check", { red: Color.Red });
		return null;
	});
}
