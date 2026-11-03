/**
 * Directory-entry fixture: the directory form of extension discovery (an
 * index.ts entry among other files that are not entries).
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.registerCommand("dir-index-command", {
		description: "command from the directory-entry fixture",
		handler: async () => {},
	});
}
