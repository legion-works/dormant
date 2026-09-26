/**
 * FAIL_UNSAFE_SETTINGS detector — a client-side UX gate that intercepts
 * pending patches whose values flip a deliberate fail-safe default to a
 * fail-unsafe one.
 *
 * The Rust server is not involved here: the listed values are normal
 * accepted server values. The gate exists to make the operator
 * consciously consent to an off-default in a place where one misplaced
 * tick can blank a screen while someone is sitting at it.
 */
import { describe, it, expect } from "vitest";
import { detectUnsafePatches } from "../app/config/entityCrud";
import type { ConfigPatch } from "../api/types";

describe("detectUnsafePatches — zones.<id>.unavailable_policy = \"absent\"", () => {
  it("flags a Set patch on zones.<id>.unavailable_policy = \"absent\"", () => {
    const patches: ConfigPatch[] = [
      { op: "set", path: ["zones", "office", "unavailable_policy"], value: "absent" },
    ];
    const hits = detectUnsafePatches(patches);
    expect(hits).toHaveLength(1);
    expect(hits[0].consequence).toMatch(/offline|empty|screen/);
    expect(hits[0].patch).toEqual(patches[0]);
  });

  it("does NOT flag a Set patch on zones.<id>.unavailable_policy = \"present\"", () => {
    const patches: ConfigPatch[] = [
      { op: "set", path: ["zones", "office", "unavailable_policy"], value: "present" },
    ];
    expect(detectUnsafePatches(patches)).toEqual([]);
  });

  it("does NOT flag a Set patch on a different field (e.g. mode)", () => {
    const patches: ConfigPatch[] = [
      { op: "set", path: ["zones", "office", "mode"], value: "all" },
    ];
    expect(detectUnsafePatches(patches)).toEqual([]);
  });

  it("does NOT flag a Set on a path that is not under zones", () => {
    const patches: ConfigPatch[] = [
      { op: "set", path: ["sensors", "radar", "unavailable_policy"], value: "absent" },
    ];
    expect(detectUnsafePatches(patches)).toEqual([]);
  });

  it("does NOT flag a deeper nested path (e.g. members.0)", () => {
    const patches: ConfigPatch[] = [
      { op: "set", path: ["zones", "office", "members", "0"], value: "absent" },
    ];
    expect(detectUnsafePatches(patches)).toEqual([]);
  });

  it("flags a CreateEntity zone whose value carries unavailable_policy = \"absent\"", () => {
    const patches: ConfigPatch[] = [
      {
        op: "create_entity",
        collection: "zones",
        id: "lounge",
        value: { mode: "any", members: ["radar"], unavailable_policy: "absent" },
      },
    ];
    const hits = detectUnsafePatches(patches);
    expect(hits).toHaveLength(1);
    expect(hits[0].consequence).toMatch(/offline|empty|screen/);
    expect(hits[0].patch).toEqual(patches[0]);
  });

  it("does NOT flag a CreateEntity zone whose value omits unavailable_policy (default is \"present\")", () => {
    const patches: ConfigPatch[] = [
      {
        op: "create_entity",
        collection: "zones",
        id: "lounge",
        value: { mode: "any", members: ["radar"] },
      },
    ];
    expect(detectUnsafePatches(patches)).toEqual([]);
  });

  it("does NOT flag a CreateEntity for a non-zone collection, even with unavailable_policy in value", () => {
    const patches: ConfigPatch[] = [
      {
        op: "create_entity",
        collection: "rules",
        id: "r-lounge",
        value: { zone: "lounge", displays: ["tv"], unavailable_policy: "absent" },
      },
    ];
    expect(detectUnsafePatches(patches)).toEqual([]);
  });

  it("Remove on zones.<id>.unavailable_policy is never unsafe (it falls back to \"present\")", () => {
    const patches: ConfigPatch[] = [
      { op: "remove", path: ["zones", "office", "unavailable_policy"] },
    ];
    expect(detectUnsafePatches(patches)).toEqual([]);
  });

  it("DeleteEntity on a zone is never unsafe", () => {
    const patches: ConfigPatch[] = [
      { op: "delete_entity", collection: "zones", id: "old-zone" },
    ];
    expect(detectUnsafePatches(patches)).toEqual([]);
  });

  it("returns one hit per unsafe patch; multiple zones with absent = multiple entries", () => {
    const patches: ConfigPatch[] = [
      { op: "set", path: ["zones", "office", "unavailable_policy"], value: "absent" },
      { op: "set", path: ["zones", "lounge", "unavailable_policy"], value: "absent" },
      { op: "set", path: ["zones", "kitchen", "unavailable_policy"], value: "present" },
    ];
    const hits = detectUnsafePatches(patches);
    expect(hits).toHaveLength(2);
    expect(hits[0].consequence).toMatch(/offline|empty|screen/);
    expect(hits[1].consequence).toMatch(/offline|empty|screen/);
  });

  it("empty patch list returns no hits", () => {
    expect(detectUnsafePatches([])).toEqual([]);
  });
});
