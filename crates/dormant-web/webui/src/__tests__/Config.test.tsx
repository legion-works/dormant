import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import Config from "../app/views/Config";


const { mocks, SAMPLE_CONFIG, SAMPLE_STATE } = vi.hoisted(() => {
  const postReload = vi.fn().mockResolvedValue(undefined);
  return {
    mocks: { postReload },
    SAMPLE_CONFIG: {
      path: "/home/user/.config/dormant/config.toml",
      config_version: 2,
      source: "last_applied" as const,
      raw_toml: [
        '[daemon]',
        'web_port = 9777',
        '',
        '[sensors."desk-mmwave"]',
        'type = "usb-ld2410"',
        'port = "/dev/ttyUSB0"',
        '',
        '[zones.office]',
        'mode = "any"',
        'members = ["desk-mmwave"]',
      ].join("\n"),
      inventory: {
        config_version: 2,
        daemon: { web_port: 9777 },
        sensors: {
          "desk-mmwave": { type: "usb-ld2410" as const, port: "/dev/ttyUSB0" },
          "room-pir": { type: "mqtt" as const, broker_url: "tcp://broker:1883", topic: "sensors/pir" },
        },
        zones: {
          office: { mode: "any", members: ["desk-mmwave"], weights: {}, unavailable_policy: "present" as const },
        },
        displays: {
          "lg-oled": {
            controllers: ["lg-webos"],
            blank_mode: "power_off" as const,
            ladder: [
              { kind: "render_black" as const, dwell: "5s" },
              { kind: "render_screensaver" as const, dwell: "30s" },
              { kind: "power_off" as const },
            ],
            screensaver: {
              trigger: "on_stage" as const,
              audio: false,
              source: [
                { path: "/home/user/screensavers/", recurse: true, shuffle: true },
                { urls: ["https://example.com/feed"], image_duration: "10s" },
              ],
            },
          },
        },
        rules: {
          "office-rule": { zone: "office", displays: [], wake_retries: 3 },
        },
      },
      validation: { ok: true, warnings: [], errors: [] },
      display_rules: {},
      fingerprint: "abc123def4567890abc123def4567890abc123def4567890abc123def4567890",
      redacted_paths: [],
    },
    SAMPLE_STATE: {
      sensors: [],
      zones: [],
      displays: [],
      pending_reload: null,
    },
  };
});

