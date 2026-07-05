import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";

const repoRoot = resolve(__dirname, "../../..");
const dsDir = resolve(repoRoot, "design/web-ui/_ds");

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
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
