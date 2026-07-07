/**
 * Recorded API fixtures for view component tests.
 *
 * These are canned responses matching the dormant-core serde shapes.
 * Tests mock the client module to return these values instead of
 * hitting a real daemon.
 */
import type { StateSnapshot, ConfigResponse, ControllerHealth } from "../../api/types";

export const SAMPLE_CONTROLLERS: ControllerHealth[] = [
  { name: "ddcci", role: "primary", healthy: true },
  { name: "kwin-dpms", role: "fallback", healthy: false, detail: "DBus timeout" },
];

export const SAMPLE_STATE: StateSnapshot = {
  sensors: [
    { id: "desk-mmwave", state: "present", last_seen_secs_ago: 3 },
    { id: "room-pir", state: "absent", last_seen_secs_ago: 45 },
    { id: "balcony-mqtt", state: "unavailable", last_seen_secs_ago: 120 },
  ],
  zones: [
    { id: "office", present: true },
    { id: "hallway", present: false },
  ],
  displays: [
    [
      "aoc-main",
      {
        phase: "active",
        inhibited: false,
        paused: false,
        cmd_gen: 42,
        controllers: SAMPLE_CONTROLLERS,
      },
    ],
    [
      "samsung-tv",
      {
        phase: "blanked",
        inhibited: false,
        paused: true,
        cmd_gen: 15,
        controllers: [
          { name: "samsung-tizen", role: "primary", healthy: true },
        ],
      },
    ],
  ],
  pending_reload: null,
};

export const SAMPLE_CONFIG: ConfigResponse = {
  path: "/home/user/.config/dormant/config.toml",
  config_version: 1,
  source: "last_applied",
  raw_toml: '[daemon]\nlog_level = "info"\n\n[sensors.desks-mmwave]\ntype = "usb-ld2410"\nport = "/dev/ttyUSB0"\n',
  inventory: {
    config_version: 1,
    daemon: {},
    sensors: {
      "desk-mmwave": { type: "usb-ld2410", port: "/dev/ttyUSB0" },
      "room-pir": { type: "mqtt", broker_url: "tcp://mqtt:1883", topic: "sensors/pir" },
      "balcony-mqtt": { type: "ha", url: "http://ha.local:8123", entity: "binary_sensor.balcony" },
    },
    zones: {
      office: { mode: "any", members: ["desk-mmwave", "room-pir"], quorum: undefined, threshold: undefined, weights: {}, unavailable_policy: "present" },
      hallway: { mode: "all", members: ["room-pir"], quorum: undefined, threshold: undefined, weights: {}, unavailable_policy: "absent" },
    },
    displays: {
      "aoc-main": { controllers: ["ddcci", "kwin-dpms"], blank_mode: "power_off" },
      "samsung-tv": { controllers: ["samsung-tizen"], blank_mode: "screen_off_audio_on" },
    },
    rules: {
      "office-rule": {
        zone: "office",
        displays: ["aoc-main"],
        wake_retries: 3,
      },
      "tv-rule": {
        zone: "hallway",
        displays: ["samsung-tv"],
        wake_retries: 5,
      },
    },
  },
  validation: { ok: true, warnings: [], errors: [] },
  display_rules: {
    "aoc-main": { rule: "office-rule", zone: "office" },
    "samsung-tv": { rule: "tv-rule", zone: "hallway" },
  },
  fingerprint: "abc123def4567890abc123def4567890abc123def4567890abc123def4567890",
  redacted_paths: [],
};
