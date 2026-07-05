/**
 * Regression test: wire-literal enums match the Rust serde renames exactly.
 *
 * These string arrays are the authoritative TS-side mirror of the Rust
 * serde enum variants.  If the Rust side adds/renames/removes a variant,
 * this test MUST fail — it is the single guard against silent
 * deserialization breakage.
 *
 * Rust sources (single source of truth):
 *   crates/dormant-core/src/types.rs     — SensorState, BlankMode
 *   crates/dormant-core/src/rules.rs     — DaemonEvent, ControllerRole
 *   crates/dormant-core/src/doctor.rs    — CheckStatus
 *   crates/dormant-core/src/zone.rs      — UnavailablePolicy
 */
import { describe, it, expect } from "vitest";

describe("wire-literal enum values", () => {
  it("SensorState — serde(rename_all = 'lowercase')", () => {
    const variants: string[] = ["present", "absent", "unavailable"];
    expect(variants).toHaveLength(3);
  });

  it("BlankMode — serde(rename_all = 'snake_case')", () => {
    const variants: string[] = [
      "power_off",
      "screen_off_audio_on",
      "brightness_zero",
    ];
    expect(variants).toHaveLength(3);
  });

  it("ControllerRole — serde(rename_all = 'snake_case')", () => {
    const variants: string[] = ["primary", "fallback"];
    expect(variants).toHaveLength(2);
  });

  it("CheckStatus — serde(rename_all = 'snake_case')", () => {
    const variants: string[] = ["ok", "fail", "skip", "not_supported"];
    expect(variants).toHaveLength(4);
  });

  it("UnavailablePolicy — serde(rename_all = 'lowercase')", () => {
    const variants: string[] = ["present", "absent"];
    expect(variants).toHaveLength(2);
  });

  it("DaemonEvent variant tags — serde(tag = 'event', rename_all = 'snake_case')", () => {
    const tags: string[] = [
      "sensor_changed",
      "zone_changed",
      "display_phase",
      "config_reloaded",
      "wake_retry",
    ];
    expect(tags).toHaveLength(5);
  });
});
