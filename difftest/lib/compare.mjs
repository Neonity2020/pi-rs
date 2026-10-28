// Structural comparison of canonical JSON views. Produces a list of
// differences as {path, ts, rust} leaf entries so reports stay readable.
export function diff(a, b, path = "$", out = []) {
	if (a === b) return out;
	const aType = typeOf(a);
	const bType = typeOf(b);
	if (aType !== bType) {
		out.push({ path, ts: preview(a), rust: preview(b) });
		return out;
	}
	if (aType === "array") {
		if (a.length !== b.length) {
			out.push({ path: `${path}.length`, ts: a.length, rust: b.length });
		}
		const n = Math.min(a.length, b.length);
		for (let i = 0; i < n; i++) diff(a[i], b[i], `${path}[${i}]`, out);
		return out;
	}
	if (aType === "object") {
		const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
		for (const k of keys) {
			if (k in a && k in b) diff(a[k], b[k], `${path}.${k}`, out);
			else out.push({ path: `${path}.${k}`, ts: k in a ? preview(a[k]) : "<absent>", rust: k in b ? preview(b[k]) : "<absent>" });
		}
		return out;
	}
	out.push({ path, ts: preview(a), rust: preview(b) });
	return out;
}

function typeOf(v) {
	if (v === null) return "null";
	if (Array.isArray(v)) return "array";
	return typeof v;
}

function preview(v) {
	if (v === undefined) return "<absent>";
	const s = JSON.stringify(v);
	if (s === undefined) return String(v);
	return s.length > 160 ? s.slice(0, 157) + "..." : s;
}
