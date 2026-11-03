# PoC fixtures

- `hello.ts`, `notify.ts`, `session-name.ts`, `permission-gate.ts` — verbatim
  copies of upstream example extensions
  (`packages/coding-agent/examples/extensions/`, upstream SHA
  `f9daca3480732d0042b1df3a6e27221f597a620d`), used unmodified to prove the
  bridge runs real upstream code.
- `flags.ts`, `factory-throw.ts`, `handler-throw.ts` — PoC-authored minimal
  fixtures for the flag and failure-isolation criteria (modeled on
  `preset.ts` flag usage).
