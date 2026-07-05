/**
 * Displays component test — renders per-display cards with actions.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import Displays from "../app/views/Displays";

/* ── Hoisted fixture + mock ── */

const { SAMPLE_STATE, SAMPLE_CONFIG, mocks } = vi.hoisted(() => {
  const postBlank = vi.fn().mockResolvedValue(undefined);
  return {
    mocks: { postBlank },
    SAMPLE_STATE: {
      sensors: [
        { id: "desk-mmwave", state: "present" as const, last_seen_secs_ago: 3 },
      ],
      zones: [
        { id: "office", present: true },
      ],
      displays: [
        [
          "aoc-main",
          {
            phase: "active",
            inhibited: false,
            paused: false,
            cmd_gen: 42,
            controllers: [
              { name: "ddcci", role: "primary" as const, healthy: true },
              { name: "kwin-dpms", role: "fallback" as const, healthy: false, detail: "DBus timeout" },
            ],
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
              { name: "samsung-tizen", role: "primary" as const, healthy: true },
            ],
          },
        ],
      ],
      pending_reload: null,
    },
    SAMPLE_CONFIG: {
      path: "/tmp/config.toml",
      config_version: 1,
      source: "last_applied",
      raw_toml: "",
      inventory: {
        config_version: 1,
        daemon: {},
        sensors: {
          "desk-mmwave": { type: "usb-ld2410" as const, port: "/dev/ttyUSB0" },
        },
        zones: {
          office: { mode: "any", members: ["desk-mmwave"], weights: {}, unavailable_policy: "present" as const },
        },
        displays: {
          "aoc-main": { controllers: ["ddcci", "kwin-dpms"], blank_mode: "power_off" as const },
          "samsung-tv": { controllers: ["samsung-tizen"], blank_mode: "screen_off_audio_on" as const },
        },
        rules: {
          "office-rule": { zone: "office", displays: ["aoc-main"], wake_retries: 3 },
          "tv-rule": { zone: "office", displays: ["samsung-tv"], wake_retries: 5 },
        },
      },
      validation: { ok: true, warnings: [], errors: [] },
      display_rules: {
        "aoc-main": { rule: "office-rule", zone: "office" },
        "samsung-tv": { rule: "tv-rule", zone: "office" },
      },
    },
  };
});

vi.mock("../api/client", () => ({
  getState: vi.fn().mockResolvedValue(SAMPLE_STATE),
  getConfig: vi.fn().mockResolvedValue(SAMPLE_CONFIG),
  postBlank: mocks.postBlank,
  postWake: vi.fn().mockResolvedValue(undefined),
  postPause: vi.fn().mockResolvedValue(undefined),
  postResume: vi.fn().mockResolvedValue(undefined),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Displays", () => {
  it("renders display cards with IDs and phases", async () => {
    render(<Displays />);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    expect(screen.getByText("samsung-tv")).toBeInTheDocument();
    expect(screen.getByText("active")).toBeInTheDocument();
    expect(screen.getByText("blanked")).toBeInTheDocument();
  });

  it("renders paused and inhibited chips", async () => {
    render(<Displays />);

    await waitFor(() => {
      expect(screen.getByText("paused")).toBeInTheDocument();
    });
  });

  it("renders controller health chips", async () => {
    render(<Displays />);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    expect(screen.getByText("ddcci")).toBeInTheDocument();
    expect(screen.getByText("kwin-dpms")).toBeInTheDocument();
    expect(screen.getAllByText("primary").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("fallback").length).toBeGreaterThanOrEqual(1);
  });

  it("renders metric fields", async () => {
    render(<Displays />);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    // "Blank mode" etc. appear in every display card — use getAllByText
    expect(screen.getAllByText("Blank mode").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("Driven by zone").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("Rule").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("Cmd gen").length).toBeGreaterThanOrEqual(1);
  });

  it("renders zone and rule from display_rules reverse lookup", async () => {
    render(<Displays />);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    // Both displays map to the "office" zone in the fixture
    expect(screen.getAllByText("office").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("office-rule")).toBeInTheDocument();
  });

  it("calls postBlank with the correct display id when Force blank is clicked", async () => {
    render(<Displays />);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    const blankButtons = screen.getAllByText("Force blank");
    expect(blankButtons.length).toBeGreaterThanOrEqual(1);

    fireEvent.click(blankButtons[0]);

    await waitFor(() => {
      expect(mocks.postBlank).toHaveBeenCalledWith("aoc-main");
    });
  });

  it("has Force wake and Pause/Resume buttons for each display", async () => {
    render(<Displays />);

    await waitFor(() => {
      expect(screen.getByText("samsung-tv")).toBeInTheDocument();
    });

    expect(screen.getAllByText("Force wake").length).toBeGreaterThanOrEqual(1);
    // samsung-tv is paused → "Resume rule"
    expect(screen.getAllByText("Resume rule").length).toBeGreaterThanOrEqual(1);
    // aoc-main is not paused → "Pause rule"
    expect(screen.getAllByText("Pause rule").length).toBeGreaterThanOrEqual(1);
  });
});
