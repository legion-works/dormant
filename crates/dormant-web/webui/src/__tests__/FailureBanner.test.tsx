/**
 * FailureBanner tests — Dashboard-level surfacing of displays that are
 * failing to wake or whose last blank command exhausted its controller
 * chain. Derivation is snapshot-only: `(wake_attempts ?? 0) > 0 ||
 * (last_blank_failed ?? false)`, mirroring dormant-tray's
 * `derive_icon_state` Failure predicate.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup } from "@testing-library/react";
import { LiveStateProvider } from "../app/state";
import { useLiveState } from "../app/hooks/useLiveState";
import FailureBanner from "../app/components/FailureBanner";
import type { StateSnapshot } from "../api/types";

const { mocks } = vi.hoisted(() => {
  const useEventsImpl = vi.fn(() => ({ connected: true, close: vi.fn() }));
  const getState = vi.fn();
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

  return { mocks: { useEventsImpl, getState, getConfig } };
});

vi.mock("../api/ws", () => ({
  useEvents: mocks.useEventsImpl,
}));

vi.mock("../api/client", () => ({
  getState: mocks.getState,
  getConfig: mocks.getConfig,
  getWear: vi.fn().mockResolvedValue({ displays: [] }),
  getWearDetail: vi.fn().mockRejectedValue(new Error("unexpected wear detail request")),
  getOperations: vi.fn().mockResolvedValue({
    exercise_in_flight: [],
    emergency_wake_in_flight: false,
  }),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function stateWith(displays: StateSnapshot["displays"]): StateSnapshot {
  return { sensors: [], zones: [], displays, pending_reload: null };
}

/** Renders once the snapshot has loaded — used to prove a "renders nothing"
 * assertion isn't just catching the pre-fetch render. */
function LoadedMarker() {
  const { snapshot } = useLiveState();
  return <span data-testid="loaded">{snapshot ? "yes" : "no"}</span>;
}

describe("FailureBanner", () => {
  it("renders a wake-failing display with its attempt count", async () => {
    mocks.getState.mockResolvedValue(
      stateWith([
        [
          "mon",
          {
            phase: "blanked",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
            wake_attempts: 3,
            last_blank_failed: false,
          },
        ],
      ]),
    );

    render(
      <LiveStateProvider>
        <FailureBanner />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("failure-banner")).toBeInTheDocument();
    });
    expect(screen.getByText("mon")).toBeInTheDocument();
    expect(screen.getByText("wake failing ×3")).toBeInTheDocument();
  });

  it("renders a blank-failed display with the correct kind label", async () => {
    mocks.getState.mockResolvedValue(
      stateWith([
        [
          "tv",
          {
            phase: "active",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
            wake_attempts: 0,
            last_blank_failed: true,
          },
        ],
      ]),
    );

    render(
      <LiveStateProvider>
        <FailureBanner />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByText("tv")).toBeInTheDocument();
    });
    expect(screen.getByText("last blank failed")).toBeInTheDocument();
    // Not also labelled as wake-failing.
    expect(screen.queryByText(/wake failing/)).toBeNull();
  });

  it("renders both displays when more than one is failing", async () => {
    mocks.getState.mockResolvedValue(
      stateWith([
        [
          "mon",
          {
            phase: "blanked",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
            wake_attempts: 2,
            last_blank_failed: false,
          },
        ],
        [
          "tv",
          {
            phase: "active",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
            wake_attempts: 0,
            last_blank_failed: true,
          },
        ],
      ]),
    );

    render(
      <LiveStateProvider>
        <FailureBanner />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("failure-row-mon")).toBeInTheDocument();
    });
    expect(screen.getByTestId("failure-row-tv")).toBeInTheDocument();
  });

  it("renders nothing for a healthy display (wake_attempts=0, last_blank_failed=false)", async () => {
    mocks.getState.mockResolvedValue(
      stateWith([
        [
          "ok",
          {
            phase: "active",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
            wake_attempts: 0,
            last_blank_failed: false,
          },
        ],
      ]),
    );

    const { container } = render(
      <LiveStateProvider>
        <LoadedMarker />
        <FailureBanner />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("loaded")).toHaveTextContent("yes");
    });
    expect(container.querySelector("[data-testid='failure-banner']")).toBeNull();
  });

  it("renders nothing when wake_attempts/last_blank_failed are undefined (legacy snapshot)", async () => {
    mocks.getState.mockResolvedValue(
      stateWith([
        [
          "legacy",
          {
            phase: "blanked",
            inhibited: false,
            paused: false,
            cmd_gen: 1,
            controllers: [],
          },
        ],
      ]),
    );

    const { container } = render(
      <LiveStateProvider>
        <LoadedMarker />
        <FailureBanner />
      </LiveStateProvider>,
    );

    await waitFor(() => {
      expect(screen.getByTestId("loaded")).toHaveTextContent("yes");
    });
    expect(container.querySelector("[data-testid='failure-banner']")).toBeNull();
  });
});
