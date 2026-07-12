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
  /** Return the pending set value for a path, or undefined if not tracked (or a remove is pending). */
  getEdit(path: string[]): unknown | undefined;
  /**
   * Track a pending `CreateEntity` for `id` in `collection` (spec §3/§7).
   * A later `trackDelete` for the same `collection`/`id` replaces it
   * (last-write-wins, mirroring the trackEdit/trackRemove pair).
   */
  trackCreate(collection: string, id: string, value: unknown): void;
  /**
   * Track a pending `DeleteEntity` for `id` in `collection`. A later
   * `trackCreate` for the same `collection`/`id` replaces it.
   */
  trackDelete(collection: string, id: string): void;
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
  /* pending creates: "collection\x1Eid" → {collection, id, value} */
  const creates = new Map<string, { collection: string; id: string; value: unknown }>();
  /* pending deletes: "collection\x1Eid" → {collection, id} */
  const deletes = new Map<string, { collection: string; id: string }>();

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

  /**
   * Return the pending set value for a path.
   *
   * Returns undefined when the path has a pending remove, has never
   * been edited, or was last touched by a remove.  Components use this
   * to compute their effective working state: `getEdit(path) ?? fetched`.
   */
  function getEdit(path: string[]): unknown | undefined {
    const key = pathKey(path);
    if (removals.has(key)) return undefined;
    return edits.get(key);
  }

  function trackCreate(collection: string, id: string, value: unknown): void {
    const key = pathKey([collection, id]);
    creates.set(key, { collection, id, value });
    // Last-write-wins: a create after a delete replaces the delete.
    deletes.delete(key);
  }

  function trackDelete(collection: string, id: string): void {
    const key = pathKey([collection, id]);
    deletes.set(key, { collection, id });
    // Last-write-wins: a delete after a create replaces the create.
    creates.delete(key);
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

    for (const { collection, id, value } of creates.values()) {
      patches.push({ op: "create_entity", collection, id, value });
    }

    for (const { collection, id } of deletes.values()) {
      patches.push({ op: "delete_entity", collection, id });
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
    creates.clear();
    deletes.clear();
  }

  return {
    trackEdit,
    trackRemove,
    getEdit,
    trackCreate,
    trackDelete,
    buildPatches,
    isLocked,
    reset,
  };
}
