/**
 * PoC-authored fixture: the factory throws, so the host must report a hard
 * factory error and exit (failure-isolation criterion).
 */

export default function () {
	throw new Error("factory exploded on purpose");
}
