// Resolve hooks for the extension-host PoC bridge: bare-specifier imports of
// upstream workspace packages are mapped to local shim modules so example
// extensions load without a node_modules tree. See
// docs/operations/extensions-host-experiment.md (§4, PoC design).
import { pathToFileURL } from "node:url";

export async function resolve(specifier, context, next) {
	const map = JSON.parse(process.env.PI_EXT_SHIM_MAP ?? "{}");
	if (map[specifier]) {
		return {
			url: pathToFileURL(`${process.env.PI_EXT_SHIM_DIR}/${map[specifier]}`).href,
			shortCircuit: true,
		};
	}
	return next(specifier, context);
}
