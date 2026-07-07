/**
 * Dirty-state store for the config editor — pure, framework-free.
 *
 * Tests the patch-assembly and path-locking logic that must mirror
 * the server-side rules in config_patch.rs.
 */
import { describe, it, expect } from "vitest";
import { createPatchStore } from "../app/config/patch";
import type { ConfigPatch } from "../api/types";

/* ────── helpers ────── */

/** Short-hand: trackEdit on a dotted-key path. */
function set(patchStore: ReturnType<typeof createPatchStore>, dotted: string, value: unknown) {
  patchStore.trackEdit(dotted.split("."), value);
}

/** Short-hand: trackRemove on a dotted-key path. */
function del(patchStore: ReturnType<typeof createPatchStore>, dotted: string) {
  patchStore.trackRemove(dotted.split("."));
}

/** Short-hand: isLocked on a dotted-key path. */
function locked(
  patchStore: ReturnType<typeof createPatchStore>,
  dotted: string,
  redactedPaths: string[][],
): boolean {
  return patchStore.isLocked(dotted.split("."), redactedPaths);
}

/* ────── tests ────── */

describe("buildPatches", () => {
  it("emits only tracked paths", () => {
    const s = createPatchStore();
    expect(s.buildPatches()).toEqual([]);
  });

  it("re-edit same path yields one patch with last value", () => {
    const s = createPatchStore();
    set(s, "daemon.hold_time", "30s");
    set(s, "daemon.hold_time", "60s");

    const patches = s.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toEqual<ConfigPatch>({
      op: "set",
      path: ["daemon", "hold_time"],
      value: "60s",
    });
  });

  it("array edit emits whole-array set", () => {
    const s = createPatchStore();
    const arr = ["eth0", "wlan0"];
    set(s, "zones.living_room.members", arr);

    const patches = s.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toEqual<ConfigPatch>({
      op: "set",
      path: ["zones", "living_room", "members"],
      value: arr,
    });
  });

  it("trackRemove emits a remove patch", () => {
    const s = createPatchStore();
    del(s, "sensors.k1.hold_time");

    const patches = s.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toEqual<ConfigPatch>({
      op: "remove",
      path: ["sensors", "k1", "hold_time"],
    });
  });

  it("edit then remove on same path yields a single remove patch (last-write-wins)", () => {
    const s = createPatchStore();
    set(s, "sensors.k1.hold_time", "30s");
    del(s, "sensors.k1.hold_time");

    const patches = s.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toEqual<ConfigPatch>({
      op: "remove",
      path: ["sensors", "k1", "hold_time"],
    });
  });

  it("remove then edit on same path yields a single set patch (last-write-wins)", () => {
    const s = createPatchStore();
    del(s, "sensors.k1.hold_time");
    set(s, "sensors.k1.hold_time", "30s");

    const patches = s.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toEqual<ConfigPatch>({
      op: "set",
      path: ["sensors", "k1", "hold_time"],
      value: "30s",
    });
  });

  it("multiple independent paths each produce a patch", () => {
    const s = createPatchStore();
    set(s, "daemon.hold_time", "30s");
    set(s, "sensors.k1.broker_url", "mqtt://localhost");
    del(s, "rules.r1.grace_period");

    const patches = s.buildPatches();
    expect(patches).toHaveLength(3);

    const paths = patches.map((p) => p.path.join(".")).sort();
    expect(paths).toEqual([
      "daemon.hold_time",
      "rules.r1.grace_period",
      "sensors.k1.broker_url",
    ]);
  });
});

describe("isLocked", () => {
  // Server rule (check_redacted in config_patch.rs):
  // A path is locked when it equals, is a descendant of, OR is an ancestor of
  // any redacted path.  Both directions are segment-prefix matches.

  const redacted: string[][] = [
    ["credentials", "token"],
    ["sensors", "k1", "broker_url"],
  ];

  it("exact match is locked", () => {
    const s = createPatchStore();
    expect(locked(s, "sensors.k1.broker_url", redacted)).toBe(true);
    expect(locked(s, "credentials.token", redacted)).toBe(true);
  });

  it("descendant of a redacted prefix is locked", () => {
    const s = createPatchStore();
    // "sensors.k1.broker_url.x" is a descendant of "sensors.k1.broker_url"
    expect(locked(s, "sensors.k1.broker_url.x", redacted)).toBe(true);
  });

  it("ancestor of a redacted path is locked", () => {
    const s = createPatchStore();
    // "sensors.k1" is an ancestor of "sensors.k1.broker_url"
    expect(locked(s, "sensors.k1", redacted)).toBe(true);
    // "sensors" is an ancestor of "sensors.k1.broker_url"
    expect(locked(s, "sensors", redacted)).toBe(true);
  });

  it("clean sibling is NOT locked", () => {
    const s = createPatchStore();
    // "sensors.k1.hold_time" is a sibling, not a descendant/ancestor
    expect(locked(s, "sensors.k1.hold_time", redacted)).toBe(false);
  });

  it("unrelated path is NOT locked", () => {
    const s = createPatchStore();
    expect(locked(s, "daemon.host", redacted)).toBe(false);
  });

  it("returns false for empty redacted set", () => {
    const s = createPatchStore();
    expect(locked(s, "credentials.token", [])).toBe(false);
  });

  it("returns false for null/undefined redacted set", () => {
    const s = createPatchStore();
    expect(locked(s, "credentials.token", null as unknown as string[][])).toBe(false);
    expect(locked(s, "credentials.token", undefined as unknown as string[][])).toBe(false);
  });

  it("path equality is SEGMENT-wise (join-string collision guard)", () => {
    const s = createPatchStore();
    // ["a","bc"] must NOT match redacted path ["a","b"] + ["c"] implicitly
    // and ["ab","c"] must NOT be confused with ["a","bc"]
    const redactedCollision: string[][] = [
      ["a", "b", "c"],
    ];

    // "a.bc" — segments: ["a","bc"]; does NOT start with ["a","b"]
    expect(locked(s, "a.bc", redactedCollision)).toBe(false);

    // "ab.c" — segments: ["ab","c"]; does NOT start with ["a","b"]
    expect(locked(s, "ab.c", redactedCollision)).toBe(false);

    // "a.b.c" — exact match — IS locked
    expect(locked(s, "a.b.c", redactedCollision)).toBe(true);

    // "a.b.c.d" — descendant — IS locked
    expect(locked(s, "a.b.c.d", redactedCollision)).toBe(true);

    // "a.b" — ancestor of ["a","b","c"] — IS locked
    expect(locked(s, "a.b", redactedCollision)).toBe(true);
  });
});

describe("reset", () => {
  it("clears all tracked edits", () => {
    const s = createPatchStore();
    set(s, "daemon.hold_time", "30s");
    del(s, "rules.r1.grace_period");
    expect(s.buildPatches()).toHaveLength(2);

    s.reset();
    expect(s.buildPatches()).toEqual([]);
  });
});
