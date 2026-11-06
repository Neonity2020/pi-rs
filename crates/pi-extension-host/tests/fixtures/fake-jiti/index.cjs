"use strict";
// Minimal fake of jiti 2.x's public surface, scoped to the pi-extension-host
// jiti-fallback test (tests/poc.rs copies this package next to a fixture as
// node_modules/jiti). Contract it fakes, matching the upstream call sites:
//   - named export `createJiti(parentId, options)` (jiti-loader.ts re-exports
//     it; loader.ts:579 calls it with import.meta.url and options);
//   - the instance exposes `import(id, options?)`, where options.default
//     returns `mod?.default ?? mod` (loader.ts:584 passes { default: true }).
//
// Declared gap vs real jiti 2.7.0: real jiti transforms the full TypeScript
// grammar with Babel (plus alias/tsconfig/virtual-modules resolution). This
// fake rewrites only `enum` declarations — the one non-erasable construct the
// fixture uses — and delegates everything else to Node: the transformed
// source is written as a `.ts` file and native-imported, so Node's own
// strip-types loader erases the remaining erasable syntax and the host's
// registered module hooks still resolve the shim imports. Resolution of
// relative imports inside the extension, module caching, tsconfig paths and
// every other jiti feature are not implemented.

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { pathToFileURL } = require("node:url");

// `enum Name { Member, Member = value }` → a plain const object. Reverse
// numeric member mapping (Color[0]) is not emitted; the fixture only reads
// forward members.
function transformEnums(source) {
	return source.replace(/^([ \t]*)enum\s+(\w+)\s*\{([^}]*)\}[ \t]*\r?$/gm, (_match, indent, name, body) => {
		const members = [];
		let next = 0;
		for (const raw of body.split(/\r?\n/)) {
			const line = raw.trim().replace(/,$/, "");
			if (!line) continue;
			const eq = line.indexOf("=");
			if (eq === -1) {
				members.push([line, next]);
				next += 1;
			} else {
				const key = line.slice(0, eq).trim();
				const value = line.slice(eq + 1).trim();
				const numeric = Number(value);
				members.push([key, value === "" ? next : Number.isFinite(numeric) ? numeric : value]);
				next = Number.isFinite(numeric) ? numeric + 1 : next;
			}
		}
		const entries = members
			.map(([key, value]) => {
				// Numeric (incl. auto) members need serialization; string members
				// are already valid JS literal tokens and pass through as-is.
				const literal = typeof value === "number" ? JSON.stringify(value) : value;
				return `${indent}\t${key}: ${literal},`;
			})
			.join("\n");
		return `${indent}const ${name} = {\n${entries}\n${indent}};`;
	});
}

function createJiti(_parentId, _options) {
	const tmpDir = path.join(os.tmpdir(), `pi-ext-host-fake-jiti-${process.pid}`);
	fs.mkdirSync(tmpDir, { recursive: true });
	let seq = 0;
	process.on("exit", () => {
		try {
			fs.rmSync(tmpDir, { recursive: true, force: true });
		} catch {}
	});
	return {
		async import(specifier, options) {
			// The bridge always passes absolute filesystem paths.
			const sourcePath = specifier.startsWith("file://") ? specifier : path.resolve(specifier);
			const source = fs.readFileSync(sourcePath, "utf8");
			const transformed = transformEnums(source);
			const target = path.join(tmpDir, `module-${process.pid}-${seq++}.ts`);
			fs.writeFileSync(target, transformed);
			const mod = await import(pathToFileURL(target).href);
			return options && options.default ? mod?.default ?? mod : mod;
		},
	};
}

module.exports = { createJiti };
