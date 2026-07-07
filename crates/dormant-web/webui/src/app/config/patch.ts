/**
 * Dirty-state store for the config editor — pure, framework-free.
 *
 * Tracks pending edits and removals as ConfigPatch objects.
 * Semantics mirror the server-side Patch behaviour in config_patch.rs:
 * arrays transit whole, last-write-wins per path, a trackRemove after
 * a trackEdit on the same path collapses to a single remove patch,
 * and isLocked implements the same prefix-aware redacted-path rejection
 * as check_redacted (exact, descendant, and ancestor directions).
 */
import type { ConfigPatch } from "../../api/types";

/** Segment-safe path key: segments joined with U+001E (Record Separator). */
function pathKey(path: string[]): string {
  return path.join("\x1E");
}

export interface PatchStore {
  trackEdit(path: string[], value: unknown): void;
  trackRemove(path: string[]): void;
  buildPatches(): ConfigPatch[];
  isLocked(path: string[], redactedPaths: string[][]): boolean;
  reset(): void;
}

/**
 * Create a new dirty-state patch store.
 *
 * The store tracks edits and removals independently — `buildPatches()`
 * returns only dirty paths, with last-write-wins semantics per path.
 */
export function createPatchStore(): PatchStore {
  /* pending edits: key → value */
  const edits = new Map<string, unknown>();
  /* pending removals: key → true */
  const removals = new Set<string>();

  function trackEdit(path: string[], value: unknown): void {
    const key = pathKey(path);
    edits.set(key, value);
    // Last-write-wins: an edit after a remove replaces the remove.
    removals.delete(key);
  }

  function trackRemove(path: string[]): void {
    const key = pathKey(path);
    removals.add(key);
    // Last-write-wins: a remove after an edit replaces the edit.
    edits.delete(key);
  }

  function buildPatches(): ConfigPatch[] {
    const patches: ConfigPatch[] = [];

    for (const [key, value] of edits) {
      const path = key.split("\x1E");
      patches.push({ op: "set", path, value });
    }

    for (const key of removals) {
      const path = key.split("\x1E");
      patches.push({ op: "remove", path });
    }

    return patches;
  }

  /**
   * Check whether a path intersects any redacted path.
   *
   * Matches the server-side `check_redacted` rule in config_patch.rs
   * (prefix-aware in both directions):
   *
   * - **Exact match** — the path equals a redacted path.
   * - **Descendant** — the path starts with a redacted prefix
   *   (editing a sub-key of a redacted value).
   * - **Ancestor** — a redacted path starts with the given path
   *   (editing a parent would replace/remove a redacted descendant).
   *
   * Segment-wise comparison guards against join-string collisions:
   * ["a","bc"] correctly ≠ ["ab","c"].
   */
  function isLocked(path: string[], redactedPaths: string[][]): boolean {
    if (!redactedPaths || redactedPaths.length === 0) return false;

    for (const r of redactedPaths) {
      if (!r || r.length === 0) continue;

      // Exact or descendant: patch-path starts with redacted path.
      if (
        path.length >= r.length &&
        r.every((seg, i) => path[i] === seg)
      ) {
        return true;
      }

      // Ancestor: redacted path starts with patch-path.
      if (
        r.length >= path.length &&
        path.every((seg, i) => r[i] === seg)
      ) {
        return true;
      }
    }

    return false;
  }

  function reset(): void {
    edits.clear();
    removals.clear();
  }

  return { trackEdit, trackRemove, buildPatches, isLocked, reset };
}
