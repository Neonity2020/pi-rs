/**
 * PoC-authored fixture (modeled on examples/extensions/preset.ts flag usage):
 * registerFlag + getFlag + a command that reports the flag, and a
 * session_start listener so event dispatch is exercised end to end.
 */

export default function (pi) {
	pi.registerFlag("poc-mode", {
		description: "PoC mode selector",
		type: "string",
	});

	pi.registerCommand("poc-flag", {
		description: "Show the poc-mode flag value",
		handler: async (_args, ctx) => {
			const value = pi.getFlag("poc-mode");
			await ctx.ui.notify(`poc-mode=${value ?? "unset"}`, "info");
		},
	});

	pi.on("session_start", async () => {});
}
