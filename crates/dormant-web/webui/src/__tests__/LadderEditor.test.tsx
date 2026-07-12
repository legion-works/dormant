/**
 * LadderEditor component tests — stage add, remove, reorder,
 * and terminal-stage rendering.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { createPatchStore } from "../app/config/patch";
import LadderEditor from "../app/config/LadderEditor";
import type { LadderStage, ConfigPatch } from "../api/types";

/** Test fixture: a simple 3-stage ladder. */
const FIXTURE_STAGES: LadderStage[] = [
  { kind: "render_black", dwell: "30s" },
  { kind: "render_screensaver", dwell: "5m" },
  { kind: "power_off" },
];

function renderEditor(stages: LadderStage[] = FIXTURE_STAGES) {
  const store = createPatchStore();
  const onDirty = () => {};
  const result = render(
    <LadderEditor
      stages={stages}
      displayId="tv"
      store={store}
      redactedPaths={[]}
      onDirty={onDirty}
      fieldErrors={{}}
    />,
  );
  return { ...result, store };
}

describe("LadderEditor", () => {
  afterEach(() => {
    cleanup();
  });

  it("adds a stage — emits whole-array set with new stage appended", async () => {
    const { store } = renderEditor();

    // Find and click the "Add Stage" button
    const addBtn = screen.getByRole("button", { name: /add stage/i });
    fireEvent.click(addBtn);

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0].op).toBe("set");

    const setPatch = patches[0] as Extract<ConfigPatch, { op: "set" }>;
    expect(setPatch.path).toEqual(["displays", "tv", "ladder"]);
    const value = setPatch.value as LadderStage[];
    expect(value).toHaveLength(4); // 3 + 1 new
    // Original stages preserved
    expect(value[0]).toEqual({ kind: "render_black", dwell: "30s" });
    expect(value[1]).toEqual({ kind: "render_screensaver", dwell: "5m" });
    expect(value[2]).toEqual({ kind: "power_off" });
    // New stage appended with default kind
    expect(value[3]).toMatchObject({ kind: "render_black" });
  });

  it("removes a stage — emits whole-array set without removed stage", async () => {
    const { store } = renderEditor();

    // Click the remove button for stage index 1 ("render_screensaver")
    const removeBtns = screen.getAllByRole("button", { name: /remove stage/i });
    expect(removeBtns).toHaveLength(3);
    fireEvent.click(removeBtns[1]);

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0].op).toBe("set");

    const setPatch = patches[0] as Extract<ConfigPatch, { op: "set" }>;
    expect(setPatch.path).toEqual(["displays", "tv", "ladder"]);
    const value = setPatch.value as LadderStage[];
    expect(value).toHaveLength(2);
    expect(value[0]).toEqual({ kind: "render_black", dwell: "30s" });
    expect(value[1]).toEqual({ kind: "power_off" });
  });

  it("reorders stage up — emits whole-array set with swapped stages", async () => {
    const { store } = renderEditor();

    // Move stage index 1 (render_screensaver) up
    const upBtns = screen.getAllByRole("button", { name: /move stage up/i });
    expect(upBtns).toHaveLength(3);
    fireEvent.click(upBtns[1]);

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0].op).toBe("set");

    const setPatch = patches[0] as Extract<ConfigPatch, { op: "set" }>;
    const value = setPatch.value as LadderStage[];
    expect(value).toHaveLength(3);
    // Index 1 (render_screensaver) swapped with index 0 (render_black)
    expect(value[0]).toEqual({ kind: "render_screensaver", dwell: "5m" });
    expect(value[1]).toEqual({ kind: "render_black", dwell: "30s" });
    expect(value[2]).toEqual({ kind: "power_off" });
  });

  it("reorders stage down — emits whole-array set with swapped stages", async () => {
    const { store } = renderEditor();

    // Move stage index 1 (render_screensaver) down
    const downBtns = screen.getAllByRole("button", { name: /move stage down/i });
    expect(downBtns).toHaveLength(3);
    fireEvent.click(downBtns[1]);

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0].op).toBe("set");

    const setPatch = patches[0] as Extract<ConfigPatch, { op: "set" }>;
    const value = setPatch.value as LadderStage[];
    expect(value).toHaveLength(3);
    // Index 1 (render_screensaver) swaps with index 2 (power_off)
    expect(value[0]).toEqual({ kind: "render_black", dwell: "30s" });
    expect(value[1]).toEqual({ kind: "power_off" });
    expect(value[2]).toEqual({ kind: "render_screensaver", dwell: "5m" });
  });

  it("renders terminal marker when last stage has no dwell", async () => {
    renderEditor();

    // The last stage (power_off, no dwell) should show a terminal marker
    expect(screen.getByText(/terminal/)).toBeInTheDocument();
    // The non-terminal stages should NOT show the marker
    // (We verify the marker appears exactly once)
  });

  it("does not render terminal marker when last stage has dwell", async () => {
    const stagesWithDwell: LadderStage[] = [
      { kind: "render_black", dwell: "10s" },
      { kind: "power_off", dwell: "0s" },
    ];
    renderEditor(stagesWithDwell);

    // All stages have dwell — no terminal marker
    expect(screen.queryByText(/terminal/)).toBeNull();
  });

  it("renders all stage kinds in kind dropdown", async () => {
    renderEditor();

    const kindSelects = screen.getAllByRole("combobox");
    expect(kindSelects.length).toBe(3);

    // The STAGE_KINDS should appear as options in the first dropdown
    const firstSelect = kindSelects[0] as HTMLSelectElement;
    const options = Array.from(firstSelect.options).map((o) => o.value);
    expect(options).toContain("render_black");
    expect(options).toContain("render_screensaver");
    expect(options).toContain("power_off");
  });

  it("omits absent dwell on terminal stage when editing another stage (null from JSON round-trip)", async () => {
    // Mimic the real server response: terminal stage has dwell: null, not absent.
    const serverStages: LadderStage[] = [
      { kind: "render_screensaver", dwell: "5m" },
      { kind: "render_black", dwell: "5m" },
      { kind: "power_off", dwell: null as unknown as undefined },
    ];
    const { store } = renderEditor(serverStages);

    // Edit the dwell of stage index 1 (render_black)
    const dwellInputs = screen.getAllByLabelText("dwell");
    expect(dwellInputs.length).toBeGreaterThanOrEqual(2);
    // The terminal stage has null dwell -> coerced to "" by DurationField,
    // but we edit index 1.
    fireEvent.change(dwellInputs[1], { target: { value: "10s" } });

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    const setPatch = patches[0] as Extract<ConfigPatch, { op: "set" }>;
    const value = setPatch.value as LadderStage[];

    // Terminal stage (index 2) must NOT have a dwell key at all.
    expect("dwell" in (value[2] as unknown as Record<string, unknown>)).toBe(false);
    // Edited stage has the new dwell
    expect(value[1]).toEqual({ kind: "render_black", dwell: "10s" });
  });

  it("serialised patch value contains no null for absent dwell", async () => {
    const serverStages: LadderStage[] = [
      { kind: "render_screensaver", dwell: "5m" },
      { kind: "render_black", dwell: "5m" },
      { kind: "power_off", dwell: null as unknown as undefined },
    ];
    const { store } = renderEditor(serverStages);

    // Edit dwell of stage index 1
    const dwellInputs = screen.getAllByLabelText("dwell");
    fireEvent.change(dwellInputs[1], { target: { value: "10s" } });

    const patches = store.buildPatches();
    const json = JSON.stringify(patches);

    // The serialised JSON must not contain null anywhere.
    // JSON.stringify produces the literal string "null" for null values;
    // we check that the word "null" does not appear as a JSON value.
    // Use a regex that matches null as a JSON value (after : or [ or ,)
    expect(json).not.toMatch(/:null\b/);
    expect(json).not.toMatch(/\[null\b/);
    expect(json).not.toMatch(/,null\b/);
  });

  it("clearing a dwell emits stage without dwell key (empty string treated as absent)", async () => {
    // Fixture: stage 0 has a dwell.  User clears the input ("").
    const stages: LadderStage[] = [
      { kind: "render_black", dwell: "5m" },
      { kind: "power_off" },
    ];
    const { store } = renderEditor(stages);

    const dwellInputs = screen.getAllByLabelText("dwell");
    expect(dwellInputs.length).toBe(1); // only stage 0 has dwell input
    fireEvent.change(dwellInputs[0], { target: { value: "" } });

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    const setPatch = patches[0] as Extract<ConfigPatch, { op: "set" }>;
    const value = setPatch.value as LadderStage[];

    // Stage 0 must NOT have a dwell key after clearing.
    expect("dwell" in (value[0] as unknown as Record<string, unknown>)).toBe(false);
    expect(value[0].kind).toBe("render_black");
  });
});
