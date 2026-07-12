/**
 * Client-side mirrors of the Rust CRUD security consts (spec §4/§5/§6,
 * config-crud-wizard T6). These tests pin FIDELITY to the real Rust
 * source — a mirror that silently drifts from `config_patch.rs` would
 * give a false "accepted" client hint for something the server rejects
 * (or vice versa), which is exactly the class of bug this file guards.
 *
 * Rust sources mirrored:
 *   crates/dormant-web/src/config_patch.rs:488-545  CREATABLE_FIELDS
 *   crates/dormant-web/src/config_patch.rs:576-600  RESERVED_ENTITY_IDS
 *   crates/dormant-web/src/config_patch.rs:607-636  validate_entity_id
 *   crates/dormant-core/src/config/validate.rs:392  VALID_INHIBITORS (THIS BRANCH)
 */
import { describe, it, expect } from "vitest";
import {
  CRUD_COLLECTIONS,
  CREATABLE_FIELDS,
  RESERVED_ENTITY_IDS,
  VALID_INHIBITORS,
  SAMSUNG_TIZEN_CONTROLLER,
  validateEntityId,
  isEntityCrudEnabled,
  isPairingEnabled,
  referencingEntities,
} from "../app/config/entityCrud";

describe("CREATABLE_FIELDS — exact mirror of config_patch.rs:488-545", () => {
  it("covers exactly the 4 CRUD collections, in order", () => {
    expect(Object.keys(CREATABLE_FIELDS)).toEqual(["sensors", "zones", "displays", "rules"]);
    expect(CRUD_COLLECTIONS).toEqual(["sensors", "zones", "displays", "rules"]);
  });

  it("sensors: exactly the 13 Rust fields", () => {
    expect(CREATABLE_FIELDS.sensors).toEqual([
      "type", "kind", "hold_time", "stale_timeout",
      "broker_url", "topic", "field", "payload_on", "payload_off",
      "url", "entity",
      "port", "baud",
    ]);
  });

  it("zones: exactly the 4 Rust fields", () => {
    expect(CREATABLE_FIELDS.zones).toEqual(["mode", "members", "unavailable_policy", "weights"]);
  });

  it("displays: exactly the 10 Rust fields (blank_command/wake_command deliberately absent)", () => {
    expect(CREATABLE_FIELDS.displays).toEqual([
      "controllers", "host", "blank_mode", "output", "ddc_display", "wol_mac",
      "samsung_restore_backlight", "restore_brightness",
      "treat_unreachable_as_blanked", "command_timeout",
    ]);
    expect(CREATABLE_FIELDS.displays).not.toContain("blank_command");
    expect(CREATABLE_FIELDS.displays).not.toContain("wake_command");
  });

  it("rules: exactly the 11 Rust fields", () => {
    expect(CREATABLE_FIELDS.rules).toEqual([
      "zone", "displays", "grace_period", "inhibitors",
      "min_blank_time", "min_wake_time", "activity_idle_threshold",
      "activity_poll_interval", "wake_retries", "wake_retry_backoff",
      "wake_retry_interval",
    ]);
  });
});

describe("RESERVED_ENTITY_IDS — mirror of config_patch.rs:576-600", () => {
  it("is a superset of every documented special-case name", () => {
    for (const n of [
      "weights", "source", "ladder", "blank_data", "wake_data", "type",
      "blank_mode", "degraded_mode", "dwell", "order", "image_duration",
      "scale_mode", "transition", "transition_duration", "hold_time",
      "stale_timeout", "ddc_display", "output", "wol_mac", "host",
    ]) {
      expect(RESERVED_ENTITY_IDS).toContain(n);
    }
  });

  it("has exactly 20 entries — a change here must be a deliberate, reviewed mirror update", () => {
    expect(RESERVED_ENTITY_IDS).toHaveLength(20);
  });
});

