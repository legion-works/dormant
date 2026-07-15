/**
 * SensorsSection tests — availability config fields (spec T6).
 *
 * Covers: the three availability_* keys render as editable text fields on
 * an mqtt sensor card, and editing them produces exact
 * `sensors.<id>.availability_*` patch paths via the real PatchStore
 * (same "render + fireEvent + buildPatches" shape as ConfigForm.test.tsx's
 * DisplaysSection mode-switch tests).
 */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent, cleanup, within, act } from "@testing-library/react";
import SensorsSection from "../app/config/SensorsSection";
import { createPatchStore } from "../app/config/patch";
import type { SensorConfig, ZoneConfig } from "../api/types";

afterEach(() => cleanup());

describe("SensorsSection — availability fields", () => {
  it("renders availability_topic/online/offline fields for an mqtt sensor", async () => {
    const store = createPatchStore();
    const sensors: Record<string, SensorConfig> = {
      "living-room": {
        type: "mqtt",
        broker_url: "tcp://mqtt:1883",
        topic: "zigbee2mqtt/living-room",
        availability_topic: "tele/living-room/LWT",
        availability_payload_online: "online",
        availability_payload_offline: "offline",
      },
    };

    render(
      <SensorsSection
        sensors={sensors}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByLabelText("availability_topic")).toBeInTheDocument();
    });
    expect(screen.getByLabelText("availability_payload_online")).toBeInTheDocument();
    expect(screen.getByLabelText("availability_payload_offline")).toBeInTheDocument();
  });

  it("editing the three availability fields emits exact sensors.<id>.availability_* patch paths", async () => {
    const store = createPatchStore();
    let dirtyCalls = 0;
    const sensors: Record<string, SensorConfig> = {
      "living-room": {
        type: "mqtt",
        broker_url: "tcp://mqtt:1883",
        topic: "zigbee2mqtt/living-room",
        availability_topic: "tele/living-room/LWT",
        availability_payload_online: "online",
        availability_payload_offline: "offline",
      },
    };

    render(
      <SensorsSection
        sensors={sensors}
        store={store}
        redactedPaths={[]}
        onDirty={() => { dirtyCalls += 1; }}
        fieldErrors={{}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByLabelText("availability_topic")).toBeInTheDocument();
    });

    fireEvent.change(screen.getByLabelText("availability_topic"), {
      target: { value: "tele/other/LWT" },
    });
    fireEvent.change(screen.getByLabelText("availability_payload_online"), {
      target: { value: "up" },
    });
    fireEvent.change(screen.getByLabelText("availability_payload_offline"), {
      target: { value: "down" },
    });

    expect(dirtyCalls).toBe(3);

    const patches = store.buildPatches();
    const byPath = new Map(patches.map((p) => ["path" in p ? p.path.join(".") : "", p]));

    expect(byPath.get("sensors.living-room.availability_topic")).toMatchObject({
      op: "set",
      value: "tele/other/LWT",
    });
    expect(byPath.get("sensors.living-room.availability_payload_online")).toMatchObject({
      op: "set",
      value: "up",
    });
    expect(byPath.get("sensors.living-room.availability_payload_offline")).toMatchObject({
      op: "set",
      value: "down",
    });
  });

  it("renders help text for the availability fields", async () => {
    const store = createPatchStore();
    const sensors: Record<string, SensorConfig> = {
      "living-room": {
        type: "mqtt",
        broker_url: "tcp://mqtt:1883",
        topic: "zigbee2mqtt/living-room",
        availability_topic: "tele/living-room/LWT",
        availability_payload_online: "online",
        availability_payload_offline: "offline",
      },
    };

    render(
      <SensorsSection
        sensors={sensors}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByLabelText("availability_topic")).toBeInTheDocument();
    });

    expect(screen.getByText(/LWT/)).toBeInTheDocument();
  });
});

/**
 * CRUD affordances (spec §7, config-crud-wizard T6): Add button gated by
 * entity_crud_enabled; create form emits an exact `create_entity`
 * patch; per-card delete confirms and warns about referencing zones.
 */
describe("SensorsSection — CRUD affordances", () => {
  const sensors: Record<string, SensorConfig> = {
    "living-room": {
      type: "mqtt",
      broker_url: "tcp://mqtt:1883",
      topic: "zigbee2mqtt/living-room",
    },
  };
  const zones: Record<string, ZoneConfig> = {
    office: {
      mode: "any",
      members: ["living-room"],
      weights: {},
      unavailable_policy: "present",
    },
  };

  function renderWithCrud(entityCrudEnabled: boolean) {
    const store = createPatchStore();
    render(
      <SensorsSection
        sensors={sensors}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
        entityCrudEnabled={entityCrudEnabled}
        zones={zones}
      />,
    );
    return store;
  }

  it("shows an Add button when entity_crud_enabled", () => {
    renderWithCrud(true);
    expect(screen.getByRole("button", { name: /add sensor/i })).toBeInTheDocument();
  });

  it("hides the Add button when entity_crud_enabled is false", () => {
    renderWithCrud(false);
    expect(screen.queryByRole("button", { name: /add sensor/i })).not.toBeInTheDocument();
  });

  it("creating an mqtt sensor emits an exact create_entity patch", () => {
    const store = renderWithCrud(true);
    fireEvent.click(screen.getByRole("button", { name: /add sensor/i }));

    const form = within(screen.getByTestId("create-sensors-form"));
    fireEvent.change(form.getByLabelText("id"), { target: { value: "new-desk" } });
    fireEvent.change(form.getByLabelText("broker_url"), { target: { value: "tcp://mqtt:1883" } });
    fireEvent.change(form.getByLabelText("topic"), { target: { value: "sensors/desk" } });
    fireEvent.click(form.getByRole("button", { name: /create/i }));

    expect(store.buildPatches()).toEqual([
      {
        op: "create_entity",
        collection: "sensors",
        id: "new-desk",
        value: { type: "mqtt", broker_url: "tcp://mqtt:1883", topic: "sensors/desk" },
      },
    ]);
  });

  it("rejects a reserved id in the create form before submit", () => {
    renderWithCrud(true);
    fireEvent.click(screen.getByRole("button", { name: /add sensor/i }));
    fireEvent.change(screen.getByLabelText("id"), { target: { value: "type" } });
    expect(screen.getByText(/reserved/i)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /create/i })).toBeDisabled();
  });

  it("confirms with the referencing zone before tracking a sensor delete", async () => {
    const store = renderWithCrud(true);
    fireEvent.click(screen.getByRole("button", { name: /delete/i }));
    const dialog = screen.getByRole("alertdialog", { name: 'Delete sensor "living-room"?' });
    expect(dialog).toHaveTextContent('zone "office"');
    fireEvent.click(screen.getByRole("button", { name: "Delete sensor" }));
    await waitFor(() => expect(store.buildPatches()).toEqual([
      { op: "delete_entity", collection: "sensors", id: "living-room" },
    ]));
  });

  it("does not track a sensor delete when the dialog is cancelled", async () => {
    const store = renderWithCrud(true);
    fireEvent.click(screen.getByRole("button", { name: /delete/i }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    // `confirm()`'s promise resolves synchronously inside Cancel's onClick
    // (useConfirmDialog's `finish`), so the `.then` continuation that would
    // call trackDelete is only scheduled as a microtask — flush before
    // asserting "not tracked" so a mutant that ignores `accepted` doesn't
    // pass by accident (C6 precedent, T6/T7/T8).
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(store.buildPatches()).toEqual([]);
  });
});
