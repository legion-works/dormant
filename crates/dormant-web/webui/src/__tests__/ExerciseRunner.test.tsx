import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import ExerciseRunner, { aggregateExerciseVerdict } from "../app/components/ExerciseRunner";
import type { OperationsStatus } from "../api/types";

const api = vi.hoisted(() => ({
  postExercise: vi.fn().mockResolvedValue({
    display: "main",
    pre_phase: "active",
    paused_rules: ["office_blank"],
    steps: [
      {
        command: "blank",
        blank_mode: "power_off",
        returned_ok: true,
        state_before: { power: "on" },
        state_after: { power: "off" },
        verdict: "confirmed",
      },
      {
        command: "wake",
        returned_ok: true,
        state_before: { power: "off" },
        state_after: { power: "on" },
        verdict: "confirmed",
      },
    ],
  }),
  operations: {
    exercise_in_flight: [],
    emergency_wake_in_flight: false,
  } as OperationsStatus,
  operationsRequestId: 1,
  refreshOperations: vi.fn(),
}));
vi.mock("../api/client", () => ({
  // Adaptation: the plan's draft uses TS parameter-property shorthand
  // (`constructor(public status: …)`). This repo's tsconfig sets
  // `erasableSyntaxOnly: true`, which rejects parameter properties —
  // mirrors the explicit-field pattern already used by
  // EmergencyWakeControl.test.tsx's ApiError mock (T5 precedent).
  ApiError: class ApiError extends Error {
    status: number;
    body: unknown;
    constructor(status: number, body: unknown) {
      super(`API ${status}`);
      this.status = status;
      this.body = body;
    }
  },
  postExercise: api.postExercise,
}));
vi.mock("../app/hooks/useLiveState", () => ({
  useLiveState: () => ({
    operations: api.operations,
    operationsRequestId: api.operationsRequestId,
    refreshOperations: api.refreshOperations,
  }),
}));

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.clearAllMocks();
  api.refreshOperations.mockReset();
  api.operations = {
    exercise_in_flight: [],
    emergency_wake_in_flight: false,
  };
  api.operationsRequestId = 1;
});

