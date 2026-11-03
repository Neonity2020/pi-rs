/**
 * PoC-authored fixture: one command handler throws while another keeps
 * working — handler-scoped isolation, not process death.
 */

export default function (pi) {
	pi.registerCommand("boom", {
		description: "Throws on purpose",
		handler: async () => {
			throw new Error("boom");
		},
	});

	pi.registerCommand("alive", {
		description: "Still works after the failure",
		handler: async (_args, ctx) => {
			await ctx.ui.notify("alive", "info");
		},
	});
}
