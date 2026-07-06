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
    ]);
  });
});
