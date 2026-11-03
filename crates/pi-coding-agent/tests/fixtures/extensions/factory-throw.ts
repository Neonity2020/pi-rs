/**
 * Factory-phase failure fixture: throws during registration so the spawn
 * reports a hard error and the assembly continues without this extension.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	void pi;
	throw new Error("factory exploded on purpose");
}
