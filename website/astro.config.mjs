import { defineConfig } from "astro/config";

// The landing page is a standalone static site; it is not part of the Rust
// workspace and has no build-time dependency on the crates.
export default defineConfig({});
