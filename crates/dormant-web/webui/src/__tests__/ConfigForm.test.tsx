/**
 * Settings form tests — editable config sections, apply flow,
 * conflict handling, and inline validation errors.
 */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import { SettingsForm } from "../app/config/SettingsForm";
import type { ConfigResponse, ApplyResponse } from "../api/types";
import { ApiError } from "../api/client";


const { mocks, SAMPLE_CONFIG, UPDATED_CONFIG } = vi.hoisted(() => {
  const postConfigApply = vi.fn();
  const getConfig = vi.fn();

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
    fingerprint: "abc123def4567890abc123def4567890abc123def4567890abc123def4567890",
    redacted_paths: REDACTED_BROKER,
  };

  const UPDATED_CONFIG: ConfigResponse = {
    ...SAMPLE_CONFIG,
    fingerprint: "def456abc7890123def456abc7890123def456abc7890123def456abc78901",
    config_version: 3,
  };

  return {
    mocks: { postConfigApply, getConfig },
    SAMPLE_CONFIG,
    UPDATED_CONFIG,
  };
});

vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    getConfig: mocks.getConfig,
    postConfigApply: mocks.postConfigApply,
  };
});

beforeEach(() => {
  mocks.getConfig.mockResolvedValue(SAMPLE_CONFIG);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});


describe("SettingsForm", () => {
  it("renders sections from fixture", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);

    // Section headers
    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });
    expect(screen.getByText("Sensors")).toBeInTheDocument();
    expect(screen.getByText("Zones")).toBeInTheDocument();
    expect(screen.getByText("Rules")).toBeInTheDocument();
    expect(screen.getByText("Displays")).toBeInTheDocument();

    // Daemon fields
    expect(screen.getByText("log_level")).toBeInTheDocument();
    expect(screen.getByText("web_port")).toBeInTheDocument();

    // Sensor names — may appear in multiple places (card name + zone members)
    const mmwaveEls = screen.getAllByText("desk-mmwave");
    expect(mmwaveEls.length).toBeGreaterThanOrEqual(1);
    const pirEls = screen.getAllByText("room-pir");
    expect(pirEls.length).toBeGreaterThanOrEqual(1);

    // Zone name — may also appear in rule zone display
    const officeEls = screen.getAllByText("office");
    expect(officeEls.length).toBeGreaterThanOrEqual(1);

    // Rule name
    expect(screen.getByText("office-rule")).toBeInTheDocument();
  });

  it("edit marks dirty and ApplyBar count increments", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Before any edit, ApplyBar should show 0 unsaved changes
    expect(screen.getByText(/0 unsaved/)).toBeInTheDocument();

    // Change log_level select
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    // ApplyBar should now show 1 unsaved change
    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });
  });

  it("locked field disabled with tooltip (redacted broker_url)", async () => {
    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      // Multiple room-pir elements (card name + zone member chip)
      const pirEls = screen.getAllByText("room-pir");
      expect(pirEls.length).toBeGreaterThanOrEqual(1);
    });

    // Find the broker_url field for room-pir — it should be disabled
    // The field is locked because it's in redacted_paths
    const brokerFields = screen.getAllByLabelText("broker_url");
    // At least one broker_url field exists (room-pir's mqtt sensor)
    expect(brokerFields.length).toBeGreaterThanOrEqual(1);

    // The locked broker_url should be disabled
    const lockedBroker = brokerFields[0] as HTMLInputElement;
    expect(lockedBroker.disabled).toBe(true);

    // Check that a lock indicator / tooltip is present
    // The locked field wrapper should have a title or aria-label
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

    // Edit log_level to make the form dirty
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // Click Apply
    const applyBtn = screen.getByRole("button", { name: /apply/i });
    fireEvent.click(applyBtn);

    // Wait for the apply call and success banner
    await waitFor(() => {
      expect(mocks.postConfigApply).toHaveBeenCalledTimes(1);
    });

    // Verify the posted body
    const callArgs = mocks.postConfigApply.mock.calls[0][0];
    expect(callArgs.fingerprint).toBe(SAMPLE_CONFIG.fingerprint);

    // Only dirty paths in patches — should be just log_level
    expect(callArgs.patches).toHaveLength(1);
    expect(callArgs.patches[0]).toMatchObject({
      op: "set",
      path: ["daemon", "log_level"],
      value: "debug",
    });

    // Green success banner
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

    // Edit to make dirty
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // Click Apply
    const applyBtn = screen.getByRole("button", { name: /apply/i });
    fireEvent.click(applyBtn);

    // Conflict dialog should appear
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

    // Edit port to trigger dirty
    const portField = screen.getByLabelText("port") as HTMLInputElement;
    fireEvent.change(portField, { target: { value: "/dev/invalid" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // Click Apply
    const applyBtn = screen.getByRole("button", { name: /apply/i });
    fireEvent.click(applyBtn);

    // Inline error should appear near the port field
    await waitFor(() => {
      expect(screen.getByText(/must be a valid device path/)).toBeInTheDocument();
    });
  });

  it("pending — neutral banner + re-fetch called", async () => {
    const applyRes: ApplyResponse = { applied: true, reload: "pending", detail: "queued for reload" };
    mocks.postConfigApply.mockResolvedValueOnce(applyRes);
    mocks.getConfig.mockResolvedValueOnce(UPDATED_CONFIG);

    render(<SettingsForm config={SAMPLE_CONFIG} />);

    await waitFor(() => {
      expect(screen.getByText("Daemon")).toBeInTheDocument();
    });

    // Edit to make dirty
    const logLevelSelect = screen.getByLabelText("log_level") as HTMLSelectElement;
    fireEvent.change(logLevelSelect, { target: { value: "debug" } });

    await waitFor(() => {
      expect(screen.getByText(/1 unsaved/)).toBeInTheDocument();
    });

    // Click Apply
    const applyBtn = screen.getByRole("button", { name: /apply/i });
    fireEvent.click(applyBtn);

    // Neutral/pending banner should appear
    await waitFor(() => {
      expect(screen.getByText(/pending/)).toBeInTheDocument();
    });

    // getConfig should have been called to re-fetch
    await waitFor(() => {
      expect(mocks.getConfig).toHaveBeenCalled();
    });
  });
});
