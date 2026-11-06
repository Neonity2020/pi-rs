/**
 * ui.* decoration fixture (round 43): exercises the fire-and-forget
 * `setEditorText`/`setStatus`/`setTitle` calls without awaiting them — the
 * frames trail the command's return frame, which is the drain-loop path.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.registerCommand("decorate", {
		description: "Exercise ui.setStatus/setTitle/setEditorText",
		handler: async (_args, ctx) => {
			ctx.ui.setStatus("files", "watching files");
			ctx.ui.setTitle("pi-ext-title");
			ctx.ui.setEditorText("prefilled text");
			ctx.ui.notify("decorated", "info");
		},
	});
}
