import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, cleanup, act } from "@testing-library/react";
import Events from "../app/views/Events";


const { mockUseEventsImpl } = vi.hoisted(() => {
  let capturedOnMessage: ((data: unknown) => void) | null = null;
  let capturedOnConnect: (() => void) | null = null;
  let capturedOnDisconnect: (() => void) | null = null;

  const impl = vi.fn(
    (opts: {
      onMessage: (data: unknown) => void;
      onConnect?: () => void;
      onDisconnect?: () => void;
    }) => {
      capturedOnMessage = opts.onMessage;
      capturedOnConnect = opts.onConnect ?? null;
      capturedOnDisconnect = opts.onDisconnect ?? null;
      // connected is consumed directly from the return value; no need
      // to fire onConnect synchronously (avoids React render-in-render warning).
      return { connected: true, close: vi.fn() };
    },
  );

  return {
    mockUseEventsImpl: {
      impl,
      get onMessage() { return capturedOnMessage; },
      get onConnect() { return capturedOnConnect; },
      get onDisconnect() { return capturedOnDisconnect; },
      reset() { capturedOnMessage = null; capturedOnConnect = null; capturedOnDisconnect = null; },
    },
  };
});

vi.mock("../api/ws", () => ({
  useEvents: mockUseEventsImpl.impl,
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  mockUseEventsImpl.reset();
});

describe("Events", () => {
  it("renders empty state when no events have arrived", async () => {
    render(<Events />);

    await waitFor(() => {
      expect(screen.getByText("live · subscribed to daemon event stream")).toBeInTheDocument();
    });

    expect(screen.getByText("Waiting for events from the daemon…")).toBeInTheDocument();
    expect(screen.getByText("0 events")).toBeInTheDocument();
  });

  it("renders appending event rows after onMessage fires", async () => {
    render(<Events />);

    await waitFor(() => {
      expect(screen.getByText("live · subscribed to daemon event stream")).toBeInTheDocument();
    });

    act(() => {
      mockUseEventsImpl.onMessage?.({
        event: "sensor_changed", sensor: "desk-mmwave", state: "present",
      });
    });

    await waitFor(() => {
      expect(screen.getByText("desk-mmwave → present")).toBeInTheDocument();
    });

    expect(screen.getByText("sensor")).toBeInTheDocument();
    expect(screen.getByText("1 events")).toBeInTheDocument();
  });

  it("renders multiple event types with correct badges", async () => {
    render(<Events />);

    await waitFor(() => {
      expect(screen.getByText("live · subscribed to daemon event stream")).toBeInTheDocument();
    });

    act(() => {
      mockUseEventsImpl.onMessage?.({ event: "zone_changed", zone: "office", present: true, cause: "radar" });
      mockUseEventsImpl.onMessage?.({ event: "config_reloaded" });
      mockUseEventsImpl.onMessage?.({ event: "wake_retry", display: "aoc-main", attempt: 2 });
    });

    expect(screen.getByText("zone 'office' → occupied (cause: radar)")).toBeInTheDocument();
    expect(screen.getByText("config reloaded")).toBeInTheDocument();
    expect(screen.getByText("aoc-main: wake retry attempt 2")).toBeInTheDocument();

    expect(screen.getByText("zone")).toBeInTheDocument();
    expect(screen.getByText("config")).toBeInTheDocument();
    expect(screen.getByText("retry")).toBeInTheDocument();
  });

  it("shows lagged banner when stream_lagged event arrives", async () => {
    render(<Events />);

    await waitFor(() => {
      expect(screen.getByText("live · subscribed to daemon event stream")).toBeInTheDocument();
    });

    act(() => {
      mockUseEventsImpl.onMessage?.({ event: "stream_lagged" });
    });

    await waitFor(() => {
      expect(screen.getByText("stream lagged — catching up")).toBeInTheDocument();
    });

    expect(screen.getByText("0 events")).toBeInTheDocument();
  });

  it("shows event count", async () => {
    render(<Events />);

    await waitFor(() => {
      expect(screen.getByText("live · subscribed to daemon event stream")).toBeInTheDocument();
    });

    act(() => {
      mockUseEventsImpl.onMessage?.({ event: "sensor_changed", sensor: "a", state: "present" });
      mockUseEventsImpl.onMessage?.({ event: "zone_changed", zone: "o", present: true, cause: "x" });
    });

    expect(screen.getByText("2 events")).toBeInTheDocument();
  });
});
