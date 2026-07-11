/**
 * WearCard tests — Dashboard panel-exposure summary.
 *
 * Fetches GET /api/wear (mocked) and renders per-display summaries.
 * The advisory line/banner is driven primarily by the fetched `advisory`
 * flag, and nudged live by `wear_snapshot` (patches on-hours/sample_count)
 * and `compensation_advisory` (nudges the advisory line) WS events.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, act } from "@testing-library/react";
import { LiveStateProvider } from "../app/state";
import WearCard from "../app/components/WearCard";
import type { WearListResponse } from "../api/types";

const { mocks } = vi.hoisted(() => {
  let capturedOnMessage: ((data: unknown) => void) | null = null;

  const useEventsImpl = vi.fn(
    (opts: { onMessage: (data: unknown) => void; onConnect?: () => void }) => {
      capturedOnMessage = opts.onMessage;
      return { connected: true, close: vi.fn() };
    },
  );

  const getWear = vi.fn();
  const getState = vi.fn().mockResolvedValue({
    sensors: [],
    zones: [],
    displays: [],
    pending_reload: null,
  });
  const getConfig = vi.fn().mockResolvedValue({
    path: "/tmp/c.toml",
    config_version: 1,
    source: "last_applied",
    raw_toml: "",
    inventory: {
      config_version: 1,
      daemon: {},
      sensors: {},
      zones: {},
      displays: {},
      rules: {},
    },
    validation: { ok: true, warnings: [], errors: [] },
    display_rules: {},
  });

  return {
    mocks: {
      useEventsImpl,
      getWear,
      getState,
      getConfig,
      get onMessage() {
        return capturedOnMessage;
      },
    },
  };
});

vi.mock("../api/ws", () => ({
  useEvents: mocks.useEventsImpl,
}));

vi.mock("../api/client", () => ({
  getState: mocks.getState,
  getConfig: mocks.getConfig,
  getWear: mocks.getWear,
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const NOW = Math.floor(Date.now() / 1000);

function sample(overrides: Partial<WearListResponse["displays"][number]> = {}): WearListResponse {
  return {
    displays: [
      {
        display: "ddc-aoc-1",
        display_name: "Office Monitor",
        panel_type: "qd-oled",
        total_on_hours: 123.4,
        seeded_usage_hours: 50,
        sample_count: 42,
        last_sample_at_epoch_s: NOW,
        last_long_dwell_epoch_s: NOW - 600, // 10 minutes ago
        advisory: false,
        hours_since_long_dwell: 0,
        ...overrides,
      },
    ],
  };
}

describe("WearCard", () => {
  it("renders the title, honesty-rule caption, and per-display summary", async () => {
    mocks.getWear.mockResolvedValue(sample());

    render(
      <LiveStateProvider>
        <WearCard />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("Panel exposure")).toBeInTheDocument();
    });
    expect(
      screen.getByText(/no spatial attribution yet — arrives with content-aware tracking \(v2\)/),
    ).toBeInTheDocument();
    expect(screen.getByText("Office Monitor")).toBeInTheDocument();
    expect(screen.getByText(/123\.4h total on-time/)).toBeInTheDocument();
    expect(screen.getByText(/\+50h seeded/)).toBeInTheDocument();
  });

  it("does not show an advisory line when the fetch reports advisory=false", async () => {
    mocks.getWear.mockResolvedValue(sample({ advisory: false }));

    render(
      <LiveStateProvider>
        <WearCard />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("Office Monitor")).toBeInTheDocument();
    });
    expect(screen.queryByText(/no long standby window/)).toBeNull();
  });

  it("shows the advisory line worded exactly 'no long standby window in N days' when the fetch reports advisory=true", async () => {
    mocks.getWear.mockResolvedValue(
      sample({
        advisory: true,
        last_long_dwell_epoch_s: NOW - 4 * 86400,
        hours_since_long_dwell: 4 * 24,
      }),
    );

    render(
      <LiveStateProvider>
        <WearCard />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText(/no long standby window in 4 days/)).toBeInTheDocument();
    });
  });

  it("shows a real day count (not '?') when advisory=true but no long dwell has ever been observed (baseline-only)", async () => {
    // T8 review Should-fix: the baseline-only case (a display that has
    // never had an observed long dwell — the common first-load case)
    // must still render a real day count derived from
    // `hours_since_long_dwell` (server-computed from
    // `advisory_baseline_epoch_s`), not fall back to "?".
    mocks.getWear.mockResolvedValue(
      sample({
        advisory: true,
        last_long_dwell_epoch_s: null,
        hours_since_long_dwell: 5 * 24,
      }),
    );

    render(
      <LiveStateProvider>
        <WearCard />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText(/no long standby window in 5 days/)).toBeInTheDocument();
    });
    expect(screen.queryByText(/no long standby window in \? days/)).toBeNull();
  });

  it("compensation_advisory WS event nudges the advisory banner into view", async () => {
    mocks.getWear.mockResolvedValue(sample({ advisory: false }));

    render(
      <LiveStateProvider>
        <WearCard />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("Office Monitor")).toBeInTheDocument();
    });
    expect(screen.queryByText(/no long standby window/)).toBeNull();

    act(() => {
      mocks.onMessage?.({
        event: "compensation_advisory",
        display: "ddc-aoc-1",
        hours_since_long_dwell: 120,
      });
    });

    await waitFor(() => {
      expect(screen.getByText(/no long standby window in 5 days/)).toBeInTheDocument();
    });
  });

  it("wear_snapshot WS event patches the displayed on-hours in place", async () => {
    mocks.getWear.mockResolvedValue(sample());

    render(
      <LiveStateProvider>
        <WearCard />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText(/123\.4h total on-time/)).toBeInTheDocument();
    });

    act(() => {
      mocks.onMessage?.({
        event: "wear_snapshot",
        display: "ddc-aoc-1",
        total_on_hours: 200.5,
        sample_count: 99,
      });
    });

    await waitFor(() => {
      expect(screen.getByText(/200\.5h total on-time/)).toBeInTheDocument();
    });
  });

  it("renders an empty state when no displays are tracked yet", async () => {
    mocks.getWear.mockResolvedValue({ displays: [] });

    render(
      <LiveStateProvider>
        <WearCard />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("No tracked displays yet.")).toBeInTheDocument();
    });
  });
});
