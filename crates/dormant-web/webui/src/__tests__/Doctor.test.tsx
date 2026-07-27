import { useState } from "react";
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, fireEvent } from "@testing-library/react";
import Doctor from "../app/views/Doctor";
import { LiveStateContext } from "../app/hooks/useLiveState";
import { liveStateFixture } from "./fixtures/live-state";
import type { DoctorReport } from "../api/types";

// Doctor now reads `doctorReport`/`setDoctorReport` and display ids from
// `useLiveState()` (provider-owned report, not a local `useState`) so it can
// mount `ExerciseRunner` for a chosen display. That means every test in this
// file — not just the new provider-wiring test — must render `<Doctor />`
// inside a `LiveStateContext.Provider`; a bare `render(<Doctor />)` now
// throws "useLiveState must be used within LiveStateProvider". The pre-T8
// tests below are otherwise unchanged in intent: same fixtures, same
// assertions, just wrapped via `renderDoctor()`.
const api = vi.hoisted(() => ({
  runDoctor: vi.fn().mockResolvedValue({
    checks: [
      { name: "Config valid", status: "ok" as const, detail: "config.toml parsed without errors" },
      { name: "IPC socket reachable", status: "ok" as const, detail: "/run/dormant.sock responds" },
      { name: "MQTT broker connection", status: "ok" as const },
      { name: "Sensor stale check", status: "skip" as const, detail: "no sensors are currently stale" },
      { name: "KWin DPMS controller", status: "fail" as const, detail: "DBus service not reachable" },
      { name: "DDC/CI device present", status: "not_supported" as const, detail: "no DDC/CI displays detected" },
    ],
  }),
  postExercise: vi.fn().mockResolvedValue({
    display: "main",
    pre_phase: "active",
    steps: [{ command: "wake", returned_ok: true, verdict: "confirmed" }],
  }),
}));

vi.mock("../api/client", () => ({
  ...api,
  // Adaptation: explicit-field mock class — see ExerciseRunner.test.tsx's
  // comment (tsconfig `erasableSyntaxOnly` rejects parameter properties).
  ApiError: class ApiError extends Error {
    status: number;
    body: unknown;
    constructor(status: number, body: unknown) {
      super(`API ${status}`);
      this.status = status;
      this.body = body;
    }
  },
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

/** Renders `<Doctor />` inside a `LiveStateContext.Provider`, owning
 * `doctorReport` the same way the real `LiveStateProvider` does (a
 * `useState` lifted above the view). Defaults to no configured displays so
 * pre-T8 assertions about the run/summary/check-list flow are unaffected by
 * the new exercise launcher (which the spec requires hidden when empty). */
function renderDoctor(snapshotOverrides: Partial<NonNullable<ReturnType<typeof liveStateFixture>["snapshot"]>> = {}) {
  function Harness() {
    const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
    const state = liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [],
        pending_reload: null,
        ...snapshotOverrides,
      },
      doctorReport,
      setDoctorReport,
    });
    return (
      <LiveStateContext.Provider value={state}>
        <Doctor />
      </LiveStateContext.Provider>
    );
  }
  return render(<Harness />);
}

