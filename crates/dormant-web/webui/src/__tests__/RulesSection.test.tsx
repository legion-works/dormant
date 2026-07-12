/**
 * RulesSection — CRUD affordances + cross-ref unlock (spec §6/§7,
 * config-crud-wizard T6).
 *
 * Covers: Add button gated by entity_crud_enabled; create form emits
 * `create_entity`; per-card delete (rules are never referenced by
 * another collection, so no references-warning is expected); the
 * previously-locked `zone`/`displays`/`inhibitors` fields (RulesSection
 * `:62-108`) unlock to dropdowns/multi-selects under entity_crud_enabled.
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, fireEvent, cleanup, within } from "@testing-library/react";
import RulesSection from "../app/config/RulesSection";
import { createPatchStore } from "../app/config/patch";
import type { RuleConfig } from "../api/types";

afterEach(() => cleanup());

const RULES: Record<string, RuleConfig> = {
  "office-rule": { zone: "office", displays: ["aoc-main"], inhibitors: ["user-activity"] },
};

function renderSection(overrides: Partial<Parameters<typeof RulesSection>[0]> = {}) {
  const store = createPatchStore();
  const props = {
    rules: RULES,
    store,
    redactedPaths: [] as string[][],
    onDirty: () => {},
    fieldErrors: {},
    entityCrudEnabled: true,
    zoneIds: ["office", "hallway"],
    displayIds: ["aoc-main", "samsung-tv"],
    ...overrides,
  };
  render(<RulesSection {...props} />);
  return { store, props };
}

describe("RulesSection — Add affordance", () => {
  it("shows an Add button when entity_crud_enabled", () => {
    renderSection();
    expect(screen.getByRole("button", { name: /add rule/i })).toBeInTheDocument();
  });

  it("hides the Add button when entity_crud_enabled is false", () => {
    renderSection({ entityCrudEnabled: false });
    expect(screen.queryByRole("button", { name: /add rule/i })).not.toBeInTheDocument();
  });

  it("creating a rule emits an exact create_entity patch", () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /add rule/i }));

    const form = within(screen.getByTestId("create-rules-form"));
    fireEvent.change(form.getByLabelText("id"), { target: { value: "new-rule" } });
    fireEvent.change(form.getByLabelText("zone"), { target: { value: "hallway" } });
    fireEvent.click(form.getByLabelText("displays: samsung-tv"));
    fireEvent.click(form.getByRole("button", { name: /create/i }));

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toMatchObject({ op: "create_entity", collection: "rules", id: "new-rule" });
    const value = (patches[0] as { value: Record<string, unknown> }).value;
    expect(value.zone).toBe("hallway");
    expect(value.displays).toEqual(["samsung-tv"]);
  });
});

describe("RulesSection — Delete affordance", () => {
  it("deletes without a references-warning (nothing references a rule)", () => {
    const { store } = renderSection();
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);

    fireEvent.click(screen.getByRole("button", { name: /delete/i }));

    expect(store.buildPatches()).toEqual([
      { op: "delete_entity", collection: "rules", id: "office-rule" },
    ]);
    confirmSpy.mockRestore();
  });
});

describe("RulesSection — cross-ref fields unlock under entity_crud_enabled (spec §6)", () => {
  it("zone renders as an editable select, not the locked span", () => {
    renderSection();
    expect(screen.getByLabelText("zone")).toBeInTheDocument();
  });

  it("displays renders as a multi-select", () => {
    renderSection();
    expect(screen.getByLabelText("displays: aoc-main")).toBeInTheDocument();
    expect(screen.getByLabelText("displays: samsung-tv")).toBeInTheDocument();
  });

  it("inhibitors renders as a multi-select from VALID_INHIBITORS", () => {
    renderSection();
    expect(screen.getByLabelText("inhibitors: user-activity")).toBeInTheDocument();
    expect(screen.getByLabelText("inhibitors: audio-playback")).toBeInTheDocument();
    expect(screen.getByLabelText("inhibitors: call")).toBeInTheDocument();
    expect(screen.getByLabelText("inhibitors: manual-pause")).toBeInTheDocument();
  });

  it("editing zone emits an exact rules.<id>.zone set patch", () => {
    const { store } = renderSection();
    fireEvent.change(screen.getByLabelText("zone"), { target: { value: "hallway" } });

    const patches = store.buildPatches();
    expect(patches).toContainEqual({ op: "set", path: ["rules", "office-rule", "zone"], value: "hallway" });
  });

  it("stays locked/read-only when entity_crud_enabled is false (regression)", () => {
    renderSection({ entityCrudEnabled: false });
    expect(screen.queryByLabelText("zone")).not.toBeInTheDocument();
    expect(screen.getByText("office")).toBeInTheDocument();
    expect(screen.getAllByLabelText("not editable in v1").length).toBeGreaterThan(0);
  });
});