describe("ExerciseRunner", () => {
  it("derives the aggregate verdict with failed taking precedence", () => {
    expect(aggregateExerciseVerdict([])).toBe("not_run");
    expect(aggregateExerciseVerdict([
      { command: "read", returned_ok: false, verdict: "unconfirmable" },
      { command: "wake", returned_ok: true, verdict: "failed" },
    ])).toBe("failed");
  });

  it("confirms before running and renders every engine-owned step", async () => {
    render(<ExerciseRunner display="main" />);
    fireEvent.click(screen.getByRole("button", { name: "Run control-path exercise" }));
    expect(screen.getByRole("alertdialog", { name: "Exercise main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Run exercise" }));

    await waitFor(() => expect(api.postExercise).toHaveBeenCalledWith("main"));
    expect(screen.getAllByText("confirmed").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("blank")).toBeInTheDocument();
    expect(screen.getByText("wake")).toBeInTheDocument();
    expect(screen.getByText("office_blank")).toBeInTheDocument();
    expect(screen.getByText("on → off")).toBeInTheDocument();
  });

  it("keeps report-timeout state disabled until authoritative operation status releases", async () => {
    vi.useFakeTimers();
    function deferred<T>() {
      let resolve!: (value: T) => void;
      const promise = new Promise<T>((done) => { resolve = done; });
      return { promise, resolve };
    }
    const stalePreTimeout = deferred<OperationsStatus>();
    const dedicatedPostTimeout = deferred<OperationsStatus>();
    const staleCommit = stalePreTimeout.promise.then((status) => {
      api.operations = status;
      api.operationsRequestId = 2;
      return status;
    });
    api.refreshOperations.mockImplementation((onStart?: (requestId: number) => void) => {
      onStart?.(3);
      return dedicatedPostTimeout.promise.then((status) => {
        api.operations = status;
        api.operationsRequestId = 3;
        return status;
      });
    });
    let reject!: (error: unknown) => void;
    api.postExercise.mockReturnValueOnce(
      new Promise((_resolve, rejectPromise) => { reject = rejectPromise; }),
    );
    const { ApiError } = await import("../api/client");
    const view = render(<ExerciseRunner display="main" />);
    fireEvent.click(screen.getByRole("button", { name: "Run control-path exercise" }));
    fireEvent.click(screen.getByRole("button", { name: "Run exercise" }));

    // Adaptation (mirrors EmergencyWakeControl.test.tsx's T5 precedent):
    // `confirm()`'s promise resolves synchronously inside the "Run
    // exercise" button's onClick, but the `.then` continuation that sets
    // `uiState` to "pending" and starts the elapsed-ticker interval is
    // only scheduled as a microtask. Without this flush,
    // `vi.advanceTimersByTime(3000)` below runs synchronously — before
    // that continuation has had a chance to run — so no interval exists
    // yet to advance.
    await act(async () => {});
    await act(async () => { vi.advanceTimersByTime(3000); });
    expect(screen.getByRole("status")).toHaveTextContent(
      "Exercise in progress · 3s elapsed · display will blink",
    );
    expect(screen.getByRole("button", { name: "Run control-path exercise" })).toBeDisabled();
    expect(api.postExercise).toHaveBeenCalledTimes(1);

    await act(async () => {
      reject(new ApiError(504, { error: "exercise_report_timeout" }));
      await Promise.resolve();
    });
    view.rerender(<ExerciseRunner display="main" />);
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Report timed out. Exercise may still be running — do not retry.",
    );
    expect(api.refreshOperations).toHaveBeenCalledOnce();
    expect(screen.getByRole("button", { name: "Run control-path exercise" })).toBeDisabled();

    // Request 2 started before exercise. Its delayed false commit is stale.
    await act(async () => {
      stalePreTimeout.resolve({
        exercise_in_flight: [],
        emergency_wake_in_flight: false,
      });
      await staleCommit;
      view.rerender(<ExerciseRunner display="main" />);
    });
    expect(api.operationsRequestId).toBe(2);
    expect(screen.getByRole("button", { name: "Run control-path exercise" })).toBeDisabled();

    // Request 3 was started by the consumer after uncertainty began.
    await act(async () => {
      dedicatedPostTimeout.resolve({
        exercise_in_flight: [],
        emergency_wake_in_flight: false,
      });
      await Promise.resolve();
      await Promise.resolve();
      view.rerender(<ExerciseRunner display="main" />);
    });
    expect(api.operationsRequestId).toBe(3);
    expect(screen.getByRole("button", { name: "Run control-path exercise" })).toBeEnabled();
  });

  it("initializes disabled when the display exercise guard is already held", () => {
    api.operations = {
      exercise_in_flight: ["main"],
      emergency_wake_in_flight: false,
    };
    api.operationsRequestId = 7;
    render(<ExerciseRunner display="main" />);
    expect(screen.getByRole("button", { name: "Run control-path exercise" })).toBeDisabled();
    expect(screen.getByRole("status")).toHaveTextContent("Exercise already in progress for main");
  });

  it("does not post when the exercise confirmation is cancelled", async () => {
    // Same microtask-flush requirement as DisplayDetail's "Cancel" negative
    // test (T6/T7 precedent): `confirm()`'s promise resolves synchronously
    // inside the Cancel button's onClick, but the `.then` continuation that
    // would call `postExercise` is only scheduled as a microtask.
    const flush = () => act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    render(<ExerciseRunner display="main" />);
    fireEvent.click(screen.getByRole("button", { name: "Run control-path exercise" }));
    expect(screen.getByRole("alertdialog", { name: "Exercise main?" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await flush();
    expect(api.postExercise).not.toHaveBeenCalled();
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  });
});
