/**
 * Dashboard v2 test — guarded quick-action chips + v2 exposure cards.
 *
 * Adaptation from the plan draft: this repo has no react-router
 * dependency (see `../app/nav.tsx` — navigation is a minimal
 * hash-based `useNavigate` hook, not a router). The draft wrapped
 * `<Dashboard />` in `react-router-dom`'s `MemoryRouter`; that import
 * does not exist here, so this test renders `<Dashboard />` directly
 * and leaves the real `useNavigate` hook in place (unmocked) — it only
 * writes to `window.location.hash`, which jsdom supports natively.
 * Every behavioral assertion from the draft is unchanged.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import Dashboard from "../app/views/Dashboard";
import type { DisplayConfig } from "../api/types";

const mocks = vi.hoisted(() => ({
  postBlank: vi.fn().mockResolvedValue(undefined),
  postWake: vi.fn().mockResolvedValue(undefined),
  selectDisplay: vi.fn(),
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
          cmd_gen: 7,
          controllers: [{ name: "ddcci", role: "primary", healthy: true }],
        }]],
        pending_reload: null,
      },
      config: {
        path: "/tmp/config.toml",
        config_version: 1,
        source: "last_applied",
        raw_toml: "",
        inventory: {
          config_version: 1,
          daemon: {},
          sensors: {},
          zones: {},
          displays: {
            main: { controllers: ["ddcci"], blank_mode: "power_off" } as DisplayConfig,
          },
          rules: { office: { zone: "office", displays: ["main"] } },
        },
        validation: { ok: true, warnings: [], errors: [] },
        display_rules: { main: { rule: "office", zone: "office" } },
        fingerprint: "abc",
        redacted_paths: [],
      },
      displayConfigs: {
        main: { controllers: ["ddcci"], blank_mode: "power_off" } as DisplayConfig,
      },
      displayRules: { main: { rule: "office", zone: "office" } },
      wear: { displays: [{
        display: "panel-main",
        display_name: "main",
        panel_type: "woled",
        total_on_hours: 120.5,
        sample_count: 300,
        advisory: true,
        hours_since_long_dwell: 96,
      }] },
      wearDetails: {
        main: {
          display: "panel-main",
          display_name: "main",
          panel_type: "woled",
          total_on_hours: 120.5,
          sample_count: 300,
          advisory: true,
          hours_since_long_dwell: 96,
          grid_rows: 9,
          grid_cols: 16,
          cells: Array(144).fill(0),
          heat: Array(144).fill(0),
        },
      },
      selectDisplay: mocks.selectDisplay,
    }),
    useEventLog: () => eventLogFixture(),
  };
});
vi.mock("../api/client", () => ({ postBlank: mocks.postBlank, postWake: mocks.postWake }));

afterEach(() => { cleanup(); vi.clearAllMocks(); });

describe("Dashboard v2", () => {
  it("shows a guarded quick action and v2 exposure card", async () => {
    render(<Dashboard />);

    expect(screen.getByText("Quick actions")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Wake main" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Blank main" }));
    expect(screen.getByRole("alertdialog", { name: "Force blank main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Force blank" }));
    await waitFor(() => expect(mocks.postBlank).toHaveBeenCalledWith("main"));

    expect(screen.getByText("120.5h total on-time")).toBeInTheDocument();
    expect(screen.getByText("no long standby window in 4 days")).toBeInTheDocument();
    expect(screen.queryByText(/spatial attribution/i)).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Open main panel detail" }));
    expect(mocks.selectDisplay).toHaveBeenCalledWith("main");
  });

  it("does not post when the blank/wake confirmation is cancelled", async () => {
    render(<Dashboard />);

    // `run()`'s `if (!accepted) return;` sits behind `await confirm(...)`,
    // whose promise is resolved *synchronously* inside the Cancel button's
    // onClick (see useConfirmDialog's `finish`). That means the `.then`
    // continuation which would call postBlank/postWake is only scheduled
    // as a microtask — it hasn't run yet immediately after `fireEvent.click`
    // returns. An `act(async () => { await Promise.resolve() x2 })` flush
    // is required before asserting "not called", otherwise a mutant that
    // ignores `accepted` would still pass this test by accident (verified
    // empirically: without the flush this test stayed green against a
    // deliberately broken `run()`).
    const flush = () => act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    fireEvent.click(screen.getByRole("button", { name: "Blank main" }));
    expect(screen.getByRole("alertdialog", { name: "Force blank main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await flush();
    expect(mocks.postBlank).not.toHaveBeenCalled();
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Wake main" }));
    expect(screen.getByRole("alertdialog", { name: "Force wake main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await flush();
    expect(mocks.postWake).not.toHaveBeenCalled();
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  });
});
