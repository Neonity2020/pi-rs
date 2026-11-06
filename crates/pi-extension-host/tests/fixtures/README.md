# PoC fixtures

- `hello.ts`, `notify.ts`, `session-name.ts`, `permission-gate.ts` — verbatim
  copies of upstream example extensions
  (`packages/coding-agent/examples/extensions/`, upstream SHA
  `f9daca3480732d0042b1df3a6e27221f597a620d`), used unmodified to prove the
  bridge runs real upstream code.
- `flags.ts`, `factory-throw.ts`, `handler-throw.ts` — PoC-authored minimal
  fixtures for the flag and failure-isolation criteria (modeled on
  `preset.ts` flag usage).
- `non-erasable.ts` — PoC-authored fixture using a TS `enum`, which Node's
  strip-only type loader refuses; loads only through the jiti rung of the
  spawn ladder (`tests/poc.rs`).
- `fake-jiti/` — template for a fixture-scoped fake of jiti 2.x's
  `createJiti`/`import` surface (`index.cjs`, CJS so `createRequire` resolves
  it on every Node >= 22.6). The test copies it into an isolated temp dir as
  `node_modules/jiti/` next to a copy of `non-erasable.ts`; no real jiti is
  vendored. The fake only rewrites `enum` declarations and delegates the
  remaining erasable syntax to Node's native strip-types loader.
