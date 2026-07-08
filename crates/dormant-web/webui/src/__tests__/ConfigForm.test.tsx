/**
 * Settings form tests — editable config sections, apply flow,
 * conflict handling, inline validation errors, and navigation guard.
 */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import { SettingsForm } from "../app/config/SettingsForm";
import Config from "../app/views/Config";
import type { ConfigResponse, ApplyResponse, StateSnapshot } from "../api/types";
import { ApiError } from "../api/client";


const { mocks, SAMPLE_CONFIG, UPDATED_CONFIG, SAMPLE_STATE, DISPLAY_WITH_LADDER, DISPLAY_WITH_REDACTED_SOURCE, DISPLAY_NO_MODE, DISPLAY_BLANK_MODE } = vi.hoisted(() => {
  const postConfigApply = vi.fn();
  const getConfig = vi.fn();
  const getState = vi.fn();
  const postReload = vi.fn().mockResolvedValue(undefined);

  const F1 = "abc123def4567890abc123def4567890abc123def4567890abc123def4567890";
  const F2 = "def456abc7890123def456abc7890123def456abc7890123def456abc78901";

  const REDACTED_BROKER: string[][] = [["sensors", "room-pir", "broker_url"]];

  const SAMPLE_CONFIG: ConfigResponse = {
    path: "/home/user/.config/dormant/config.toml",
    config_version: 2,
    source: "last_applied",
    raw_toml: '[daemon]\nlog_level = "info"\nweb_port = 9777\n',
    inventory: {
      config_version: 2,
      daemon: {
        log_level: "info",
        web_port: 9777,
        web_bind: "127.0.0.1",
      },
      sensors: {
        "desk-mmwave": {
          type: "usb-ld2410",
          port: "/dev/ttyUSB0",
          baud: 256000,
          hold_time: "5s",
          stale_timeout: "30s",
        },
        "room-pir": {
          type: "mqtt",
          broker_url: "tcp://mqtt:1883",
          topic: "sensors/pir",
          hold_time: "2s",
          stale_timeout: "60s",
        },
      },
      zones: {
        office: {
          mode: "any",
          members: ["desk-mmwave", "room-pir"],
          quorum: undefined,
          threshold: undefined,
          weights: {},
          unavailable_policy: "present",
        },
      },
      displays: {
        "lg-oled": { controllers: ["lg-webos"], blank_mode: "power_off" },
      },
      rules: {
        "office-rule": {
          zone: "office",
          displays: ["lg-oled"],
          grace_period: "30s",
          wake_retries: 3,
          wake_retry_interval: "5s",
          inhibitors: ["manual"],
        },
      },
    },
    validation: { ok: true, warnings: [], errors: [] },
    display_rules: {
      "lg-oled": { rule: "office-rule", zone: "office" },
    },
    fingerprint: F1,
    redacted_paths: REDACTED_BROKER,
  };

  const UPDATED_CONFIG: ConfigResponse = {
    ...SAMPLE_CONFIG,
    fingerprint: F2,
    config_version: 3,
  };

  const SAMPLE_STATE: StateSnapshot = {
    sensors: [],
    zones: [],
    displays: [],
    pending_reload: null,
  };

  // ── Rich display fixtures ──

  /** Display with escalation ladder + screensaver. */
  const DISPLAY_WITH_LADDER: ConfigResponse = {
    ...SAMPLE_CONFIG,
    fingerprint: "aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44ee55ff66aa11bb22cc33",
    inventory: {
      ...SAMPLE_CONFIG.inventory,
      displays: {
        "lg-oled": {
          controllers: ["lg-webos"],
          ladder: [
            { kind: "render_black", dwell: "30s" },
            { kind: "power_off" },
          ],
          screensaver: {
            trigger: "escalation",
            audio: false,
            scale_mode: "fill",
            transition: "crossfade",
            source: [{ path: "/wallpapers", recurse: true }],
          },
        },
      },
    },
    redacted_paths: [],
  };

  /** Display with redacted screensaver source URL (ancestor-lock trigger). */
  const DISPLAY_WITH_REDACTED_SOURCE: ConfigResponse = {
    ...SAMPLE_CONFIG,
    fingerprint: "bb22cc33dd44ee55ff66aa11bb22cc33dd44ee55ff66aa11bb22cc33dd",
    inventory: {
      ...SAMPLE_CONFIG.inventory,
      displays: {
        "tv": {
          controllers: ["samsung-tizen"],
          ladder: [
            { kind: "render_black", dwell: "30s" },
            { kind: "power_off" },
          ],
          screensaver: {
            trigger: "escalation",
            audio: true,
            source: [
              { urls: ["https://secret.example/wallpaper.jpg"], shuffle: true },
            ],
          },
        },
      },
    },
    redacted_paths: [
      ["displays", "tv", "screensaver", "source", "0", "urls", "0"],
    ],
  };

  /** Display with neither blank_mode nor ladder configured. */
  const DISPLAY_NO_MODE: ConfigResponse = {
    ...SAMPLE_CONFIG,
    fingerprint: "cc33dd44ee55ff66aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44ee",
    inventory: {
      ...SAMPLE_CONFIG.inventory,
      displays: {
        "monitor": {
          controllers: ["ddc-ci"],
          output: "DP-1",
        },
      },
    },
    redacted_paths: [],
  };

  /** Display with blank_mode only (simple mode). */
  const DISPLAY_BLANK_MODE: ConfigResponse = {
    ...SAMPLE_CONFIG,
    fingerprint: "dd44ee55ff66aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44ee55ff",
    inventory: {
      ...SAMPLE_CONFIG.inventory,
      displays: {
        "lg-oled": {
          controllers: ["lg-webos"],
          blank_mode: "power_off",
          degraded_mode: "screen_off_audio_on",
        },
      },
    },
    redacted_paths: [],
  };

  return {
    mocks: { postConfigApply, getConfig, getState, postReload },
    SAMPLE_CONFIG,
    UPDATED_CONFIG,
    SAMPLE_STATE,
    DISPLAY_WITH_LADDER,
    DISPLAY_WITH_REDACTED_SOURCE,
    DISPLAY_NO_MODE,
    DISPLAY_BLANK_MODE,
  };
});

vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    getConfig: mocks.getConfig,
    getState: mocks.getState,
    postConfigApply: mocks.postConfigApply,
    postReload: mocks.postReload,
  };
});


// ── SettingsForm tests ──

describe("SettingsForm", () => {
  beforeEach(() => {
    mocks.getConfig.mockResolvedValue(SAMPLE_CONFIG);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("renders sections from fixture", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });
    expect(screen.getByText("Sensors")).toBeInTheDocument();
    expect(screen.getByText("Zones")).toBeInTheDocument();
    expect(screen.getByText("Rules")).toBeInTheDocument();
    expect(screen.getByText("Displays")).toBeInTheDocument();

    expect(screen.getByText("log_level")).toBeInTheDocument();
    expect(screen.getByText("web_port")).toBeInTheDocument();

    const mmwaveEls = screen.getAllByText("desk-mmwave");
    expect(mmwaveEls.length).toBeGreaterThanOrEqual(1);
    const pirEls = screen.getAllByText("room-pir");
    expect(pirEls.length).toBeGreaterThanOrEqual(1);
    const officeEls = screen.getAllByText("office");
    expect(officeEls.length).toBeGreaterThanOrEqual(1);

    expect(screen.getByText("office-rule")).toBeInTheDocument();
  });

  it("edit marks dirty and ApplyBar count increments", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    expect(screen.getByText(/0 unsaved/)).toBeInTheDocument();

    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });
  });

  it("locked field disabled with tooltip (redacted broker_url)", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      const pirEls = screen.getAllByText("room-pir");
      expect(pirEls.length).toBeGreaterThanOrEqual(1);
    });

    const brokerFields = screen.getAllByLabelText("broker_url");
    expect(brokerFields.length).toBeGreaterThanOrEqual(1);

    const lockedBroker = brokerFields[0] as HTMLInputElement;
    expect(lockedBroker.disabled).toBe(true);

    const lockIndicator = screen.getAllByTitle(/contains credentials/);
    expect(lockIndicator.length).toBeGreaterThanOrEqual(1);
  });

  it("apply success — green banner and posted body has fingerprint + only dirty paths", async () => {
    const applyRes: ApplyResponse = { applied: true, reload: "reloaded" };
    mocks.postConfigApply.mockResolvedValueOnce(applyRes);

    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    const applyBtn = screen.getByRole("button", { name: /apply/i });
    fireEvent.click(applyBtn);

    await waitFor(() => {
      expect(mocks.postConfigApply).toHaveBeenCalledTimes(1);
    });

    const callArgs = mocks.postConfigApply.mock.calls[0][0];
    expect(callArgs.fingerprint).toBe(SAMPLE_CONFIG.fingerprint);

    expect(callArgs.patches).toHaveLength(1);
    expect(callArgs.patches[0]).toMatchObject({
      op: "set",
      path: ["daemon", "log_level"],
      value: "debug",
    });

    await waitFor(() => {
      expect(screen.getByText(/reloaded/)).toBeInTheDocument();
    });
  });

  it("409 — conflict dialog rendered", async () => {
    mocks.postConfigApply.mockRejectedValueOnce(
      new ApiError(409, { error: "Config changed on disk — fingerprint mismatch" }),
    );

    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    const applyBtn = screen.getByRole("button", { name: /apply/i });
    fireEvent.click(applyBtn);

    await waitFor(() => {
      expect(screen.getByText(/Config changed on disk/)).toBeInTheDocument();
    });
    expect(screen.getByRole("button", { name: /reload/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /keep editing/i })).toBeInTheDocument();
  });

  it("422 — inline error lands on the matching field", async () => {
    mocks.postConfigApply.mockRejectedValueOnce(
      new ApiError(422, {
        errors: [
          { what: "bad_value", detail: "sensors.desk-mmwave.port: must be a valid device path" },
        ],
      }),
    );

    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      const mmwaveEls = screen.getAllByText("desk-mmwave");
      expect(mmwaveEls.length).toBeGreaterThanOrEqual(1);
    });

    const portField = screen.getByLabelText("port") as HTMLInputElement;
    fireEvent.change(portField, { target: { value: "/dev/invalid" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    const applyBtn = screen.getByRole("button", { name: /apply/i });
    fireEvent.click(applyBtn);

    await waitFor(() => {
      expect(screen.getByText(/must be a valid device path/)).toBeInTheDocument();
    });
  });

  it("pending — immediate refetch, neutral banner", async () => {
    const applyRes: ApplyResponse = { applied: true, reload: "pending", detail: "queued for reload" };
    mocks.postConfigApply.mockResolvedValueOnce(applyRes);
    mocks.getConfig.mockResolvedValueOnce(UPDATED_CONFIG);

    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    const applyBtn = screen.getByRole("button", { name: /apply/i });
    fireEvent.click(applyBtn);

    // Neutral banner appears
    await waitFor(() => {
      expect(screen.getByText(/pending/)).toBeInTheDocument();
    });

    // getConfig should have been called immediately (no setTimeout delay)
    await waitFor(() => {
      expect(mocks.getConfig).toHaveBeenCalled();
    });
  });

  it("pending→refetch cycle — next apply uses new fingerprint", async () => {
    // Step 1: Apply with F1 → pending
    const applyRes1: ApplyResponse = { applied: true, reload: "pending" };
    mocks.postConfigApply.mockResolvedValueOnce(applyRes1);
    // Refetch returns config with fingerprint F2
    mocks.getConfig.mockResolvedValueOnce(UPDATED_CONFIG);

    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // First edit + apply
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    fireEvent.click(screen.getByRole("button", { name: /apply/i }));

    // First apply used F1
    await waitFor(() => {
      expect(mocks.postConfigApply).toHaveBeenCalledTimes(1);
    });
    expect(mocks.postConfigApply.mock.calls[0][0].fingerprint).toBe(SAMPLE_CONFIG.fingerprint);

    // Refetch returns F2 — the form now has the new fingerprint internally
    await waitFor(() => {
      expect(mocks.getConfig).toHaveBeenCalled();
    });

    // Now make another edit and apply — should use F2
    const webPortField = screen.getByLabelText("web_port") as HTMLInputElement;
    fireEvent.change(webPortField, { target: { value: "8080" } });

    await waitFor(() => {
      // Two dirty paths: log_level (from first edit) + web_port (new)
      expect(screen.getByText(/2 unsaved/)).toBeInTheDocument();
    });

    const applyRes2: ApplyResponse = { applied: true, reload: "reloaded" };
    mocks.postConfigApply.mockResolvedValueOnce(applyRes2);

    fireEvent.click(screen.getByRole("button", { name: /apply/i }));

    await waitFor(() => {
      expect(mocks.postConfigApply).toHaveBeenCalledTimes(2);
    });

    // Second apply must use F2 (the refetched fingerprint)
    expect(mocks.postConfigApply.mock.calls[1][0].fingerprint).toBe(UPDATED_CONFIG.fingerprint);
  });

  // ── Fingerprint refetch on conflict / rejection ──

  it("409 → refetches fingerprint, preserves dirty store", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);
    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Make an edit
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });
    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // 409 on apply — conflict dialog appears, refetch returns fresh fingerprint
    mocks.postConfigApply.mockRejectedValueOnce(
      new ApiError(409, { error: "fingerprint_mismatch" }),
    );
    mocks.getConfig.mockResolvedValueOnce(UPDATED_CONFIG);

    fireEvent.click(screen.getByRole("button", { name: /apply/i }));

    // Conflict dialog visible
    await waitFor(() => {
      expect(screen.getByText(/config changed on disk/i)).toBeInTheDocument();
    });

    // getConfig was called for the refetch
    await waitFor(() => {
      expect(mocks.getConfig).toHaveBeenCalled();
    });

    // Dirty store preserved — still 1 unsaved change
    expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();

    // "Keep editing" — dismiss the dialog
    fireEvent.click(screen.getByRole("button", { name: /keep editing/i }));

    // Make a second edit
    const webPortField = screen.getByLabelText("web_port") as HTMLInputElement;
    fireEvent.change(webPortField, { target: { value: "8080" } });
    await waitFor(() => {
      expect(screen.getByText(/2 unsaved/)).toBeInTheDocument();
    });

    // Next apply uses the fresh fingerprint (F2)
    const applyRes: ApplyResponse = { applied: true, reload: "reloaded" };
    mocks.postConfigApply.mockResolvedValueOnce(applyRes);

    fireEvent.click(screen.getByRole("button", { name: /apply/i }));

    await waitFor(() => {
      expect(mocks.postConfigApply).toHaveBeenCalledTimes(2);
    });
    expect(mocks.postConfigApply.mock.calls[1][0].fingerprint).toBe(UPDATED_CONFIG.fingerprint);
  });

  it("rejected → refetches fingerprint, preserves dirty store, banner shows file-written note", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);
    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Make an edit
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });
    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // Apply returns rejected — file was written but daemon refused the reload
    const applyRes: ApplyResponse = { applied: true, reload: "rejected", detail: "invalid zone config" };
    mocks.postConfigApply.mockResolvedValueOnce(applyRes);
    mocks.getConfig.mockResolvedValueOnce(UPDATED_CONFIG);

    fireEvent.click(screen.getByRole("button", { name: /apply/i }));

    // Rejected banner visible with file-written note
    await waitFor(() => {
      expect(screen.getByText(/config rejected/i)).toBeInTheDocument();
    });
    expect(screen.getByText(/file on disk contains your change/i)).toBeInTheDocument();
    expect(screen.getByText(/invalid zone config/i)).toBeInTheDocument();

    // getConfig was called for the refetch
    await waitFor(() => {
      expect(mocks.getConfig).toHaveBeenCalled();
    });

    // Dirty store preserved — still 1 unsaved change (store was not reset)
    expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();

    // Next apply uses the fresh fingerprint (F2)
    const applyRes2: ApplyResponse = { applied: true, reload: "reloaded" };
    mocks.postConfigApply.mockResolvedValueOnce(applyRes2);

    fireEvent.click(screen.getByRole("button", { name: /apply/i }));

    await waitFor(() => {
      expect(mocks.postConfigApply).toHaveBeenCalledTimes(2);
    });
    expect(mocks.postConfigApply.mock.calls[1][0].fingerprint).toBe(UPDATED_CONFIG.fingerprint);
  });

  // ── Navigation guard: beforeunload ──

  it("registers beforeunload when dirty, removes after discard", async () => {
    const addSpy = vi.spyOn(window, "addEventListener");
    const removeSpy = vi.spyOn(window, "removeEventListener");

    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Initially clean — no beforeunload registered
    expect(addSpy).not.toHaveBeenCalledWith("beforeunload", expect.any(Function));

    // Make dirty
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // beforeunload registered when dirty
    const beforeunloadCalls = addSpy.mock.calls.filter(([ev]) => ev === "beforeunload");
    expect(beforeunloadCalls.length).toBeGreaterThanOrEqual(1);

    // Discard — dirty count goes to 0
    const discardBtn = screen.getByRole("button", { name: /discard/i });
    fireEvent.click(discardBtn);

    await waitFor(() => {
      expect(screen.getByText(/0 unsaved/)).toBeInTheDocument();
    });

    // beforeunload removed
    const removeCalls = removeSpy.mock.calls.filter(([ev]) => ev === "beforeunload");
    expect(removeCalls.length).toBeGreaterThanOrEqual(1);

    addSpy.mockRestore();
    removeSpy.mockRestore();
  });

  it("removes beforeunload after apply success", async () => {
    const removeSpy = vi.spyOn(window, "removeEventListener");
    mocks.postConfigApply.mockResolvedValueOnce({ applied: true, reload: "reloaded" } as ApplyResponse);

    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Make dirty
    fireEvent.change(
      screen.getByLabelText("log_level") as HTMLSelectElement,
      { target: { value: "debug" } },
    );

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // Apply
    fireEvent.click(screen.getByRole("button", { name: /apply/i }));

    // Wait for success banner
    await waitFor(() => {
      expect(screen.getByText(/reloaded/)).toBeInTheDocument();
    });

    // beforeunload removed after apply success (dirty count → 0)
    const removeCalls = removeSpy.mock.calls.filter(([ev]) => ev === "beforeunload");
    expect(removeCalls.length).toBeGreaterThanOrEqual(1);

    removeSpy.mockRestore();
  });
});


