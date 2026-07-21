/**
 * DisplaysSection — CRUD affordances (spec §7, config-crud-wizard T6).
 *
 * Covers: Add button gated by entity_crud_enabled; create form emits
 * `create_entity` via the store; per-card delete with a
 * references-warning confirm naming referencing rules.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent, cleanup, act } from "@testing-library/react";
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
  it("display_editor_round_trips_shared_scope_and_input_code", () => {
    const { store } = renderSection({
      displays: { "aoc-main": { controllers: ["ddcci"], blank_mode: "power_off", scope: "shared", shared_input_code: 15 } },
    });

    expect(screen.getByLabelText("scope")).toHaveValue("shared");
    expect(screen.getByLabelText("shared_input_code")).toHaveValue(15);
    fireEvent.change(screen.getByLabelText("shared_input_code"), { target: { value: "16" } });

    expect(store.buildPatches()).toContainEqual({
      op: "set",
      path: ["displays", "aoc-main", "shared_input_code"],
      value: 16,
    });
  });

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
  it("confirms with the referencing rule before tracking a display delete", async () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /delete/i }));
    const dialog = screen.getByRole("alertdialog", { name: 'Delete display "aoc-main"?' });
    expect(dialog).toHaveTextContent('rule "office-rule"');
    fireEvent.click(screen.getByRole("button", { name: "Delete display" }));
    await waitFor(() => expect(store.buildPatches()).toEqual([
      { op: "delete_entity", collection: "displays", id: "aoc-main" },
    ]));
  });

  it("does not track a display delete when the dialog is cancelled", async () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /delete/i }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    // Flush the microtask the async confirm() continuation runs on — see
    // SensorsSection.test.tsx's cancel test for why this is required to
    // actually bite a mutant that ignores `accepted` (C6 precedent). This
    // cancel test did not exist before this task — added to mirror the
    // coverage the other three entity sections have.
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(store.buildPatches()).toEqual([]);
  });
});
