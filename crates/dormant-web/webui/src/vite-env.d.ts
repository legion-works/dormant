/// <reference types="vite/client" />

/**
 * Workspace crate version, injected at build time by `vite.config.ts` (and
 * mirrored in `vitest.config.ts` for tests) via the `define` option. Sourced
 * from `[workspace.package].version` in the repo-root Cargo.toml — see
 * `scripts/workspace-version.ts`.
 */
declare const __DORMANT_VERSION__: string;
