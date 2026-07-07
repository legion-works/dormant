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


const { mocks, SAMPLE_CONFIG, UPDATED_CONFIG, SAMPLE_STATE } = vi.hoisted(() => {
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

  return {
    mocks: { postConfigApply, getConfig, getState, postReload },
    SAMPLE_CONFIG,
    UPDATED_CONFIG,
    SAMPLE_STATE,
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