// ── Config tab-switch navigation guard tests ──

describe("Config tab-switch guard", () => {
  beforeEach(() => {
    mocks.getConfig.mockResolvedValue(SAMPLE_CONFIG);
    mocks.getState.mockResolvedValue(SAMPLE_STATE);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("shows confirm dialog when switching from dirty Settings to Raw TOML", async () => {
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(false);

    render(<Config />);

    // Wait for Settings tab to load
    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Make dirty
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // Click Raw TOML tab
    fireEvent.click(screen.getByText("Raw TOML"));

    // confirm should have been called
    expect(confirmSpy).toHaveBeenCalled();
    expect(confirmSpy.mock.calls[0][0]).toMatch(/Discard.*unsaved/);

    // Since confirm returned false, we stay on Settings tab
    expect(screen.getByText("Daemon")).toBeInTheDocument();
    // Raw TOML content should NOT be visible
    expect(screen.queryByText("Parsed inventory")).toBeNull();

    confirmSpy.mockRestore();
  });

  it("confirming discard switches tab and resets dirty state", async () => {
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);

    render(<Config />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Make dirty
    fireEvent.change(
      screen.getByLabelText("log_level") as HTMLSelectElement,
      { target: { value: "debug" } },
    );

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // Click Raw TOML tab — confirm returns true
    fireEvent.click(screen.getByText("Raw TOML"));

    // Should have switched to Raw TOML tab
    await waitFor(() => {
      expect(screen.getByText("Parsed inventory")).toBeInTheDocument();
    });

    // Switch back to Settings — should be clean (discard reset the store)
    fireEvent.click(screen.getByText("Settings"));

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Should show 0 unsaved (discard was called)
    expect(screen.getByText(/0 unsaved/)).toBeInTheDocument();

    confirmSpy.mockRestore();
  });

  // ── Per-field guidance (help text) ──

  it("zone mode renders help text", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Zones")).toBeInTheDocument();
    });

    // The zone `mode` field should render its help text under the select
    expect(screen.getByText(/How members combine into one presence result/)).toBeInTheDocument();
  });

  it("zone unavailable_policy renders help text", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Zones")).toBeInTheDocument();
    });

    expect(screen.getByText(/offline.stale sensor/)).toBeInTheDocument();
  });

  it("display blank_mode renders help text", async () => {
    render(<SettingsForm config={DISPLAY_BLANK_MODE} />);

    await waitFor(() => {
      expect(screen.getByText("Displays")).toBeInTheDocument();
    });

    // Both blank_mode and degraded_mode share the same help — at least one must match
    expect(screen.getAllByText(/full display power-off/).length).toBeGreaterThanOrEqual(1);
  });

  // ── Missing daemon enum fields (idle_time_unit, idle_source, stale_sensor_timeout) ──

  it("renders idle_time_unit as enum with correct options", async () => {
    const cfg: ConfigResponse = {
      ...SAMPLE_CONFIG,
      fingerprint: "fe01aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44ee55ff66aa11bb22cc",
      inventory: {
        ...SAMPLE_CONFIG.inventory,
        daemon: {
          ...SAMPLE_CONFIG.inventory.daemon,
          idle_time_unit: "auto",
          idle_source: "auto",
          stale_sensor_timeout: "300s",
        },
      },
    };

    render(<SettingsForm config={cfg} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // idle_time_unit — should be a select element with label
    const idleTimeUnitLabel = screen.getByText("idle_time_unit");
    expect(idleTimeUnitLabel).toBeInTheDocument();

    // idle_source — should be a select element with label
    const idleSourceLabel = screen.getByText("idle_source");
    expect(idleSourceLabel).toBeInTheDocument();

    // stale_sensor_timeout — should be a text input with label
    const staleLabel = screen.getByText("stale_sensor_timeout");
    expect(staleLabel).toBeInTheDocument();
  });

  it("editing the 3 new daemon fields creates valid patches (not locked or unknown)", async () => {
    // Use the raw createPatchStore to test patch assembly directly.
    const { createPatchStore } = await import("../app/config/patch");

    const s = createPatchStore();
    // Simulate edits like the form would produce
    s.trackEdit(["daemon", "idle_time_unit"], "ms");
    s.trackEdit(["daemon", "idle_source"], "wayland");
    s.trackEdit(["daemon", "stale_sensor_timeout"], "600s");

    const patches = s.buildPatches();
    expect(patches).toHaveLength(3);

    // Verify the patch paths — these must be accepted by the server's
    // is_known_config_path and editable-subset checks.
    const paths = patches.map((p) => p.path.join("."));
    expect(paths).toContain("daemon.idle_time_unit");
    expect(paths).toContain("daemon.idle_source");
    expect(paths).toContain("daemon.stale_sensor_timeout");

    // None of these paths should be locked to redacted (none involve creds)
    const noRedacted: string[][] = [];
    expect(s.isLocked(["daemon", "idle_time_unit"], noRedacted)).toBe(false);
    expect(s.isLocked(["daemon", "idle_source"], noRedacted)).toBe(false);
    expect(s.isLocked(["daemon", "stale_sensor_timeout"], noRedacted)).toBe(false);
  });

  // ── DurationField placeholder ──

  it("DurationField shows persistent placeholder when placeholder prop given", async () => {
    const cfg: ConfigResponse = {
      ...SAMPLE_CONFIG,
      fingerprint: "fe03aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44ee55ff66aa11bb22cc",
      inventory: {
        ...SAMPLE_CONFIG.inventory,
        daemon: {
          ...SAMPLE_CONFIG.inventory.daemon,
          startup_holdoff: "",
        },
      },
    };

    render(<SettingsForm config={cfg} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    const holdoffInput = screen.getByLabelText("startup_holdoff") as HTMLInputElement;
    // The placeholder is set when the input is empty
    expect(holdoffInput.placeholder).toBe("30s");
  });

  it("does not show confirm when switching while clean", async () => {
    const confirmSpy = vi.spyOn(window, "confirm");

    render(<Config />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Not dirty — switch to Raw TOML without confirm
    fireEvent.click(screen.getByText("Raw TOML"));

    await waitFor(() => {
      expect(screen.getByText("Parsed inventory")).toBeInTheDocument();
    });

    expect(confirmSpy).not.toHaveBeenCalled();

    confirmSpy.mockRestore();
  });
});


// ── Display mode switch tests ──

describe("DisplaysSection — mode switch", () => {
  beforeEach(() => {
    mocks.getConfig.mockResolvedValue(SAMPLE_CONFIG);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("switching from blank_mode to ladder emits set ladder + remove blank_mode + remove degraded_mode", async () => {
    mocks.getConfig.mockResolvedValue(DISPLAY_BLANK_MODE);

    // Render DisplaysSection directly with a real store for precise patch assertions
    const { createPatchStore } = await import("../app/config/patch");
    const { default: DisplaysSection } = await import("../app/config/DisplaysSection");
    const store = createPatchStore();

    render(
      <DisplaysSection
        displays={DISPLAY_BLANK_MODE.inventory.displays}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("lg-oled")).toBeInTheDocument();
    });

    // Click "Escalation ladder" toggle
    const ladderToggle = screen.getByRole("button", { name: /escalation ladder/i });
    fireEvent.click(ladderToggle);

    const patches = store.buildPatches();

    // Must contain: set ladder, remove blank_mode, remove degraded_mode
    const hasSetLadder = patches.some(
      (p) => p.op === "set" && p.path.join(".") === "displays.lg-oled.ladder",
    );
    const hasRemoveBlank = patches.some(
      (p) => p.op === "remove" && p.path.join(".") === "displays.lg-oled.blank_mode",
    );
    const hasRemoveDegraded = patches.some(
      (p) => p.op === "remove" && p.path.join(".") === "displays.lg-oled.degraded_mode",
    );

    expect(hasSetLadder).toBe(true);
    expect(hasRemoveBlank).toBe(true);
    expect(hasRemoveDegraded).toBe(true);

    // No OTHER patches targeting this display
    const displayPatches = patches.filter(
      (p) => p.path[0] === "displays" && p.path[1] === "lg-oled",
    );
    expect(displayPatches).toHaveLength(3);
  });

  it("switching from ladder to blank_mode emits set blank_mode + remove ladder", async () => {
    // Render DisplaysSection directly with a real store
    const { createPatchStore } = await import("../app/config/patch");
    const { default: DisplaysSection } = await import("../app/config/DisplaysSection");
    const store = createPatchStore();

    render(
      <DisplaysSection
        displays={DISPLAY_WITH_LADDER.inventory.displays}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("lg-oled")).toBeInTheDocument();
    });

    // Click "Simple blank" toggle
    const blankToggle = screen.getByRole("button", { name: /simple blank/i });
    fireEvent.click(blankToggle);

    const patches = store.buildPatches();

    // Must contain: set blank_mode, remove ladder
    const hasSetBlank = patches.some(
      (p) => p.op === "set" && p.path.join(".") === "displays.lg-oled.blank_mode",
    );
    const hasRemoveLadder = patches.some(
      (p) => p.op === "remove" && p.path.join(".") === "displays.lg-oled.ladder",
    );

    expect(hasSetBlank).toBe(true);
    expect(hasRemoveLadder).toBe(true);

    // No OTHER patches targeting this display
    const displayPatches = patches.filter(
      (p) => p.path[0] === "displays" && p.path[1] === "lg-oled",
    );
    expect(displayPatches).toHaveLength(2);
  });

  it("display with NEITHER blank_mode NOR ladder — renders warning card, no crash", async () => {
    mocks.getConfig.mockResolvedValue(DISPLAY_NO_MODE);

    render(<SettingsForm config={DISPLAY_NO_MODE} />);

    await waitFor(() => {
      expect(screen.getByText("Displays")).toBeInTheDocument();
    });

    // Should render the display name
    expect(screen.getByText("monitor")).toBeInTheDocument();
    // Updated warning text (no stale "until this editor ships" copy)
    expect(screen.getByText(/neither blank_mode nor a ladder/)).toBeInTheDocument();
  });

  it("queued mode switch — ladder editor visible immediately with 'applies on Apply' hint", async () => {
    // Use DISPLAY_BLANK_MODE — currently in blank mode
    const { createPatchStore } = await import("../app/config/patch");
    const { default: DisplaysSection } = await import("../app/config/DisplaysSection");
    const store = createPatchStore();

    render(
      <DisplaysSection
        displays={DISPLAY_BLANK_MODE.inventory.displays}
        store={store}
        redactedPaths={[]}
        onDirty={() => {}}
        fieldErrors={{}}
      />,
    );

    await waitFor(() => {
      expect(screen.getByText("lg-oled")).toBeInTheDocument();
    });

    // Fetched mode is blank — "Simple blank" button should be active, ladder NOT
    const blankBtn = screen.getByRole("button", { name: /simple blank/i });
    expect(blankBtn.getAttribute("aria-pressed")).toBe("true");

    // Click "Escalation ladder" → queued mode switch
    fireEvent.click(screen.getByRole("button", { name: /escalation ladder/i }));

    // After click: ladder editor should render immediately (pending state)
    // "Escalation ladder" appears in BOTH the toggle button and the card header
    await waitFor(() => {
      const ladderEls = screen.getAllByText("Escalation ladder");
      expect(ladderEls.length).toBeGreaterThanOrEqual(2);
    });

    // The "applies on Apply" hint should appear
    expect(screen.getByText(/applies on Apply/)).toBeInTheDocument();
  });

  it("display with ladder shows the ladder editor", async () => {
    mocks.getConfig.mockResolvedValue(DISPLAY_WITH_LADDER);

    render(<SettingsForm config={DISPLAY_WITH_LADDER} />);

    await waitFor(() => {
      expect(screen.getByText("Displays")).toBeInTheDocument();
    });

    // lg-oled appears both as the display card name and in the rules section
    const displayNames = screen.getAllByText("lg-oled");
    expect(displayNames.length).toBeGreaterThanOrEqual(1);
    // The ladder editor should render — "Escalation ladder" appears as both
    // the mode toggle button and the ladder card header
    const ladderTexts = screen.getAllByText("Escalation ladder");
    expect(ladderTexts.length).toBeGreaterThanOrEqual(1);
  });

  it("display with redacted source locks sources editor but not ladder editor", async () => {
    mocks.getConfig.mockResolvedValue(DISPLAY_WITH_REDACTED_SOURCE);

    render(<SettingsForm config={DISPLAY_WITH_REDACTED_SOURCE} />);

    await waitFor(() => {
      expect(screen.getByText("Displays")).toBeInTheDocument();
    });

    expect(screen.getByText("tv")).toBeInTheDocument();
  });
});
