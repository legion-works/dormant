/**
 * SettingsForm — entity_crud_enabled/pairing_enabled wiring
 * (config-crud-wizard spec §2/§10, T6).
 *
 * Covers: feature-off (`daemon.entity_crud_enabled = false`) hides every
 * section's Add button; `daemon.pairing_enabled = false` hides the
 * pairing wizard; both default to visible/enabled when the daemon
 * config omits the keys (Rust defaults, spec §10); the wizard is
 * mounted in the Displays vicinity and its post-pair hand-off reaches
 * the Displays create form pre-filled.
 */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import { SettingsForm } from "../app/config/SettingsForm";
import type { ConfigResponse } from "../api/types";

vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    getConfig: vi.fn(),
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const BASE_CONFIG: ConfigResponse = {
  path: "/home/user/.config/dormant/config.toml",
  config_version: 1,
  source: "last_applied",
  raw_toml: "[daemon]\n",
  inventory: {
    config_version: 1,
    daemon: {},
    sensors: {
      "desk-mmwave": { type: "usb-ld2410", port: "/dev/ttyUSB0" },
    },
    zones: {
      office: { mode: "any", members: ["desk-mmwave"], weights: {}, unavailable_policy: "present" },
    },
    displays: {
      "aoc-main": { controllers: ["ddcci"], blank_mode: "power_off" },
    },
    rules: {
      "office-rule": { zone: "office", displays: ["aoc-main"] },
    },
  },
  validation: { ok: true, warnings: [], errors: [] },
  display_rules: {},
  fingerprint: "abc123def4567890abc123def4567890abc123def4567890abc123def4567890",
  redacted_paths: [],
};

describe("SettingsForm — entity_crud_enabled default (absent key)", () => {
  it("shows every section's Add button and the pairing wizard when daemon config omits the flags", () => {
    render(<SettingsForm config={BASE_CONFIG} />);

    expect(screen.getByRole("button", { name: /add sensor/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /add zone/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /add display/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /add rule/i })).toBeInTheDocument();
    expect(screen.getByTestId("pairing-wizard")).toBeInTheDocument();
  });
});

describe("SettingsForm — entity_crud_enabled: false", () => {
  const config: ConfigResponse = {
    ...BASE_CONFIG,
    inventory: { ...BASE_CONFIG.inventory, daemon: { entity_crud_enabled: false } },
  };

  it("hides every section's Add button", () => {
    render(<SettingsForm config={config} />);

    expect(screen.queryByRole("button", { name: /add sensor/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /add zone/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /add display/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /add rule/i })).not.toBeInTheDocument();
  });

  it("hides every Delete button", () => {
    render(<SettingsForm config={config} />);
    expect(screen.queryByRole("button", { name: /delete/i })).not.toBeInTheDocument();
  });
});

describe("SettingsForm — pairing_enabled: false", () => {
  it("hides the pairing wizard entirely", () => {
    const config: ConfigResponse = {
      ...BASE_CONFIG,
      inventory: { ...BASE_CONFIG.inventory, daemon: { pairing_enabled: false } },
    };
    render(<SettingsForm config={config} />);
    expect(screen.queryByTestId("pairing-wizard")).not.toBeInTheDocument();
  });
});

describe("SettingsForm — pairing wizard hand-off reaches the Displays create form", () => {
  it("clicking 'create display?' after a paired status pre-fills the Displays section's create form", async () => {
    const client = await import("../api/client");
    const postSpy = vi.spyOn(client, "postPairSamsung").mockResolvedValue({ pair_id: "pid-handoff" });
    const getSpy = vi.spyOn(client, "getPairStatus").mockResolvedValue({ state: "paired" });

    render(<SettingsForm config={BASE_CONFIG} />);

    fireEvent.change(screen.getByLabelText(/host/i), { target: { value: "192.0.2.88" } });
    fireEvent.click(screen.getByRole("button", { name: /^pair$/i }));

    const createDisplayBtn = await screen.findByRole(
      "button",
      { name: /create.*display/i },
      { timeout: 3000 },
    );
    fireEvent.click(createDisplayBtn);

    await waitFor(() => {
      expect(screen.getByTestId("create-displays-form")).toBeInTheDocument();
    });
    expect(screen.getByLabelText("host")).toHaveValue("192.0.2.88");
    expect((screen.getByLabelText("controllers: samsung-tizen") as HTMLInputElement).checked).toBe(true);

    postSpy.mockRestore();
    getSpy.mockRestore();
  });
});
