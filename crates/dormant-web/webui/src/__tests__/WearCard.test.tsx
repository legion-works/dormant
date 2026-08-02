/**
 * WearCard tests — Dashboard panel-exposure summary.
 *
 * T6 rewrite: WearCard no longer fetches privately (`GET /api/wear` now
 * lives in `LiveStateProvider.refreshWear`) — it just renders whatever
 * `useLiveState()` currently holds. These tests mock `useLiveState`
 * directly (via the shared `liveStateFixture` helper) with the exact
 * provider shapes T4 introduced (`wear`, `wearError`, `selectDisplay`)
 * instead of mocking the API client / WS layer.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import WearCard from "../app/components/WearCard";
import { liveStateFixture } from "./fixtures/live-state";
import type { WearSummary } from "../api/types";

const mocks = vi.hoisted(() => ({
  selectDisplay: vi.fn(),
  state: { current: null as unknown },
  getConfig: vi.fn(),
  getWearSamplingStatus: vi.fn(),
  postWearSamplingEnable: vi.fn(),
  getDaemon: vi.fn(),
  postWearSamplingNudgeDismiss: vi.fn(),
}));

vi.mock("../app/hooks/useLiveState", async () => {
  const { liveStateFixture: fixture } = await import("./fixtures/live-state");
  return {
    useLiveState: () => mocks.state.current ?? fixture(),
  };
});

vi.mock("../api/client", () => ({
  getConfig: mocks.getConfig,
  getWearSamplingStatus: mocks.getWearSamplingStatus,
  postWearSamplingEnable: mocks.postWearSamplingEnable,
  getDaemon: mocks.getDaemon,
  postWearSamplingNudgeDismiss: mocks.postWearSamplingNudgeDismiss,
}));

// Set defaults BEFORE each test runs (vitest runs `beforeEach` before the
// first `it` — `afterEach` only fires after the first test, so the first
// test would otherwise see mocks with no implementation and the daemon
// identity would resolve to `undefined`).
beforeEach(() => {
  mocks.state.current = null;
  mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: false } } } });
  mocks.getWearSamplingStatus.mockResolvedValue({ status: "granted" });
  // Default: daemon identity reports wear_sampling is supported on this host
  // AND the user has not dismissed the onboarding nudge. Individual tests
  // override either field to drive the platform-capability gate and the
  // persisted-dismissal flag, mirroring the star-nudge pattern.
  mocks.getDaemon.mockResolvedValue({
    pid: 1,
    started_epoch_s: 0,
    version: "test",
    socket: "/tmp/dormant.sock",
    wear_sampling_supported: true,
    wear_sampling_nudge_dismissed: false,
  });
  window.location.hash = "";
});

afterEach(() => {
  vi.useRealTimers();
  cleanup();
  vi.resetAllMocks();
});

function summary(overrides: Partial<WearSummary> = {}): WearSummary {
  return {
    display: "panel-office",
    display_name: "Office Monitor",
    panel_type: "qd-oled",
    total_on_hours: 123.4,
    sample_count: 42,
    advisory: false,
    hours_since_long_dwell: 0,
    ...overrides,
  };
}

function setState(overrides: Parameters<typeof liveStateFixture>[0]) {
  mocks.state.current = liveStateFixture({ selectDisplay: mocks.selectDisplay, ...overrides });
}

describe("WearCard", () => {
  it("polls once after awaiting consent and stops at granted or denied", async () => {
    mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true, sampled_display: "desk" } } } });
    mocks.getWearSamplingStatus
      .mockResolvedValueOnce({ status: "error", reason: "wear_sampling_needs_consent" })
      .mockResolvedValueOnce({ status: "granted" });
    mocks.postWearSamplingEnable.mockResolvedValue({ status: "awaiting_consent" });
    setState({ wear: { displays: [summary({ config_display_id: "desk" })] } });
    render(<WearCard />);
    await screen.findByRole("button", { name: "Enable active sampling" });
    vi.useFakeTimers();
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Enable active sampling" })); });
    await vi.advanceTimersByTimeAsync(1000);
    expect(mocks.getWearSamplingStatus).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(3000);
    expect(mocks.getWearSamplingStatus).toHaveBeenCalledTimes(2);
  });

  it("hides Enable when config is disabled, streaming, or suspended", async () => {
    mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: false } } } });
    mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_needs_consent" });
    setState({ wear: { displays: [summary()] } });
    const { unmount } = render(<WearCard />);
    await waitFor(() => expect(screen.getByText("Needs consent")).toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "Enable active sampling" })).not.toBeInTheDocument();
    unmount();
    mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
    mocks.getWearSamplingStatus.mockResolvedValue({ status: "granted" });
    render(<WearCard />);
    await waitFor(() => expect(screen.getByText("Granted")).toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "Enable active sampling" })).not.toBeInTheDocument();
    cleanup();
    mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_not_active" });
    render(<WearCard />);
    await waitFor(() => expect(screen.getByText("Sampling degraded")).toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "Enable active sampling" })).not.toBeInTheDocument();
  });

  it("stops polling when consent is denied", async () => {
    mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
    mocks.getWearSamplingStatus.mockResolvedValueOnce({ status: "error", reason: "wear_sampling_needs_consent" })
      .mockResolvedValueOnce({ status: "denied" });
    mocks.postWearSamplingEnable.mockResolvedValue({ status: "awaiting_consent" });
    setState({ wear: { displays: [summary()] } });
    render(<WearCard />);
    await screen.findByRole("button", { name: "Enable active sampling" });
    vi.useFakeTimers();
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Enable active sampling" })); });
    await vi.advanceTimersByTimeAsync(1000);
    await vi.advanceTimersByTimeAsync(3000);
    expect(mocks.getWearSamplingStatus).toHaveBeenCalledTimes(2);
  });
  it("shows sampling state, age, and the consent action only for enabled needs-consent configuration", async () => {
    mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
    mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_needs_consent" });
    mocks.postWearSamplingEnable.mockResolvedValue({ status: "awaiting_consent" });
    setState({ wear: { displays: [summary({ last_sample_at_epoch_s: Math.floor(Date.now() / 1000) - 90 })] } });

    render(<WearCard />);

    await waitFor(() => expect(screen.getByText("Needs consent")).toBeInTheDocument());
    expect(screen.getByText("Last sample: 1m ago")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Enable active sampling" }));
    await waitFor(() => expect(mocks.postWearSamplingEnable).toHaveBeenCalledOnce());
  });

  it("shows degraded reasons and terminal consent states without an Enable action", async () => {
    mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
    mocks.getWearSamplingStatus.mockResolvedValue({ status: "denied" });
    setState({ wear: { displays: [summary()] } });

    render(<WearCard />);

    await waitFor(() => expect(screen.getByText("Consent denied")).toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "Enable active sampling" })).not.toBeInTheDocument();
  });
  it("renders the title, honesty-rule caption (no spatial attribution), and per-display summary", () => {
    setState({ wear: { displays: [summary()] } });

    render(<WearCard />);

    expect(screen.getByText("Panel exposure")).toBeInTheDocument();
    expect(screen.getByText("on-time, sampling, and compensation status")).toBeInTheDocument();
    expect(screen.queryByText(/spatial attribution/i)).not.toBeInTheDocument();
    expect(screen.getByText("Office Monitor")).toBeInTheDocument();
    expect(screen.getByText("123.4h total on-time")).toBeInTheDocument();
    expect(screen.getByText("42 samples")).toBeInTheDocument();
  });

  it("applies the success tone and 'compensation window healthy' when advisory is false", () => {
    setState({ wear: { displays: [summary({ advisory: false })] } });

    render(<WearCard />);

    expect(screen.getByTestId("wear-row-Office Monitor")).toHaveClass("wear-row--success");
    expect(screen.getByText("compensation window healthy")).toBeInTheDocument();
    expect(screen.queryByText(/no long standby window/)).not.toBeInTheDocument();
  });

  it("applies the warning tone and exact 'no long standby window in N days' wording when advisory is true", () => {
    setState({
      wear: {
        displays: [summary({ advisory: true, hours_since_long_dwell: 4 * 24 })],
      },
    });

    render(<WearCard />);

    expect(screen.getByTestId("wear-row-Office Monitor")).toHaveClass("wear-row--warning");
    expect(screen.getByText("no long standby window in 4 days")).toBeInTheDocument();
  });

  it("shows a real day count (not '?') when advisory is true but no long dwell has ever been observed (baseline-only)", () => {
    // T8 review Should-fix, carried forward: `hours_since_long_dwell` is
    // always a real server-derived number (baseline or observed), so
    // this never falls back to a "?" day count.
    setState({
      wear: {
        displays: [summary({ advisory: true, hours_since_long_dwell: 5 * 24 })],
      },
    });

    render(<WearCard />);

    expect(screen.getByText("no long standby window in 5 days")).toBeInTheDocument();
    expect(screen.queryByText(/no long standby window in \? days/)).not.toBeInTheDocument();
  });

  it("applies the error tone and a top-level message when wearError is set", () => {
    setState({
      wear: { displays: [summary({ advisory: false })] },
      wearError: "3 wear detail requests failed",
    });

    render(<WearCard />);

    expect(screen.getByText("Wear data unavailable: 3 wear detail requests failed")).toBeInTheDocument();
    expect(screen.getByTestId("wear-row-Office Monitor")).toHaveClass("wear-row--error");
  });

  it("renders a loading state while wear has not been fetched yet", () => {
    setState({ wear: null });

    render(<WearCard />);

    expect(screen.getByText("Loading…")).toBeInTheDocument();
  });

  it("renders an empty state when no displays are tracked yet", () => {
    setState({ wear: { displays: [] } });

    render(<WearCard />);

    expect(screen.getByText("No tracked displays yet.")).toBeInTheDocument();
  });

  it("clicking a summary selects the display and navigates to the Displays view", () => {
    setState({ wear: { displays: [summary()] } });

    render(<WearCard />);

    fireEvent.click(screen.getByRole("button", { name: "Open Office Monitor panel detail" }));

    expect(mocks.selectDisplay).toHaveBeenCalledWith("Office Monitor");
    expect(window.location.hash).toBe("#/displays");
  });

    it("#201 clicking a row with config_display_id selects by stable config id, not display_name", () => {
      // When config_display_id differs from display_name, selection must use the
      // stable config id so the detail panel opens the correct display.
      setState({
        wear: {
          displays: [
            summary({
              display: "panel-office",
              display_name: "Office Monitor",
              config_display_id: "panel-office",
            }),
          ],
        },
      });

      render(<WearCard />);

      fireEvent.click(screen.getByRole("button", { name: "Open Office Monitor panel detail" }));

      // Must select by config_display_id ("panel-office"), not display_name ("Office Monitor").
      expect(mocks.selectDisplay).toHaveBeenCalledWith("panel-office");
      expect(window.location.hash).toBe("#/displays");
    });

    // ── #186 Task 19 — platform-gated active-sampling onboarding nudge ───

  describe("#186 onboarding nudge", () => {
    it("shows the nudge with Enable + Dismiss when platform supports sampling, config is enabled, and attribution is uniform with no consent", async () => {
      mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
      // needs_consent — the IPC consent flow already-returned this rejection
      mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_needs_consent" });
      mocks.postWearSamplingEnable.mockResolvedValue({ status: "awaiting_consent" });
      mocks.postWearSamplingNudgeDismiss.mockResolvedValue(undefined);
      mocks.getDaemon.mockResolvedValue({
        pid: 1, started_epoch_s: 0, version: "test", socket: "/tmp/dormant.sock",
        wear_sampling_supported: true,
        wear_sampling_nudge_dismissed: false,
      });
      setState({
        wear: {
          displays: [summary({ wear_attribution_mode: "uniform" })],
        },
      });

      render(<WearCard />);

      const nudge = await screen.findByTestId("wear-sampling-nudge");
      expect(nudge).toBeInTheDocument();
      // Enable action — same affordance the row already exposes, but moved
      // inside the nudge so the user understands where the portal flow starts.
      expect(within(nudge).getByRole("button", { name: "Enable active sampling" })).toBeInTheDocument();
      // Dismiss action — persists the flag so the nudge never shows again.
      expect(within(nudge).getByRole("button", { name: "Dismiss" })).toBeInTheDocument();
    });

    it("hides the nudge entirely on a non-Linux host (config.enabled is NOT platform capability)", async () => {
      // Even with config enabled, uniform attribution, AND needs_consent — the
      // platform-capability gate from GET /api/daemon must take precedence.
      // A macOS user must never see the portal action.
      mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
      mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_needs_consent" });
      mocks.getDaemon.mockResolvedValue({
        pid: 1, started_epoch_s: 0, version: "test", socket: "/tmp/dormant.sock",
        wear_sampling_supported: false,
        wear_sampling_nudge_dismissed: false,
      });
      setState({
        wear: {
          displays: [summary({ wear_attribution_mode: "uniform" })],
        },
      });

      render(<WearCard />);

      // Wait for the effect to settle (config + daemon + sampling status
      // all resolve), then assert the nudge is absent.
      await waitFor(() => expect(screen.getByText("Needs consent")).toBeInTheDocument());
      expect(screen.queryByTestId("wear-sampling-nudge")).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Enable active sampling" })).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Dismiss" })).not.toBeInTheDocument();
    });

    it("hides the nudge when config is disabled even on a portal-capable platform (user intent gates capability)", async () => {
      // The pre-existing "hides Enable when config is disabled" test
      // (L106) only asserts the Enable BUTTON is absent — it does not
      // assert the nudge DOM is absent. A non-Enable nudge variant
      // could render when config is off and that test would still pass.
      // This case closes the gap: portal-capable Linux + uniform
      // attribution + needs_consent + config disabled → the nudge must
      // NOT render at all. Drop the `samplingEnabled` term from
      // `nudgeVisible` and this test must redden.
      mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: false } } } });
      mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_needs_consent" });
      mocks.getDaemon.mockResolvedValue({
        pid: 1, started_epoch_s: 0, version: "test", socket: "/tmp/dormant.sock",
        wear_sampling_supported: true,
        wear_sampling_nudge_dismissed: false,
      });
      setState({
        wear: {
          displays: [summary({ wear_attribution_mode: "uniform" })],
        },
      });

      render(<WearCard />);

      await waitFor(() => expect(screen.getByText("Needs consent")).toBeInTheDocument());
      // The nudge testid is the entire nudge surface — assert it's gone,
      // not just the Enable button. Future nudge variants that surface
      // without an Enable action would still fail this check.
      expect(screen.queryByTestId("wear-sampling-nudge")).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Enable active sampling" })).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Dismiss" })).not.toBeInTheDocument();
    });

    it("hides the nudge when attribution is sampled (active sampling has already paid for content-weighted data)", async () => {
      mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
      // needs_consent — but the ledger already shows sampled attribution, so
      // the nudge is moot; the tracker is paying for content-weighted data
      // through another path.
      mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_needs_consent" });
      mocks.getDaemon.mockResolvedValue({
        pid: 1, started_epoch_s: 0, version: "test", socket: "/tmp/dormant.sock",
        wear_sampling_supported: true,
        wear_sampling_nudge_dismissed: false,
      });
      setState({
        wear: {
          displays: [summary({ wear_attribution_mode: "sampled" })],
        },
      });

      render(<WearCard />);

      await waitFor(() => expect(screen.getByText("Needs consent")).toBeInTheDocument());
      expect(screen.queryByTestId("wear-sampling-nudge")).not.toBeInTheDocument();
    });

    it("hides the nudge when the user has previously dismissed it (persisted flag)", async () => {
      mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
      mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_needs_consent" });
      mocks.getDaemon.mockResolvedValue({
        pid: 1, started_epoch_s: 0, version: "test", socket: "/tmp/dormant.sock",
        wear_sampling_supported: true,
        wear_sampling_nudge_dismissed: true, // persisted dismissal
      });
      setState({
        wear: {
          displays: [summary({ wear_attribution_mode: "uniform" })],
        },
      });

      render(<WearCard />);

      await waitFor(() => expect(screen.getByText("Needs consent")).toBeInTheDocument());
      expect(screen.queryByTestId("wear-sampling-nudge")).not.toBeInTheDocument();
    });

    it("clicking Dismiss calls the dismiss endpoint and on remount the nudge stays hidden", async () => {
      mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
      mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_needs_consent" });
      mocks.postWearSamplingNudgeDismiss.mockResolvedValue(undefined);
      // First render: not dismissed. Second render (after click): the daemon
      // identity reflects the persisted dismissal on the next poll.
      mocks.getDaemon
        .mockResolvedValueOnce({
          pid: 1, started_epoch_s: 0, version: "test", socket: "/tmp/dormant.sock",
          wear_sampling_supported: true,
          wear_sampling_nudge_dismissed: false,
        })
        .mockResolvedValueOnce({
          pid: 1, started_epoch_s: 0, version: "test", socket: "/tmp/dormant.sock",
          wear_sampling_supported: true,
          wear_sampling_nudge_dismissed: true,
        });
      setState({
        wear: {
          displays: [summary({ wear_attribution_mode: "uniform" })],
        },
      });

      const { unmount } = render(<WearCard />);
      const dismiss = await screen.findByRole("button", { name: "Dismiss" });
      await act(async () => { fireEvent.click(dismiss); });

      await waitFor(() => expect(mocks.postWearSamplingNudgeDismiss).toHaveBeenCalledTimes(1));
      unmount();

      // Second mount: daemon identity reports the flag is now set, so the
      // nudge should stay hidden — proving the dismissal is actually
      // persisted observable, not just a local-state change.
      render(<WearCard />);
      await waitFor(() => expect(screen.getByText("Needs consent")).toBeInTheDocument());
      expect(screen.queryByTestId("wear-sampling-nudge")).not.toBeInTheDocument();
    });

    it("shows a doctor link instead of an Enable affordance when sampling is suspended (port unreachable)", async () => {
      mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: true } } } });
      // The lifecycle is `suspended` (or the IPC flow errors with the
      // portal-unreachable reason). The existing row's Enable lives behind
      // `needs_consent`; when the sampler is suspended instead (target
      // display unavailable), we MUST NOT show that Enable button — invite
      // the user to the doctor view instead.
      mocks.getWearSamplingStatus.mockResolvedValue({ status: "error", reason: "wear_sampling_portal_unreachable" });
      mocks.getDaemon.mockResolvedValue({
        pid: 1, started_epoch_s: 0, version: "test", socket: "/tmp/dormant.sock",
        wear_sampling_supported: true,
        wear_sampling_nudge_dismissed: false,
      });
      setState({
        wear: {
          displays: [summary({ wear_attribution_mode: "uniform" })],
        },
      });

      render(<WearCard />);

      await waitFor(() => expect(screen.getByText("Sampling degraded")).toBeInTheDocument());
      // No Enable — the existing per-row action is needs-consent only, and
      // a portal-unreachable failure is not a consent problem.
      expect(screen.queryByRole("button", { name: "Enable active sampling" })).not.toBeInTheDocument();
      // A link to the Doctor view replaces the dead-end.
      const doctorLink = screen.getByRole("link", { name: /doctor/i });
      expect(doctorLink).toBeInTheDocument();
      expect(doctorLink.getAttribute("href")).toBe("#/doctor");
    });
  });
});
