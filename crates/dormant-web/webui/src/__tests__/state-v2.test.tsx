import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { LiveStateProvider } from "../app/state";
import { useLiveState } from "../app/hooks/useLiveState";

const { api, fixtures } = vi.hoisted(() => ({
  api: {
    getState: vi.fn(),
    getConfig: vi.fn(),
    getOperations: vi.fn(),
    getWear: vi.fn(),
    getWearDetail: vi.fn(),
  },
  fixtures: {
    state: { sensors: [], zones: [], displays: [], pending_reload: null },
    operations: { exercise_in_flight: [] as string[], emergency_wake_in_flight: false },
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
        displays: {},
        rules: {},
      },
      validation: { ok: true, warnings: [], errors: [] },
      display_rules: {},
      fingerprint: "abc",
      redacted_paths: [],
    },
    wear: {
      displays: [{
        display: "panel-main",
        display_name: "main",
        panel_type: "woled",
        total_on_hours: 12.5,
        sample_count: 15,
        advisory: false,
        hours_since_long_dwell: 8,
      }],
    },
    detail: {
      display: "panel-main",
      display_name: "main",
      panel_type: "woled",
      total_on_hours: 12.5,
      sample_count: 15,
      advisory: false,
      hours_since_long_dwell: 8,
      grid_rows: 9,
      grid_cols: 16,
      cells: Array(144).fill(0),
      heat: Array(144).fill(0),
    },
  },
}));
const ws = vi.hoisted(() => ({ onMessage: null as null | ((event: unknown) => void) }));

