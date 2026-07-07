/**
 * Displays component test — renders per-display cards with actions.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import Displays from "../app/views/Displays";
import { LiveStateProvider } from "../app/state";


const { SAMPLE_STATE, SAMPLE_CONFIG, mocks } = vi.hoisted(() => {
  const postBlank = vi.fn().mockResolvedValue(undefined);
  const postWake = vi.fn().mockResolvedValue(undefined);
  const postPause = vi.fn().mockResolvedValue(undefined);
  const postResume = vi.fn().mockResolvedValue(undefined);
  return {
    mocks: { postBlank, postWake, postPause, postResume },
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
        [
          "lg-oled",
          {
            phase: "staged",
            inhibited: false,
            paused: false,
            cmd_gen: 7,
            controllers: [{ name: "lg-webos", role: "primary" as const, healthy: true }],
            stage: { idx: 0, kind: "render_black" },
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
          "lg-oled": { controllers: ["lg-webos"], blank_mode: "power_off" as const, ladder: [{ kind: "render_black", dwell: "5s" }] },
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
        "lg-oled": { rule: "office-rule", zone: "office" },
      },
    },
  };
});

vi.mock("../api/client", () => ({
  getState: vi.fn().mockResolvedValue(SAMPLE_STATE),
  getConfig: vi.fn().mockResolvedValue(SAMPLE_CONFIG),
  postBlank: mocks.postBlank,
  postWake: mocks.postWake,
  postPause: mocks.postPause,
  postResume: mocks.postResume,
}));

vi.mock("../api/ws", () => ({
  useEvents: vi.fn(() => ({ connected: false, close: vi.fn() })),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Displays", () => {
  it("renders display cards with IDs and phases", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    expect(screen.getByText("samsung-tv")).toBeInTheDocument();
    expect(screen.getByText("active")).toBeInTheDocument();
    expect(screen.getByText("blanked")).toBeInTheDocument();
  });

  it("renders paused and inhibited chips", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("paused")).toBeInTheDocument();
    });
  });

  it("renders controller health chips", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    expect(screen.getByText("ddcci")).toBeInTheDocument();
    expect(screen.getByText("kwin-dpms")).toBeInTheDocument();
    expect(screen.getAllByText("primary").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("fallback").length).toBeGreaterThanOrEqual(1);
  });

  it("renders metric fields", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

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
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    // Both displays map to the "office" zone in the fixture
    expect(screen.getAllByText("office").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("office-rule").length).toBeGreaterThanOrEqual(1);
  });

  it("calls postBlank/postWake/postPause/postResume with correct ids", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });
    expect(screen.getByText("samsung-tv")).toBeInTheDocument();

    // First card (aoc-main): not paused → "Pause rule", "Force blank", "Force wake"
    const blankBtns = screen.getAllByText("Force blank");
    const wakeBtns = screen.getAllByText("Force wake");
    const pauseBtns = screen.getAllByText("Pause rule");
    const resumeBtns = screen.getAllByText("Resume rule");

    // Click Force blank on first display
    fireEvent.click(blankBtns[0]);
    await waitFor(() => expect(mocks.postBlank).toHaveBeenCalledWith("aoc-main"));

    // Click Force wake on first display
    fireEvent.click(wakeBtns[0]);
    await waitFor(() => expect(mocks.postWake).toHaveBeenCalledWith("aoc-main"));

    // Click Pause rule on first display (aoc-main → rule "office-rule")
    fireEvent.click(pauseBtns[0]);
    await waitFor(() => expect(mocks.postPause).toHaveBeenCalledWith({ rule: "office-rule" }));

    // Click Resume rule on second display (samsung-tv is paused → rule "tv-rule")
    expect(resumeBtns.length).toBeGreaterThanOrEqual(1);
    fireEvent.click(resumeBtns[0]);
    await waitFor(() => expect(mocks.postResume).toHaveBeenCalledWith({ rule: "tv-rule" }));
  });

  it("has Force wake and Pause/Resume buttons for each display", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

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

  it("renders stage detail when a display is in the staged phase", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("lg-oled")).toBeInTheDocument();
    });

    // The chip should show "staged · render black" for the staged display.
    expect(screen.getByText("staged · render black")).toBeInTheDocument();
  });

  it("does not render stage detail for non-staged displays", async () => {
    render(<LiveStateProvider><Displays /></LiveStateProvider>);

    await waitFor(() => {
      expect(screen.getByText("aoc-main")).toBeInTheDocument();
    });

    // The active display shows its phase label as "active" (unchanged).
    expect(screen.getByText("active")).toBeInTheDocument();
    // The blanked display shows "blanked".
    expect(screen.getByText("blanked")).toBeInTheDocument();

    // The stage-detail label only appears ONCE — for the staged display.
    const stageLabels = screen.getAllByText(/render black/);
    expect(stageLabels).toHaveLength(1);
  });
