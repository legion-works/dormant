/**
 * EmergencyWakeControl — local single-flight + honest report-timeout
 * handling. Pins the freshness contract: the uncertain state clears
 * ONLY when `operationsRequestId >= startedAfter` (the id recorded
 * synchronously by `refreshOperations`'s `onStart`) AND
 * `!operations.emergency_wake_in_flight`. A delayed pre-timeout
 * observation with a lower request id must NOT release the UI, even if
 * it commits `emergency_wake_in_flight: false`.
 *
 * Adaptations (T5) from the plan's draft:
 *  - The draft mock class used a constructor with TS parameter-property
 *    shorthand (`constructor(public status: …)`). This repo's tsconfig
 *    sets `erasableSyntaxOnly: true`, which rejects parameter
 *    properties — mirrors the explicit-field pattern already used by
 *    `state-v2.test.tsx`'s `ApiError` mock.
 *  - The first scenario inserts an extra `await act(async () => {})`
 *    between the confirm click and the fake-timer advance. Without it,
 *    `vi.advanceTimersByTime(3000)` runs synchronously inside the outer
 *    `act(async () => …)` callback's own synchronous prefix — BEFORE
 *    the confirm-dialog promise's `.then` continuation (which sets
 *    `uiState` to "pending" and mounts the elapsed-ticker interval) has
 *    had a chance to run as a microtask. Empirically verified (a
 *    minimal Node.js repro of the same resolve-then-act ordering shows
 *    the same result): resolving a promise synchronously during a
 *    `fireEvent.click` does not run its `.then` continuation until the
 *    test yields to the microtask queue, which happens strictly after
 *    an `act(async () => { sync-work })` callback's synchronous body
 *    has already executed. The extra empty `act` flush is a
 *    synchronization step, not a behavioral relaxation — every
 *    assertion below is unchanged from the plan's draft.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import EmergencyWakeControl from "../app/components/EmergencyWakeControl";
import type { OperationsStatus } from "../api/types";

const mocks = vi.hoisted(() => ({
  postEmergencyWake: vi.fn(),
  operations: {
    exercise_in_flight: [],
    emergency_wake_in_flight: false,
  } as OperationsStatus,
  operationsRequestId: 1,
  refreshOperations: vi.fn(),
}));

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
}));
vi.mock("../app/hooks/useLiveState", () => ({
  useLiveState: () => ({
    operations: mocks.operations,
    operationsRequestId: mocks.operationsRequestId,
    refreshOperations: mocks.refreshOperations,
  }),
}));

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.clearAllMocks();
  mocks.refreshOperations.mockReset();
  mocks.operations = {
    exercise_in_flight: [],
    emergency_wake_in_flight: false,
  };
  mocks.operationsRequestId = 1;
});

describe("EmergencyWakeControl", () => {
  it("keeps a report timeout uncertain until authoritative operation status releases", async () => {
    vi.useFakeTimers();
    function deferred<T>() {
      let resolve!: (value: T) => void;
      const promise = new Promise<T>((done) => { resolve = done; });
      return { promise, resolve };
    }
    const stalePreTimeout = deferred<OperationsStatus>();
    const dedicatedPostTimeout = deferred<OperationsStatus>();
    const staleCommit = stalePreTimeout.promise.then((status) => {
      mocks.operations = status;
      mocks.operationsRequestId = 2;
      return status;
    });
    mocks.refreshOperations.mockImplementation((onStart?: (requestId: number) => void) => {
      onStart?.(3);
      return dedicatedPostTimeout.promise.then((status) => {
        mocks.operations = status;
        mocks.operationsRequestId = 3;
        return status;
      });
    });
    let reject!: (error: unknown) => void;
    mocks.postEmergencyWake.mockReturnValueOnce(
      new Promise((_resolve, rejectPromise) => { reject = rejectPromise; }),
    );
    const { ApiError } = await import("../api/client");
    const view = render(<EmergencyWakeControl />);

    fireEvent.click(screen.getByRole("button", { name: "Emergency wake" }));
    fireEvent.click(screen.getByRole("button", { name: "Wake every display" }));
    await act(async () => {});
    await act(async () => { vi.advanceTimersByTime(3000); });
    expect(screen.getByRole("status")).toHaveTextContent("Emergency wake in progress · 3s elapsed");
    expect(screen.getByRole("button", { name: "Emergency wake" })).toBeDisabled();

    await act(async () => {
      reject(new ApiError(504, { error: "emergency_wake_report_timeout" }));
      await Promise.resolve();
    });
    view.rerender(<EmergencyWakeControl />);
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Report timed out. Emergency wake may still be running — do not retry.",
    );
    expect(mocks.refreshOperations).toHaveBeenCalledOnce();
    expect(screen.getByRole("button", { name: "Emergency wake" })).toBeDisabled();

    // Request 2 started before the operation. Its delayed false commit is stale.
    await act(async () => {
      stalePreTimeout.resolve({
        exercise_in_flight: [],
        emergency_wake_in_flight: false,
      });
      await staleCommit;
      view.rerender(<EmergencyWakeControl />);
    });
    expect(mocks.operationsRequestId).toBe(2);
    expect(screen.getByRole("button", { name: "Emergency wake" })).toBeDisabled();

    // Request 3 was started by the consumer after uncertainty began.
    await act(async () => {
      dedicatedPostTimeout.resolve({
        exercise_in_flight: [],
        emergency_wake_in_flight: false,
      });
      await Promise.resolve();
      await Promise.resolve();
      view.rerender(<EmergencyWakeControl />);
    });
    expect(mocks.operationsRequestId).toBe(3);
    expect(screen.getByRole("button", { name: "Emergency wake" })).toBeEnabled();
  });

  it("initializes disabled from an in-flight operation after remount", () => {
    mocks.operations = {
      exercise_in_flight: [],
      emergency_wake_in_flight: true,
    };
    mocks.operationsRequestId = 7;
    render(<EmergencyWakeControl />);
    expect(screen.getByRole("button", { name: "Emergency wake" })).toBeDisabled();
    expect(screen.getByRole("status")).toHaveTextContent("Emergency wake already in progress");
  });

  it("does not post when confirmation is cancelled", () => {
    render(<EmergencyWakeControl />);
    fireEvent.click(screen.getByRole("button", { name: "Emergency wake" }));
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(mocks.postEmergencyWake).not.toHaveBeenCalled();
  });
});