describe("Doctor", () => {
  it("renders Run button and empty state before first run", () => {
    renderDoctor();

    expect(screen.getByText("Run doctor")).toBeInTheDocument();
    expect(screen.getByText(/Run diagnostics/)).toBeInTheDocument();
  });

  it("runs doctor on button click and renders results", async () => {
    renderDoctor();

    fireEvent.click(screen.getByText("Run doctor"));

    await waitFor(() => {
      expect(screen.getByText("Config valid")).toBeInTheDocument();
    });

    expect(api.runDoctor).toHaveBeenCalledTimes(1);
  });

  it("renders summary cards with correct counts", async () => {
    renderDoctor();

    fireEvent.click(screen.getByText("Run doctor"));

    await waitFor(() => {
      expect(screen.getByText("Config valid")).toBeInTheDocument();
    });

    expect(screen.getByText("Passing")).toBeInTheDocument();
    expect(screen.getByText("Skipped")).toBeInTheDocument();
    expect(screen.getByText("Failing")).toBeInTheDocument();

    const threeVals = screen.getAllByText("3");
    expect(threeVals.length).toBeGreaterThanOrEqual(1);
    const twoVals = screen.getAllByText("2");
    expect(twoVals.length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("1")).toBeInTheDocument();
  });

  it("renders check detail lines and status tags", async () => {
    renderDoctor();

    fireEvent.click(screen.getByText("Run doctor"));

    await waitFor(() => {
      expect(screen.getByText("Config valid")).toBeInTheDocument();
    });

    expect(screen.getByText("config.toml parsed without errors")).toBeInTheDocument();
    expect(screen.getByText("/run/dormant.sock responds")).toBeInTheDocument();
    expect(screen.getByText("DBus service not reachable")).toBeInTheDocument();

    expect(screen.getAllByText("ok").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("skip")).toBeInTheDocument();
    expect(screen.getByText("fail")).toBeInTheDocument();
    // not_supported is labelled "not applicable on this platform" per spec.
    expect(screen.getByText("not applicable on this platform")).toBeInTheDocument();
  });

  it("shows loading state while running", () => {
    vi.mocked(api.runDoctor).mockReturnValueOnce(new Promise(() => {}));

    renderDoctor();
    fireEvent.click(screen.getByText("Run doctor"));

    expect(screen.getByText("Running…")).toBeInTheDocument();
  });

  it("changes button text after first run", async () => {
    renderDoctor();

    fireEvent.click(screen.getByText("Run doctor"));

    await waitFor(() => {
      expect(screen.getByText("Run again")).toBeInTheDocument();
    });
  });

  it("shows summary tiles and launches exercise for a chosen display", async () => {
    // Two competing `vi.mock` factories are not possible with a shared `api`;
    // override the single mock with `mockResolvedValueOnce` here.
    vi.mocked(api.runDoctor).mockResolvedValueOnce({
      checks: [
        { name: "config", status: "ok", detail: "valid" },
        { name: "mqtt", status: "warn" as unknown as "ok", detail: "slow" },
        { name: "usb", status: "skip", detail: "not configured" },
        { name: "ddcci", status: "fail", detail: "timeout" },
      ],
    });

    function DoctorHarness() {
      const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
      const state = liveStateFixture({
        snapshot: {
          sensors: [],
          zones: [],
          displays: [["main", {
            phase: "active",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
          }]],
          pending_reload: null,
        },
        doctorReport,
        setDoctorReport,
      });
      return <LiveStateContext.Provider value={state}><Doctor /></LiveStateContext.Provider>;
    }

    render(<DoctorHarness />);
    fireEvent.click(screen.getByRole("button", { name: "Run doctor" }));
    await waitFor(() => expect(screen.getByText("Failing")).toBeInTheDocument());
    expect(screen.getByText("Passing")).toBeInTheDocument();
    expect(screen.getByText("Skipped")).toBeInTheDocument();
    // Warnings tile is removed (W0-4). Three tiles: Passing · Failing · Skipped.
    expect(screen.queryByText("Warnings")).not.toBeInTheDocument();
    // Exercise is now a peer panel, not a <select>.
    expect(screen.getByRole("button", { name: "Run control-path exercise" })).toBeInTheDocument();
  });

  it("renders checks grouped by heuristic fallback when no category/subject", async () => {
    vi.mocked(api.runDoctor).mockResolvedValueOnce({
      checks: [
        { name: "config", status: "ok" as const, detail: "valid" },
        { name: "unknown-probe", status: "fail" as const, detail: "something broke" },
      ],
    });

    function Harness() {
      const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
      const state = liveStateFixture({
        doctorReport,
        setDoctorReport,
      });
      return <LiveStateContext.Provider value={state}><Doctor /></LiveStateContext.Provider>;
    }

    render(<Harness />);
    fireEvent.click(screen.getByText("Run doctor"));

    await waitFor(() => {
      // "config" name matches heuristic → CONFIG group header.
      expect(screen.getByText("CONFIG")).toBeInTheDocument();
      // "unknown-probe" doesn't match any heuristic → OTHER group.
      expect(screen.getByText("OTHER")).toBeInTheDocument();
    });

    // The Other bucket contains the unknown probe.
    expect(screen.getByText("unknown-probe")).toBeInTheDocument();
  });

  it("renders exercise runner with button not disabled from local state", async () => {
    vi.mocked(api.runDoctor).mockResolvedValueOnce({
      checks: [],
    });

    function Harness() {
      const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
      const state = liveStateFixture({
        snapshot: {
          sensors: [],
          zones: [],
          displays: [["main", {
            phase: "active",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
          }]],
          pending_reload: null,
        },
        doctorReport,
        setDoctorReport,
      });
      return <LiveStateContext.Provider value={state}><Doctor /></LiveStateContext.Provider>;
    }

    render(<Harness />);
    fireEvent.click(screen.getByText("Run doctor"));

    await waitFor(() => {
      const btn = screen.getByRole("button", { name: "Run control-path exercise" });
      // Exercise button should be enabled when no exercise is in flight.
      expect(btn).not.toBeDisabled();
    });
  });

  it("renders groups using real category/subject when BG-7 data is present", async () => {
    vi.mocked(api.runDoctor).mockResolvedValueOnce({
      checks: [
        { name: "config", status: "ok", detail: "valid", category: "config" },
        { name: "ddcci (studio)", status: "ok", detail: "last attempt succeeded", category: "display", subject: "studio" },
        { name: "mqtt desk-mmwave", status: "fail", detail: "timeout", category: "sensor", subject: "desk-mmwave" },
        { name: "oddball-check", status: "skip", detail: "mystery" },
        // Warm-up A (W4 residual Should): a check whose name would match the
        // display heuristic — `name.match(/\(([^)]+)\)$/)` pulls "studio" —
        // but whose BG-7 category is "sensor", so it lands under SENSOR, not
        // DISPLAY.  The fixture includes "studio" in `displays` so the
        // heuristic WOULD have placed it in DISPLAY had category been absent.
        { name: "panel (studio)", status: "ok", detail: "panel ok", category: "sensor", subject: "studio" },
      ],
    });

    function Harness() {
      const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
      const state = liveStateFixture({
        snapshot: {
          sensors: [],
          zones: [],
          displays: [["studio", { display_id: "studio", phase: "active", blank: false, inhibited: false, paused: false, cmd_gen: 1, controllers: [] }]],
          pending_reload: null,
        },
        doctorReport,
        setDoctorReport,
      });
      return <LiveStateContext.Provider value={state}><Doctor /></LiveStateContext.Provider>;
    }

    render(<Harness />);
    fireEvent.click(screen.getByText("Run doctor"));

    await waitFor(() => {
      // Category headers appear in uppercase.
      // config → CONFIG, display → DISPLAY, sensor → SENSOR.
      // oddball-check (no category) falls into OTHER bucket.
      expect(screen.getByText("oddball-check")).toBeInTheDocument();

      // Warm-up A: "panel (studio)" has category "sensor", subject "studio".
      // The heuristic on its name would match "studio" as a display and place
      // it under DISPLAY, but the BG-7 category wins — assert it renders under
      // the SENSOR group header.  Two SENSOR groups exist (desk-mmwave and
      // studio), so we find the one containing our target check text.
      const sensorGroup = Array.from(document.querySelectorAll(".doctor-group"))
        .find((el) => el.querySelector(".doctor-group__category")?.textContent === "SENSOR"
          && el.textContent?.includes("panel (studio)"));
      expect(sensorGroup).toBeTruthy();
      expect(sensorGroup!.textContent).toContain("panel (studio)");
    });
  });

  it("scrolls and highlights ?subject= group", async () => {
    vi.mocked(api.runDoctor).mockResolvedValueOnce({
      checks: [
        { name: "ddcci (studio)", status: "ok", detail: "ok", category: "display", subject: "studio" },
      ],
    });

    window.location.hash = "#/doctor?subject=studio";

    function Harness() {
      const [doctorReport, setDoctorReport] = useState<DoctorReport | null>(null);
      const state = liveStateFixture({
        doctorReport,
        setDoctorReport,
      });
      return <LiveStateContext.Provider value={state}><Doctor /></LiveStateContext.Provider>;
    }

    render(<Harness />);
    fireEvent.click(screen.getByText("Run doctor"));

    await waitFor(() => {
      const groups = document.querySelectorAll(".doctor-group--highlighted");
      expect(groups.length).toBeGreaterThanOrEqual(1);
    });
  });
});
