//! pi-codemode — sandboxed script execution where the only capability is
//! calling injected tools.
//!
//! Rust port of `pi/packages/codemode`: pi's `codemode` tool lets the model
//! write a script and run it against the agent's tools inside a sandbox
//! (QuickJS in the TS original; a WASM interpreter such as `rquickjs` or a
//! Deno-style isolate is the Rust counterpart).
//!
//! Framework stage: placeholder. The script runtime, tool-call bridge, and
//! declarations generator land after the core agent loop is complete.
