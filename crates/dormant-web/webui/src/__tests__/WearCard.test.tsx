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
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import WearCard from "../app/components/WearCard";
import { liveStateFixture } from "./fixtures/live-state";
import type { WearSummary } from "../api/types";

const mocks = vi.hoisted(() => ({
  selectDisplay: vi.fn(),
  state: { current: null as unknown },
}));

vi.mock("../app/hooks/useLiveState", async () => {
  const { liveStateFixture: fixture } = await import("./fixtures/live-state");
  return {
    useLiveState: () => mocks.state.current ?? fixture(),
  };
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mocks.state.current = null;
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