describe("validateEntityId — mirror of config_patch.rs:607-636", () => {
  it("accepts a well-formed id", () => {
    expect(validateEntityId("desk-mmwave_2")).toEqual({ ok: true });
  });

  it("rejects empty", () => {
    expect(validateEntityId("").ok).toBe(false);
  });

  it("rejects >64 chars", () => {
    expect(validateEntityId("a".repeat(65)).ok).toBe(false);
    expect(validateEntityId("a".repeat(64)).ok).toBe(true);
  });

  it("rejects a leading digit (AOT-index ambiguity)", () => {
    expect(validateEntityId("1sensor").ok).toBe(false);
  });

  it("rejects uppercase", () => {
    expect(validateEntityId("Desk").ok).toBe(false);
  });

  it("rejects space and dot", () => {
    expect(validateEntityId("my sensor").ok).toBe(false);
    expect(validateEntityId("my.sensor").ok).toBe(false);
  });

  it("rejects unicode", () => {
    expect(validateEntityId("café").ok).toBe(false);
  });

  it("rejects every reserved name, matching RESERVED_ENTITY_IDS exactly", () => {
    for (const id of RESERVED_ENTITY_IDS) {
      const result = validateEntityId(id);
      expect(result.ok, `expected '${id}' to be rejected`).toBe(false);
      expect(result.reason).toMatch(/reserved/);
    }
  });

  it("client charset matches server charset on a known-good/known-bad matrix", () => {
    const goodIds = ["a", "desk", "office", "tv", "r", "living-room-2"];
    const badIds = ["", "1x", "Desk", "my sensor", "my.sensor", "café", "a".repeat(65)];
    for (const id of goodIds) expect(validateEntityId(id).ok, id).toBe(true);
    for (const id of badIds) expect(validateEntityId(id).ok, id).toBe(false);
  });
});

describe("VALID_INHIBITORS — THIS BRANCH mirror of validate.rs:392", () => {
  it("is exactly user-activity + manual-pause (no audio literals on this branch)", () => {
    expect(VALID_INHIBITORS).toEqual(["user-activity", "manual-pause"]);
  });
});

describe("SAMSUNG_TIZEN_CONTROLLER literal", () => {
  it("is 'samsung-tizen'", () => {
    expect(SAMSUNG_TIZEN_CONTROLLER).toBe("samsung-tizen");
  });
});

describe("daemon CRUD flags — default true when absent", () => {
  it("isEntityCrudEnabled defaults to true when the key is absent", () => {
    expect(isEntityCrudEnabled({})).toBe(true);
    expect(isEntityCrudEnabled(undefined)).toBe(true);
  });

  it("isEntityCrudEnabled reads an explicit false", () => {
    expect(isEntityCrudEnabled({ entity_crud_enabled: false })).toBe(false);
  });

  it("isPairingEnabled defaults to true when absent, reads explicit false", () => {
    expect(isPairingEnabled({})).toBe(true);
    expect(isPairingEnabled({ pairing_enabled: false })).toBe(false);
  });
});

describe("referencingEntities — client-side delete-confirm pre-check", () => {
  const inv = {
    zones: {
      office: { mode: "any", members: ["desk-mmwave"] },
      hallway: { mode: "all", members: ["room-pir"] },
    },
    rules: {
      "office-rule": { zone: "office", displays: ["aoc-main"] },
      "tv-rule": { zone: "hallway", displays: ["samsung-tv", "aoc-main"] },
    },
  };

  it("finds rules referencing a zone", () => {
    expect(referencingEntities("zones", "office", inv)).toEqual(['rule "office-rule"']);
  });

  it("finds zones referencing a sensor via members", () => {
    expect(referencingEntities("sensors", "desk-mmwave", inv)).toEqual(['zone "office"']);
  });

  it("finds rules referencing a display", () => {
    expect(referencingEntities("displays", "aoc-main", inv)).toEqual([
      'rule "office-rule"',
      'rule "tv-rule"',
    ]);
  });

  it("returns empty when nothing references the entity", () => {
    expect(referencingEntities("sensors", "nobody-home", inv)).toEqual([]);
  });
});
