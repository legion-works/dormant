/**
 * Shell v2 global chrome — rollback/failure banners, transient poll
 * warning, nav badges, config path pill, and the global emergency-wake
 * control (guarded by the shared confirm dialog).
 *
 * Adaptation note (T5): the plan's draft wrapped this render in
 * react-router-dom's `MemoryRouter` and asserted `getByRole("link", …)`
 * nav items. This repo has no router dependency — Shell uses hash-based
 * navigation (`window.location.hash` + a `hashchange` listener, see
 * `nav.tsx`'s `useNavigate`). Nav items render as `<a href="#/…">`
 * anchors, which already carry the ARIA `link` role without a router,
 * so the router wrapper is dropped and behavioral assertions are kept
 * verbatim.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import Shell from "../app/Shell";

const mocks = vi.hoisted(() => ({
  postEmergencyWake: vi.fn().mockResolvedValue({
    paused: true,
    displays: [{ display: "main", ok: true }],
  }),
  selectDisplay: vi.fn(),
}));

vi.mock("../app/state", () => ({
  LiveStateProvider: ({ children }: { children: React.ReactNode }) => <>{children}</>,
}));
vi.mock("../app/hooks/useLiveState", async () => {
  const { liveStateFixture, eventLogFixture } = await import("./fixtures/live-state");
  return {
    useLiveState: () => liveStateFixture({
      snapshot: {
        sensors: [],
        zones: [],
        displays: [["main", {
          phase: "active",
          inhibited: false,
          paused: false,
          cmd_gen: 1,
          controllers: [{ name: "ddcci", role: "primary", healthy: false, detail: "timeout" }],
          last_blank_failed: true,
        }]],
        pending_reload: "rolled back",
        rollback: {
          failed_fp: "12:deadbeef",
          lkg_fp: "11:cafebabe",
          detail: "rolled back to last-known-good",
        },
      },
      config: {
        path: "/home/user/.config/dormant/config.toml",
        config_version: 1,
        source: "last_applied",
        raw_toml: "",
        inventory: { config_version: 1, daemon: {}, sensors: {}, zones: {}, displays: {}, rules: {} },
        validation: { ok: true, warnings: [], errors: [] },
        display_rules: {},
        fingerprint: "abc",
        redacted_paths: [],
      },
      pollWarning: "temporary disconnect",
      selectDisplay: mocks.selectDisplay,
      doctorReport: { checks: [{ name: "config", status: "fail", detail: "bad" }] },
    }),
    useEventLog: () => eventLogFixture({ connected: true }),
  };
});
vi.mock("../api/client", () => ({
  ApiError: class ApiError extends Error {
    status: number;
    body: unknown;
    constructor(status: number, body: unknown) {
      super(`API ${status}`);
      this.status = status;
      this.body = body;
    }
  },
  postEmergencyWake: mocks.postEmergencyWake,
  postReload: vi.fn().mockResolvedValue(undefined),
  getDaemon: vi.fn().mockResolvedValue({
    pid: 48213,
    started_epoch_s: Math.floor(Date.now() / 1000) - 6 * 3600,
    version: "0.2.0",
    socket: "/tmp/dormant.sock",
  }),
}));
vi.mock("../app/views/Dashboard", () => ({ default: () => <div>dashboard view</div> }));
vi.mock("../app/views/Displays", () => ({ default: () => <div>displays view</div> }));
vi.mock("../app/views/Events", () => ({ default: () => <div>events view</div> }));
vi.mock("../app/views/Config", () => ({ default: () => <div>config view</div> }));
vi.mock("../app/views/Doctor", () => ({ default: () => <div>doctor view</div> }));

afterEach(() => { cleanup(); vi.clearAllMocks(); });

describe("Shell v2 global chrome", () => {
  it("shows rollback then failure banners and live nav badges", () => {
    render(<Shell />);
    const banners = screen.getAllByRole("alert");
    expect(banners[0]).toHaveTextContent("Running on rolled-back config");
    expect(banners[1]).toHaveTextContent("blank chain exhausted");
    expect(banners[2]).toHaveTextContent("Live refresh delayed; showing the last snapshot");
    expect(screen.getByRole("link", { name: /Displays 1/i })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Events live/i })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Config rollback/i })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Doctor 1/i })).toBeInTheDocument();
    expect(screen.getByText("/home/user/.config/dormant/config.toml")).toBeInTheDocument();
  });

  it("guards global emergency wake with the shared dialog", async () => {
    render(<Shell />);
    fireEvent.click(screen.getByRole("button", { name: "Emergency wake" }));
    expect(screen.getByRole("alertdialog", { name: "Emergency wake every display?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Wake every display" }));
    await waitFor(() => expect(mocks.postEmergencyWake).toHaveBeenCalledOnce());
    expect(screen.getByRole("status")).toHaveTextContent("1/1 displays woke");
  });
});
