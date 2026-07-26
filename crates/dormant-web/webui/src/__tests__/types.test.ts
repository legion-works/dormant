/**
 * Regression test: wire-literal enum arrays in types.ts are the single
 * source of truth, and they match the Rust serde variants exactly.
 *
 * If a future types.ts edit accidentally changes an array away from the
 * Rust strings, this test FAILS — it is the drift guard.
 */
import { describe, it, expect } from "vitest";
import {
  SENSOR_STATES,
  BLANK_MODES,
  CONTROLLER_ROLES,
  CHECK_STATUSES,
  UNAVAILABLE_POLICIES,
  DAEMON_EVENT_TAGS,
} from "../api/types";
import type { RollbackStatus, StateSnapshot } from "../api/types";

describe("enum arrays match Rust serde wire strings", () => {
  it("SensorState — serde(rename_all = 'lowercase')", () => {
    expect(SENSOR_STATES).toEqual(["present", "absent", "unavailable"]);
  });

  it("BlankMode — serde(rename_all = 'snake_case')", () => {
    expect(BLANK_MODES).toEqual(["power_off", "screen_off_audio_on", "brightness_zero"]);
  });

  it("ControllerRole — serde(rename_all = 'snake_case')", () => {
    expect(CONTROLLER_ROLES).toEqual(["primary", "fallback"]);
  });

  it("CheckStatus — serde(rename_all = 'snake_case')", () => {
    expect(CHECK_STATUSES).toEqual(["ok", "fail", "skip", "not_supported"]);
  });

  it("UnavailablePolicy — serde(rename_all = 'lowercase')", () => {
    expect(UNAVAILABLE_POLICIES).toEqual(["present", "absent"]);
  });

  it("DaemonEvent variant tags — serde(tag = 'event', rename_all = 'snake_case')", () => {
    expect(DAEMON_EVENT_TAGS).toEqual([
      "sensor_changed",
      "zone_changed",
      "display_phase",
      "config_reloaded",
      "wake_retry",
      "config_reload_rejected",
      "wear_snapshot",
      "compensation_advisory",
      "blank_failure",
      "blank_recovered",
      "wake_recovered",
      "ownership",
    ]);
  });

  it("PanelType — serde(rename_all = 'kebab-case')", async () => {
    const { PANEL_TYPES } = await import("../api/types");
    expect(PANEL_TYPES).toEqual(["woled", "qd-oled", "unknown"]);
  });
});

it("mirrors additive rollback status from rules.rs", () => {
  const rollback = {
    failed_fp: "12:00000000deadbeef",
    lkg_fp: "11:00000000cafebabe",
    detail: "rolled back to last-known-good",
  } satisfies RollbackStatus;

  const snapshot = {
    sensors: [],
    zones: [],
    displays: [],
    pending_reload: null,
    rollback,
  } satisfies StateSnapshot;

  expect(snapshot.rollback.failed_fp).toContain("deadbeef");
});

it("Ownership wire shape matches BG-1 serde(tag='event', rename_all='snake_case')", async () => {
  const { OwnershipEvent } = await import("../api/types");
  // Import the interface type for `satisfies` narrowing.
  const ev = null as unknown as OwnershipEvent;
  void ev; // reference the type

  // Verified pull — all fields present.
  const verified = {
    event: "ownership" as const,
    display: "shared_oled",
    owned: true,
    observed_input_code: 15,
    cause: "pull",
    verified: true,
    degraded: false,
  } satisfies OwnershipEvent;
  expect(verified.event).toBe("ownership");
  expect(verified.observed_input_code).toBe(15);

  // Poll-observed loss — verified: None (read-only), no degraded flag.
  const polled = {
    event: "ownership" as const,
    display: "shared_oled",
    owned: false,
    observed_input_code: 16,
    cause: "poll",
  } satisfies OwnershipEvent;
  expect(polled.cause).toBe("poll");
  expect(polled.verified).toBeUndefined();
  expect(polled.degraded).toBeUndefined();

  // Failed write — verified: false.
  const failed = {
    event: "ownership" as const,
    display: "shared_oled",
    owned: true,
    cause: "pull",
    verified: false,
    degraded: false,
  } satisfies OwnershipEvent;
  expect(failed.verified).toBe(false);
  expect(failed.observed_input_code).toBeUndefined();
});
