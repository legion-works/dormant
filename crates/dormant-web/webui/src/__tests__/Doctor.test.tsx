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
    expect(screen.getByText("n/a")).toBeInTheDocument();
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

  it("shows four summary tiles and launches exercise for a chosen display", async () => {
    // Adaptation: the plan's RED test draft declares its own standalone
    // `runDoctor` mock (checks: config/ok, mqtt/warn, usb/skip,
    // ddcci/fail). This file has a single hoisted `api` object shared by
    // every test (mirroring the DisplayDetail.test.tsx "single final
    // definition" precedent — two competing `vi.mock("../api/client", …)`
    // factories are not possible), so the pre-T8 tests' six-check fixture
    // stays the shared default and this test overrides it for one call
    // via `mockResolvedValueOnce` with the plan's exact data instead.
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
    await waitFor(() => expect(screen.getByText("Warnings")).toBeInTheDocument());
    expect(screen.getByText("Passing")).toBeInTheDocument();
    expect(screen.getByText("Skipped")).toBeInTheDocument();
    expect(screen.getByText("Failing")).toBeInTheDocument();
    expect(screen.getAllByText("1")).toHaveLength(4);
    expect(screen.getByRole("combobox", { name: "Exercise display" })).toHaveValue("main");
    expect(screen.getByRole("button", { name: "Run control-path exercise" })).toBeInTheDocument();
  });
});
