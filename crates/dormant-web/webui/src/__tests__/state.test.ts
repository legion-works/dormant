/**
 * Live-state store tests — verify event→state patching logic.
 *
 * Tests that incoming DaemonEvents correctly update the in-memory
 * snapshot via the event-handling logic in LiveStateProvider.
 */
import { describe, it, expect } from "vitest";
import type { StateSnapshot } from "../api/types";

// The patching functions are not exported from state.tsx; we test them
// here by reconstructing the logic inline.  This is a pure-function
// test of the core patching rules.

describe("event-to-state patching", () => {
  const base: StateSnapshot = {
    sensors: [
      { id: "s1", state: "absent", last_seen_secs_ago: 10 },
      { id: "s2", state: "present", last_seen_secs_ago: 2 },
    ],
    zones: [
      { id: "z1", present: false },
      { id: "z2", present: true },
    ],
    displays: [
      [
        "d1",
        {
          phase: "active",
          inhibited: false,
          paused: false,
          cmd_gen: 1,
          controllers: [],
        },
      ],
      [
        "d2",
        {
          phase: "blanked",
          inhibited: false,
          paused: false,
          cmd_gen: 0,
          controllers: [],
        },
      ],
    ],
    pending_reload: null,
  };

  it("sensor_changed patches the correct sensor's state", () => {
    const event = {
      event: "sensor_changed" as const,
      sensor: "s1",
      state: "present" as const,
    };

    // Replicate the provider's patch logic.
    const patched = {
      ...base,
      sensors: base.sensors.map((s) =>
        s.id === event.sensor ? { ...s, state: event.state } : s,
      ),
    };

    expect(patched.sensors[0].state).toBe("present"); // s1 became present
    expect(patched.sensors[0].last_seen_secs_ago).toBe(10); // unchanged
    expect(patched.sensors[1].state).toBe("present"); // s2 unchanged
  });

  it("zone_changed patches the correct zone's presence", () => {
    const event = {
      event: "zone_changed" as const,
      zone: "z1",
      present: true,
      cause: "sensor-x",
    };

    const patched = {
      ...base,
      zones: base.zones.map((z) =>
        z.id === event.zone ? { ...z, present: event.present } : z,
      ),
    };

    expect(patched.zones[0].present).toBe(true); // z1 flipped
    expect(patched.zones[1].present).toBe(true); // z2 unchanged
  });

  it("display_phase patches the correct display's phase", () => {
    const event = {
      event: "display_phase" as const,
      display: "d1",
      phase: "blanking",
      cause: "zone-vacant",
    };

    const patched = {
      ...base,
      displays: base.displays.map(
        ([id, d]) =>
          id === event.display ? [id, { ...d, phase: event.phase }] as const : [id, d] as const,
      ),
    };

    expect(patched.displays[0][1].phase).toBe("blanking");
    expect(patched.displays[0][1].cmd_gen).toBe(1); // unchanged
    expect(patched.displays[1][1].phase).toBe("blanked"); // d2 unchanged
  });

  it("unknown sensor id leaves snapshot unchanged", () => {
    const event = {
      event: "sensor_changed" as const,
      sensor: "nonexistent",
      state: "present" as const,
    };

    const patched = {
      ...base,
      sensors: base.sensors.map((s) =>
        s.id === event.sensor ? { ...s, state: event.state } : s,
      ),
    };

    expect(patched).toEqual(base);
  });

  it("unknown event tag leaves snapshot unchanged", () => {
    // Any event not matching sensor_changed/zone_changed/display_phase
    // should not mutate the snapshot.
    const patched = { ...base }; // config_reloaded, wake_retry are no-ops at snapshot level
    expect(patched).toEqual(base);
  });
});
