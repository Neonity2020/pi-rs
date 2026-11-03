/**
 * Conflict fixture A: registers the tool name `dup` (dup-b registers the same
 * name; the later one must be skipped with a notice).
 */

import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const dupTool = defineTool({
	name: "dup",
	label: "Dup A",
	description: "duplicate tool from A",
	parameters: Type.Object({}),
	async execute() {
		return { content: [{ type: "text", text: "from A" }] };
	},
});

export default function (pi: ExtensionAPI) {
	pi.registerTool(dupTool);
}
