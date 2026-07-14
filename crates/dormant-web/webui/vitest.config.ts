import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
import { getWorkspaceVersion } from "./scripts/workspace-version";

const repoRoot = resolve(__dirname, "../../..");
const dormantVersion = getWorkspaceVersion(repoRoot);

export default defineConfig({
  plugins: [react()],
  define: {
    __DORMANT_VERSION__: JSON.stringify(dormantVersion),
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/__tests__/setup.ts"],
    include: ["src/**/*.{test,spec}.{ts,tsx}"],
  },
});
