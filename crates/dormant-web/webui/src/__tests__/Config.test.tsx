/**
 * Config component test — rendered config + validation + reload.
 */
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
        displays: {},
        rules: {
          "office-rule": { zone: "office", displays: [], wake_retries: 3 },
        },
      },
      validation: { ok: true, warnings: [], errors: [] },
      display_rules: {},
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

describe("Config", () => {
  it("renders config path in the file viewer header", async () => {
    render(<Config />);

    await waitFor(() => {
      expect(screen.getByText("/home/user/.config/dormant/config.toml")).toBeInTheDocument();
    });
  });

  it("renders raw TOML line-by-line in the file body", async () => {
    render(<Config />);

    await waitFor(() => {
      expect(screen.getByText('[daemon]')).toBeInTheDocument();
    });

    expect(screen.getByText("web_port")).toBeInTheDocument();
    expect(screen.getByText("9777")).toBeInTheDocument();
    expect(screen.getByText('[sensors."desk-mmwave"]')).toBeInTheDocument();
    expect(screen.getByText('"usb-ld2410"')).toBeInTheDocument();
  });

  it("renders validation OK state", async () => {
    render(<Config />);

    await waitFor(() => {
      expect(screen.getByText(/Configuration parsed with no unknown keys/)).toBeInTheDocument();
    });
  });

  it("renders parsed inventory with sensor/zone/rule counts", async () => {
    render(<Config />);

    await waitFor(() => {
      expect(screen.getByText("Parsed inventory")).toBeInTheDocument();
    });

    expect(screen.getByText("Sensors")).toBeInTheDocument();
    expect(screen.getByText("2")).toBeInTheDocument(); // 2 sensors
    // Inventory has: 2 sensors, 1 zone, 0 displays, 1 rule
    // "1" appears for both zone count and rule count
    const vals = screen.getAllByText("1");
    expect(vals.length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText("0")).toBeInTheDocument(); // displays
  });

  it("renders a reload button that calls postReload", async () => {
    render(<Config />);

    await waitFor(() => {
      expect(screen.getByText("↻ Reload config")).toBeInTheDocument();
    });

    fireEvent.click(screen.getByText("↻ Reload config"));
    await waitFor(() => {
      expect(mocks.postReload).toHaveBeenCalled();
    });
  });

  it("renders validation error state", async () => {
    vi.mocked((await import("../api/client")).getConfig).mockResolvedValueOnce({
      ...SAMPLE_CONFIG,
      validation: {
        ok: false,
        warnings: [],
        errors: [{ what: "unknown_key", detail: "field 'foo' is not recognized" }],
      },
    } as typeof SAMPLE_CONFIG);

    render(<Config />);

    await waitFor(() => {
      expect(screen.getByText(/unknown_key/)).toBeInTheDocument();
    });
  });
});
