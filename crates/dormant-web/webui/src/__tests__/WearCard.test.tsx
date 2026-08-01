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
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import WearCard from "../app/components/WearCard";
import { liveStateFixture } from "./fixtures/live-state";
import type { WearSummary } from "../api/types";

const mocks = vi.hoisted(() => ({
  selectDisplay: vi.fn(),
  state: { current: null as unknown },
  getConfig: vi.fn(),
  getWearSamplingStatus: vi.fn(),
  postWearSamplingEnable: vi.fn(),
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
}));

afterEach(() => {
  vi.useRealTimers();
  cleanup();
  vi.resetAllMocks();
  mocks.state.current = null;
  mocks.getConfig.mockResolvedValue({ inventory: { wear: { active_sampling: { enabled: false } } } });
  mocks.getWearSamplingStatus.mockResolvedValue({ status: "granted" });
  window.location.hash = "";
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
});
