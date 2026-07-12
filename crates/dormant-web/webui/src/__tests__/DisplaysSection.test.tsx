/**
 * DisplaysSection — CRUD affordances (spec §7, config-crud-wizard T6).
 *
 * Covers: Add button gated by entity_crud_enabled; create form emits
 * `create_entity` via the store; per-card delete with a
 * references-warning confirm naming referencing rules.
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import DisplaysSection from "../app/config/DisplaysSection";
import { createPatchStore } from "../app/config/patch";
import type { DisplayConfig, RuleConfig } from "../api/types";

afterEach(() => cleanup());

const DISPLAYS: Record<string, DisplayConfig> = {
  "aoc-main": { controllers: ["ddcci"], blank_mode: "power_off" },
};

const RULES: Record<string, RuleConfig> = {
  "office-rule": { zone: "office", displays: ["aoc-main"] },
};

function renderSection(overrides: Partial<Parameters<typeof DisplaysSection>[0]> = {}) {
  const store = createPatchStore();
  const props = {
    displays: DISPLAYS,
    store,
    redactedPaths: [] as string[][],
    onDirty: () => {},
    fieldErrors: {},
    entityCrudEnabled: true,
    rules: RULES,
    ...overrides,
  };
  render(<DisplaysSection {...props} />);
  return { store, props };
}

describe("DisplaysSection — Add affordance", () => {
  it("shows an Add button when entity_crud_enabled", () => {
    renderSection();
    expect(screen.getByRole("button", { name: /add display/i })).toBeInTheDocument();
  });

  it("hides the Add button when entity_crud_enabled is false", () => {
    renderSection({ entityCrudEnabled: false });
    expect(screen.queryByRole("button", { name: /add display/i })).not.toBeInTheDocument();
  });

  it("creating a display via the form emits an exact create_entity patch, never blank_command/wake_command", () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /add display/i }));

    fireEvent.change(screen.getByLabelText("id"), { target: { value: "new-tv" } });
    fireEvent.click(screen.getByLabelText("controllers: samsung-tizen"));
    fireEvent.change(screen.getByLabelText("host"), { target: { value: "192.0.2.50" } });
    fireEvent.click(screen.getByRole("button", { name: /create/i }));

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toMatchObject({ op: "create_entity", collection: "displays", id: "new-tv" });
    const value = (patches[0] as { value: Record<string, unknown> }).value;
    expect(value.controllers).toEqual(["samsung-tizen"]);
    expect(value.host).toBe("192.0.2.50");
    expect(value).not.toHaveProperty("blank_command");
    expect(value).not.toHaveProperty("wake_command");
  });
});

describe("DisplaysSection — pairing wizard hand-off (spec §8.3)", () => {
  it("createPrefill auto-opens the create form pre-filled with host + controllers", () => {
    render(
      <DisplaysSection
        displays={{}}
        store={createPatchStore()}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
        entityCrudEnabled={true}
        rules={{}}
        createPrefill={{ host: "192.0.2.77", controllers: ["samsung-tizen"] }}
      />,
    );
    expect(screen.getByLabelText("host")).toHaveValue("192.0.2.77");
    expect((screen.getByLabelText("controllers: samsung-tizen") as HTMLInputElement).checked).toBe(true);
  });
});

describe("DisplaysSection — Delete affordance", () => {
  it("confirms and warns about referencing rules before tracking a delete", () => {
    const { store } = renderSection();
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);

    fireEvent.click(screen.getByRole("button", { name: /delete/i }));

    expect(confirmSpy).toHaveBeenCalled();
    expect(confirmSpy.mock.calls[0][0]).toMatch(/office-rule/);
    expect(store.buildPatches()).toEqual([
      { op: "delete_entity", collection: "displays", id: "aoc-main" },
    ]);

    confirmSpy.mockRestore();
  });
});
