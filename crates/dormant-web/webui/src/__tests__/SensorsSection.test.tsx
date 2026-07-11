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
import { render, screen, waitFor, fireEvent, cleanup } from "@testing-library/react";
import SensorsSection from "../app/config/SensorsSection";
import { createPatchStore } from "../app/config/patch";
import type { SensorConfig } from "../api/types";

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
    const byPath = new Map(patches.map((p) => [p.path.join("."), p]));

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
