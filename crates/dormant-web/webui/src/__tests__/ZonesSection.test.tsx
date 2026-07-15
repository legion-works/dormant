/**
 * ZonesSection — CRUD affordances (spec §6/§7, config-crud-wizard T6).
 *
 * Covers: Add button gated by entity_crud_enabled; create form emits
 * `create_entity` via the store; per-card delete with a
 * references-warning confirm; `members` unlocks to a multi-select
 * under entity_crud_enabled (was read-only/locked before this feature).
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent, cleanup, within, act } from "@testing-library/react";
import ZonesSection from "../app/config/ZonesSection";
import { createPatchStore } from "../app/config/patch";
import type { ZoneConfig, RuleConfig } from "../api/types";

afterEach(() => cleanup());

const ZONES: Record<string, ZoneConfig> = {
  office: {
    mode: "any",
    members: ["desk-mmwave"],
    weights: {},
    unavailable_policy: "present",
  },
};

const RULES: Record<string, RuleConfig> = {
  "office-rule": { zone: "office", displays: ["aoc-main"] },
};

function renderSection(overrides: Partial<Parameters<typeof ZonesSection>[0]> = {}) {
  const store = createPatchStore();
  const props = {
    zones: ZONES,
    store,
    redactedPaths: [] as string[][],
    onDirty: () => {},
    fieldErrors: {},
    entityCrudEnabled: true,
    sensorIds: ["desk-mmwave", "room-pir"],
    rules: RULES,
    ...overrides,
  };
  render(<ZonesSection {...props} />);
  return { store, props };
}

describe("ZonesSection — Add affordance", () => {
  it("shows an Add button when entity_crud_enabled", () => {
    renderSection();
    expect(screen.getByRole("button", { name: /add zone/i })).toBeInTheDocument();
  });

  it("hides the Add button when entity_crud_enabled is false", () => {
    renderSection({ entityCrudEnabled: false });
    expect(screen.queryByRole("button", { name: /add zone/i })).not.toBeInTheDocument();
  });

  it("clicking Add opens a create form; creating a zone emits create_entity via the store", () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /add zone/i }));

    const form = within(screen.getByTestId("create-zones-form"));
    fireEvent.change(form.getByLabelText("id"), { target: { value: "new-zone" } });
    fireEvent.click(form.getByLabelText("members: room-pir"));
    fireEvent.click(form.getByRole("button", { name: /create/i }));

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toMatchObject({
      op: "create_entity",
      collection: "zones",
      id: "new-zone",
    });
    expect((patches[0] as { value: { members: string[] } }).value.members).toEqual(["room-pir"]);
  });

  it("renders section even with zero zones, when entity_crud_enabled", () => {
    renderSection({ zones: {} });
    expect(screen.getByRole("button", { name: /add zone/i })).toBeInTheDocument();
  });

  it("renders nothing when there are zero zones and entity_crud_enabled is false (regression)", () => {
    const { container } = render(
      <ZonesSection
        zones={{}}
        store={createPatchStore()}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
        entityCrudEnabled={false}
        sensorIds={[]}
        rules={{}}
      />,
    );
    expect(container.firstChild).toBeNull();
  });
});

describe("ZonesSection — Delete affordance", () => {
  it("confirms with the referencing rule before tracking a zone delete", async () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /delete/i }));
    const dialog = screen.getByRole("alertdialog", { name: 'Delete zone "office"?' });
    expect(dialog).toHaveTextContent('rule "office-rule"');
    fireEvent.click(screen.getByRole("button", { name: "Delete zone" }));
    await waitFor(() => expect(store.buildPatches()).toEqual([
      { op: "delete_entity", collection: "zones", id: "office" },
    ]));
  });

  it("does not track a zone delete when the dialog is cancelled", async () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByRole("button", { name: /delete/i }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    // Flush the microtask the async confirm() continuation runs on — see
    // SensorsSection.test.tsx's cancel test for why this is required to
    // actually bite a mutant that ignores `accepted` (C6 precedent).
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(store.buildPatches()).toEqual([]);
  });
});

describe("ZonesSection — members cross-ref unlock (spec §6)", () => {
  it("renders members as an editable multi-select under entity_crud_enabled", () => {
    renderSection();
    const checkbox = screen.getByLabelText("members: desk-mmwave") as HTMLInputElement;
    expect(checkbox).toBeInTheDocument();
    expect(checkbox.checked).toBe(true);
    expect(checkbox.disabled).toBe(false);
  });

  it("stays locked/read-only when entity_crud_enabled is false (regression)", () => {
    renderSection({ entityCrudEnabled: false });
    expect(screen.queryByLabelText("members: desk-mmwave")).not.toBeInTheDocument();
    expect(screen.getByText("desk-mmwave")).toBeInTheDocument();
  });

  it("toggling a member emits an exact zones.<id>.members set patch", () => {
    const { store } = renderSection();
    fireEvent.click(screen.getByLabelText("members: room-pir"));

    const patches = store.buildPatches();
    expect(patches).toHaveLength(1);
    expect(patches[0]).toMatchObject({ op: "set", path: ["zones", "office", "members"] });
    expect((patches[0] as { value: string[] }).value).toEqual(["desk-mmwave", "room-pir"]);
  });
});
