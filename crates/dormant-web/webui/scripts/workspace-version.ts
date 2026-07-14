import { readFileSync } from "node:fs";
import { resolve } from "node:path";

/**
 * Extracts the `[workspace.package].version` field from a Cargo.toml document
 * string. This is a narrow, dependency-free parser (no TOML library) that only
 * understands enough of the format to find the workspace package version —
 * it is not a general-purpose TOML parser.
 *
 * Throws a descriptive error if the `[workspace.package]` table, or its
 * `version` field, cannot be found, so builds fail loudly instead of quietly
 * falling back to a stale or placeholder version.
 */
export function parseWorkspaceVersion(cargoToml: string): string {
  const lines = cargoToml.split(/\r?\n/);
  const tableStart = lines.findIndex((line) => line.trim() === "[workspace.package]");
  if (tableStart === -1) {
    throw new Error(
      "Could not find a [workspace.package] table in the workspace Cargo.toml; " +
        "cannot determine the web UI version.",
    );
  }

  for (let i = tableStart + 1; i < lines.length; i += 1) {
    const trimmed = lines[i].trim();
    if (trimmed.startsWith("[")) {
      // Reached the next table without finding a version field.
      break;
    }
    const match = trimmed.match(/^version\s*=\s*"([^"]+)"/);
    if (match) {
      return match[1];
    }
  }

  throw new Error(
    "Found [workspace.package] in the workspace Cargo.toml but no version field " +
      "inside it; cannot determine the web UI version.",
  );
}

/**
 * Reads the repo-root Cargo.toml relative to `repoRoot` and returns the
 * workspace package version. This is the single source of truth for the web
 * UI's displayed version — there is no fallback to the webui package.json
 * version (which is a placeholder `0.0.0`).
 */
export function getWorkspaceVersion(repoRoot: string): string {
  const cargoTomlPath = resolve(repoRoot, "Cargo.toml");
  let contents: string;
  try {
    contents = readFileSync(cargoTomlPath, "utf-8");
  } catch (err) {
    throw new Error(
      `Could not read ${cargoTomlPath} to determine the web UI version: ${(err as Error).message}`,
    );
  }
  return parseWorkspaceVersion(contents);
}
