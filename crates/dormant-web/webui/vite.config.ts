import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
import { getWorkspaceVersion } from "./scripts/workspace-version.ts";

const repoRoot = resolve(__dirname, "../../..");
const dsDir = resolve(repoRoot, "design/web-ui/_ds");

// Source of truth for the version shown in the UI: the workspace crate
// version from the repo-root Cargo.toml (`[workspace.package].version`).
// This throws (failing the build) if the field can't be found — there is
// no fallback to the webui package.json's placeholder `0.0.0` version.
const dormantVersion = getWorkspaceVersion(repoRoot);

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  define: {
    __DORMANT_VERSION__: JSON.stringify(dormantVersion),
  },
  resolve: {
    alias: {
      // Resolve DS token CSS imports from the repo-level design directory.
      "@ds": dsDir,
    },
  },
  server: {
    // Allow Vite to serve files from outside the project root (the DS lives in the repo's design/ dir).
    fs: {
      allow: [repoRoot, dsDir],
    },
    proxy: {
      // Proxy /api requests to the dormant daemon's HTTP server.
      "/api": {
        target: "http://127.0.0.1:9090",
        changeOrigin: true,
        // WebSocket /api/events proxying
        ws: true,
      },
    },
  },
});
