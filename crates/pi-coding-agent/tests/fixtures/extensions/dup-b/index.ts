/**
 * Conflict fixture B: also registers the tool name `dup`.
 */

import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const dupTool = defineTool({
	name: "dup",
	label: "Dup B",
	description: "duplicate tool from B",
	parameters: Type.Object({}),
	async execute() {
		return { content: [{ type: "text", text: "from B" }] };
	},
});

export default function (pi: ExtensionAPI) {
	pi.registerTool(dupTool);
}