vi.mock("../api/client", () => ({
  ...api,
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
vi.mock("../api/ws", () => ({
  useEvents: vi.fn((opts: { onMessage: (event: unknown) => void }) => {
    ws.onMessage = opts.onMessage;
    return { connected: true, close: vi.fn() };
  }),
}));

function Consumer() {
  const state = useLiveState();
  const [startedOperationsRequest, setStartedOperationsRequest] = useState(0);
  return (
    <div>
      <span data-testid="state-calls">{state.snapshot ? "loaded" : "loading"}</span>
      <span data-testid="wear-count">{state.wear?.displays.length ?? 0}</span>
      <span data-testid="detail-count">{Object.keys(state.wearDetails).length}</span>
      <span data-testid="fatal-error">{state.error ?? "none"}</span>
      <span data-testid="poll-warning">{state.pollWarning ?? "none"}</span>
      <span data-testid="config-path">{state.config?.path ?? "none"}</span>
      <span data-testid="operations">
        {state.operations
          ? `${state.operations.emergency_wake_in_flight}:${state.operations.exercise_in_flight.join(",")}`
          : "loading"}
      </span>
      <span data-testid="operations-request-id">{state.operationsRequestId}</span>
      <span data-testid="started-operations-request">{startedOperationsRequest}</span>
      <span data-testid="detail-ids">{Object.keys(state.wearDetails).sort().join(",")}</span>
      <span data-testid="detail-storage">
        {Object.values(state.wearDetails).map((detail) => detail.display).sort().join(",")}
      </span>
      <button type="button" onClick={() => state.selectDisplay("main")}>select</button>
      <button type="button" onClick={() => void state.refreshWear()}>refresh wear</button>
      <button
        type="button"
        onClick={() => void state.refreshOperations(setStartedOperationsRequest)}
      >
        refresh operations
      </button>
      <span data-testid="selected">{state.selectedDisplay ?? "none"}</span>
    </div>
  );
}

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.clearAllMocks();
});

beforeEach(() => {
  api.getOperations.mockResolvedValue(fixtures.operations);
});

describe("LiveStateProvider v2", () => {
  it("loads state, config, wear list, and wear detail", async () => {
    api.getState.mockResolvedValue(fixtures.state);
    api.getConfig.mockResolvedValue(fixtures.config);
    api.getWear.mockResolvedValue(fixtures.wear);
    api.getWearDetail.mockResolvedValue(fixtures.detail);

    render(<LiveStateProvider><Consumer /></LiveStateProvider>);

    await waitFor(() => expect(screen.getByTestId("state-calls")).toHaveTextContent("loaded"));
    expect(screen.getByTestId("wear-count")).toHaveTextContent("1");
    expect(screen.getByTestId("detail-count")).toHaveTextContent("1");
    fireEvent.click(screen.getByRole("button", { name: "select" }));
    expect(screen.getByTestId("selected")).toHaveTextContent("main");
  });

  it("polls state and authoritative operation guards roughly once per second", async () => {
    vi.useFakeTimers();
    api.getState.mockResolvedValue(fixtures.state);
    api.getConfig.mockResolvedValue(fixtures.config);
    api.getWear.mockResolvedValue({ displays: [] });

    render(<LiveStateProvider><Consumer /></LiveStateProvider>);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(api.getState).toHaveBeenCalledTimes(1);
    expect(api.getOperations).toHaveBeenCalledTimes(1);

    await act(async () => {
      vi.advanceTimersByTime(1000);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(api.getState).toHaveBeenCalledTimes(2);
    expect(api.getOperations).toHaveBeenCalledTimes(2);
    expect(api.getConfig).toHaveBeenCalledTimes(1);
    expect(api.getWear).toHaveBeenCalledTimes(1);
  });

  it("shows a transient poll warning and clears it on the next successful poll", async () => {
    vi.useFakeTimers();
    api.getState
      .mockResolvedValueOnce(fixtures.state)
      .mockRejectedValueOnce(new Error("temporary disconnect"))
      .mockResolvedValueOnce({ ...fixtures.state, pending_reload: "recovered" });
    api.getConfig.mockResolvedValue(fixtures.config);
    api.getWear.mockResolvedValue({ displays: [] });
    render(<LiveStateProvider><Consumer /></LiveStateProvider>);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });

    await act(async () => {
      vi.advanceTimersByTime(1000);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(screen.getByTestId("fatal-error")).toHaveTextContent("none");
    expect(screen.getByTestId("poll-warning")).toHaveTextContent("temporary disconnect");

    await act(async () => {
      vi.advanceTimersByTime(1000);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(screen.getByTestId("poll-warning")).toHaveTextContent("none");
  });

  it("loads operation guards on mount and refreshes them with the status poll", async () => {
    vi.useFakeTimers();
    api.getState.mockResolvedValue(fixtures.state);
    api.getConfig.mockResolvedValue(fixtures.config);
    api.getWear.mockResolvedValue({ displays: [] });
    api.getOperations
      .mockResolvedValueOnce(fixtures.operations)
      .mockResolvedValueOnce({
        exercise_in_flight: ["main"],
        emergency_wake_in_flight: true,
      });

    render(<LiveStateProvider><Consumer /></LiveStateProvider>);
    await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    expect(screen.getByTestId("operations")).toHaveTextContent("false:");
    expect(screen.getByTestId("operations-request-id")).toHaveTextContent("1");

    await act(async () => {
      vi.advanceTimersByTime(1000);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(screen.getByTestId("operations")).toHaveTextContent("true:main");
    expect(screen.getByTestId("operations-request-id")).toHaveTextContent("2");
  });

  it("assigns request ids at start and refuses an older delayed operations commit", async () => {
    vi.useFakeTimers();
    function deferred<T>() {
      let resolve!: (value: T) => void;
      const promise = new Promise<T>((done) => { resolve = done; });
      return { promise, resolve };
    }
    api.getState.mockResolvedValue(fixtures.state);
    api.getConfig.mockResolvedValue(fixtures.config);
    api.getWear.mockResolvedValue({ displays: [] });
    render(<LiveStateProvider><Consumer /></LiveStateProvider>);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(screen.getByTestId("operations-request-id")).toHaveTextContent("1");

    const older = deferred<typeof fixtures.operations>();
    const newer = deferred<typeof fixtures.operations>();
    api.getOperations
      .mockImplementationOnce(() => older.promise)
      .mockImplementationOnce(() => newer.promise);
    fireEvent.click(screen.getByRole("button", { name: "refresh operations" }));
    fireEvent.click(screen.getByRole("button", { name: "refresh operations" }));
    expect(screen.getByTestId("started-operations-request")).toHaveTextContent("3");

    newer.resolve({ exercise_in_flight: ["main"], emergency_wake_in_flight: true });
    await act(async () => { await newer.promise; await Promise.resolve(); });
    expect(screen.getByTestId("operations-request-id")).toHaveTextContent("3");
    older.resolve(fixtures.operations);
    await act(async () => { await older.promise; await Promise.resolve(); });
    expect(screen.getByTestId("operations-request-id")).toHaveTextContent("3");
    expect(screen.getByTestId("operations")).toHaveTextContent("true:main");
  });

  it("keeps existing data when a config-reload background refresh fails and clears its warning after recovery", async () => {
    vi.useFakeTimers();
    const flush = async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    };
    api.getState.mockResolvedValue(fixtures.state);
    api.getConfig
      .mockResolvedValueOnce(fixtures.config)
      .mockRejectedValueOnce(new Error("reload refresh failed"))
      .mockResolvedValueOnce({ ...fixtures.config, path: "/tmp/recovered.toml" });
    api.getWear.mockResolvedValue({ displays: [] });

    render(<LiveStateProvider><Consumer /></LiveStateProvider>);
    await act(flush);
    expect(screen.getByTestId("config-path")).toHaveTextContent("/tmp/config.toml");

    act(() => ws.onMessage?.({ event: "config_reloaded" }));
    await act(flush);
    expect(screen.getByTestId("poll-warning")).toHaveTextContent("reload refresh failed");
    expect(screen.getByTestId("fatal-error")).toHaveTextContent("none");
    expect(screen.getByTestId("config-path")).toHaveTextContent("/tmp/config.toml");

    // A successful background state+operations poll must NOT erase an
    // active config-reload background-refresh warning: only a fresh
    // config_reloaded refresh (success or failure) may touch it.
    await act(async () => {
      vi.advanceTimersByTime(1000);
      await flush();
    });
    expect(screen.getByTestId("poll-warning")).toHaveTextContent("reload refresh failed");
    expect(screen.getByTestId("fatal-error")).toHaveTextContent("none");

    act(() => ws.onMessage?.({ event: "config_reloaded" }));
    await act(flush);
    expect(screen.getByTestId("config-path")).toHaveTextContent("/tmp/recovered.toml");
    expect(screen.getByTestId("poll-warning")).toHaveTextContent("none");
  });

  it("refreshes and invalidates wear details on config reload", async () => {
    api.getState.mockResolvedValue(fixtures.state);
    api.getConfig.mockResolvedValue(fixtures.config);
    api.getWear
      .mockResolvedValueOnce({
        displays: [
          { ...fixtures.wear.displays[0], display: "panel-main-v1", display_name: "main" },
          { ...fixtures.wear.displays[0], display: "panel-old", display_name: "old" },
        ],
      })
      .mockResolvedValueOnce({
        displays: [
          { ...fixtures.wear.displays[0], display: "panel-main-v2", display_name: "main" },
          { ...fixtures.wear.displays[0], display: "panel-new", display_name: "new" },
        ],
      });
    api.getWearDetail.mockImplementation(async (display: string) => ({
      ...fixtures.detail,
      display,
      display_name: display.includes("main") ? "main" : display.replace("panel-", ""),
    }));

    render(<LiveStateProvider><Consumer /></LiveStateProvider>);
    await waitFor(() => expect(screen.getByTestId("detail-ids").textContent).toBe("main,old"));
    act(() => ws.onMessage?.({ event: "config_reloaded" }));
    await waitFor(() => expect(screen.getByTestId("detail-ids").textContent).toBe("main,new"));
    // Exact match, not substring: a merge (instead of replace) would leave
    // "main,new,old" / "panel-main-v2,panel-new,panel-old" here, both of
    // which CONTAIN the strings a toHaveTextContent check would accept.
    expect(screen.getByTestId("detail-storage").textContent).toBe("panel-main-v2,panel-new");
    expect(screen.getByTestId("detail-storage")).not.toHaveTextContent("panel-main-v1");
    expect(api.getConfig).toHaveBeenCalledTimes(2);
  });

  it("ignores an older wear refresh that completes after a newer one", async () => {
    function deferred<T>() {
      let resolve!: (value: T) => void;
      const promise = new Promise<T>((done) => { resolve = done; });
      return { promise, resolve };
    }
    const older = deferred<typeof fixtures.wear>();
    const newer = deferred<typeof fixtures.wear>();
    api.getState.mockResolvedValue(fixtures.state);
    api.getConfig.mockResolvedValue(fixtures.config);
    api.getWear
      .mockImplementationOnce(() => older.promise)
      .mockImplementationOnce(() => newer.promise);
    api.getWearDetail.mockImplementation(async (display: string) => ({
      ...fixtures.detail,
      display,
      display_name: "main",
    }));

    render(<LiveStateProvider><Consumer /></LiveStateProvider>);
    fireEvent.click(screen.getByRole("button", { name: "refresh wear" }));
    newer.resolve({
      displays: [{ ...fixtures.wear.displays[0], display: "panel-new", display_name: "main" }],
    });
    await waitFor(() => expect(screen.getByTestId("detail-storage").textContent).toBe("panel-new"));
    older.resolve({
      displays: [{ ...fixtures.wear.displays[0], display: "panel-old", display_name: "main" }],
    });
    await act(async () => { await older.promise; await Promise.resolve(); });
    expect(screen.getByTestId("detail-storage").textContent).toBe("panel-new");
    expect(screen.getByTestId("detail-storage")).not.toHaveTextContent("panel-old");
  });
});