vi.mock("../api/client", () => ({
  getConfig: vi.fn().mockResolvedValue(SAMPLE_CONFIG),
  getState: vi.fn().mockResolvedValue(SAMPLE_STATE),
  postReload: mocks.postReload,
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

/** Switch from the default Settings tab to the Raw TOML tab. */
async function openRawToml() {
  await waitFor(() => {
    expect(screen.getByText("Raw TOML")).toBeInTheDocument();
  });
  fireEvent.click(screen.getByText("Raw TOML"));
}

describe("Config", () => {
  it("renders config path in the file viewer header", async () => {
    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText("/home/user/.config/dormant/config.toml")).toBeInTheDocument();
    });
  });

  it("renders raw TOML line-by-line in the file body", async () => {
    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText('[daemon]')).toBeInTheDocument();
    });

    expect(screen.getByText("web_port")).toBeInTheDocument();
    expect(screen.getByText("9777")).toBeInTheDocument();
    expect(screen.getByText('[sensors."desk-mmwave"]')).toBeInTheDocument();
    expect(screen.getByText('"usb-ld2410"')).toBeInTheDocument();
  });

  it("renders validation OK message when config is valid", async () => {
    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText(/Configuration parsed with no unknown keys/)).toBeInTheDocument();
    });
  });

  it("renders parsed inventory with sensor/zone/rule counts", async () => {
    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText("Parsed inventory")).toBeInTheDocument();
    });

    expect(screen.getByText("Sensors")).toBeInTheDocument();
    expect(screen.getByText("2")).toBeInTheDocument(); // 2 sensors
    const vals = screen.getAllByText("1"); // zones, displays, rules: one each
    expect(vals.length).toBeGreaterThanOrEqual(3);
  });

  it("renders a reload button that calls postReload", async () => {
    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText("↻ Reload config")).toBeInTheDocument();
    });

    fireEvent.click(screen.getByText("↻ Reload config"));
    await waitFor(() => {
      expect(mocks.postReload).toHaveBeenCalled();
    });
  });

  it("renders all validation errors when config has multiple errors", async () => {
    vi.mocked((await import("../api/client")).getConfig).mockResolvedValueOnce({
      ...SAMPLE_CONFIG,
      validation: {
        ok: false,
        warnings: [],
        errors: [
          { what: "unknown_key", detail: "field 'foo' is not recognized" },
          { what: "bad_reference", detail: "zone 'ghost' not defined" },
        ],
      },
    });

    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText(/unknown_key/)).toBeInTheDocument();
    });

    expect(screen.getByText(/field 'foo'/)).toBeInTheDocument();
    expect(screen.getByText(/bad_reference/)).toBeInTheDocument();
    expect(screen.getByText(/zone 'ghost'/)).toBeInTheDocument();
    expect(screen.getByText("Validation errors")).toBeInTheDocument();
  });

  it("renders all validation warnings when config has multiple warnings", async () => {
    vi.mocked((await import("../api/client")).getConfig).mockResolvedValueOnce({
      ...SAMPLE_CONFIG,
      validation: {
        ok: true,
        warnings: [
          { key_path: "sensors.old-pir.topic", message: "deprecated — use 'entity_id' instead" },
          { key_path: "daemon.web_port", message: "port below 1024 requires root" },
        ],
        errors: [],
      },
    });

    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText(/sensors.old-pir.topic/)).toBeInTheDocument();
    });

    expect(screen.getByText(/deprecated/)).toBeInTheDocument();
    expect(screen.getByText(/daemon.web_port/)).toBeInTheDocument();
    expect(screen.getByText(/port below 1024/)).toBeInTheDocument();
    expect(screen.getByText("Validation warnings")).toBeInTheDocument();
  });

  it("renders load_error when config fails to parse", async () => {
    vi.mocked((await import("../api/client")).getConfig).mockResolvedValueOnce({
      ...SAMPLE_CONFIG,
      validation: {
        ok: false,
        warnings: [],
        errors: [],
        load_error: "TOML parse error at line 42: unexpected character",
      },
    });

    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText(/TOML parse error/)).toBeInTheDocument();
    });

    expect(screen.getByText("Validation errors")).toBeInTheDocument();
  });

  it("renders pending-reload banner when state has pending_reload", async () => {
    vi.mocked((await import("../api/client")).getState).mockResolvedValueOnce({
      ...SAMPLE_STATE,
      pending_reload: "validating new config…",
    });

    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText(/Config reload pending — validating new config…/)).toBeInTheDocument();
    });
  });

  it("renders source-mismatch banner when config source is not last_applied", async () => {
    vi.mocked((await import("../api/client")).getConfig).mockResolvedValueOnce({
      ...SAMPLE_CONFIG,
      source: "on_disk",
    });

    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText(/Config source: on_disk \(not yet applied\)/)).toBeInTheDocument();
    });
  });
});

  it("renders ladder and screensaver summary when displays have them configured", async () => {
    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText("Ladder & Screensaver")).toBeInTheDocument();
    });

    // Display name — appears in both the key and inventory-names columns.
    const nameEls = screen.getAllByText("lg-oled");
    expect(nameEls.length).toBeGreaterThanOrEqual(2);

    // Ladder stages rendered with dwells in order
    expect(screen.getByText(/render black \(5s\) → render screensaver \(30s\) → power off/)).toBeInTheDocument();

    // Screensaver source count
    expect(screen.getByText("2 sources")).toBeInTheDocument();
  });

  it("does not render ladder section when no display has ladder or screensaver", async () => {
    // Override with config that has no ladder/screensaver on any display.
    vi.mocked((await import("../api/client")).getConfig).mockResolvedValueOnce({
      ...SAMPLE_CONFIG,
      inventory: {
        ...SAMPLE_CONFIG.inventory,
        displays: {
          "aoc-main": { controllers: ["ddcci"], blank_mode: "power_off" as const },
        },
      },
    });

    render(<Config />);
    await openRawToml();

    await waitFor(() => {
      expect(screen.getByText("Parsed inventory")).toBeInTheDocument();
    });

    // Ladder section should be absent.
    expect(screen.queryByText("Ladder & Screensaver")).toBeNull();
  });
