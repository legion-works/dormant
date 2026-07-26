/**
 * SwitchState tests — pull/push state machine.
 *
 * Acceptance criteria from flows/kvm-pull-push.md:
 * - six pull states render
 * - five push states render
 * - push absent with a reason when not push-capable
 * - error copy always names written code and read-back code
 * - second pull never blocked by peer-owned verdict
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor, cleanup } from "@testing-library/react";
import SwitchState from "../app/views/SwitchState";
import { LiveStateContext, EventLogContext } from "../app/hooks/useLiveState";
import { liveStateFixture, eventLogFixture } from "./fixtures/live-state";

afterEach(() => cleanup());

const mocks = vi.hoisted(() => ({
  postSwitch: vi.fn().mockResolvedValue(undefined),
  postPush: vi.fn().mockResolvedValue(undefined),
}));
vi.mock("../api/client", () => ({
  postSwitch: mocks.postSwitch,
  postPush: mocks.postPush,
}));

function renderSwitch(
  props: Partial<Parameters<typeof SwitchState>[0]> = {},
  events: any[] = [],
) {
  const live = liveStateFixture({
    snapshot: {
      sensors: [],
      zones: [],
      displays: [["test", {
        phase: "active", inhibited: false, paused: false, cmd_gen: 1,
        controllers: [], scope: "shared", owned: props.owned ?? true,
        observed_input_code: props.observedInputCode ?? 0x0f,
      }]],
      pending_reload: null,
    },
  });
  return render(
    <LiveStateContext.Provider value={live}>
      <EventLogContext.Provider
        value={eventLogFixture({ events, connected: true, lagged: false })}
      >
        <SwitchState
          displayId="test"
          switchCapable={true}
          pushCapable={false}
          localWriteCode={0x15}
          peerWriteCode={undefined}
          owned={true}
          observedInputCode={0x0f}
          {...props}
        />
      </EventLogContext.Provider>
    </LiveStateContext.Provider>,
  );
}

describe("SwitchState", () => {
  it("renders pull button when switch-capable", () => {
    renderSwitch();
    expect(screen.getByText("◀ Pull here")).toBeInTheDocument();
  });

  it("hides pull section when not switch-capable", () => {
    renderSwitch({ switchCapable: false });
    // The pull button text must not be in the document.
    expect(screen.queryByText("◀ Pull here")).not.toBeInTheDocument();
  });

  it("renders push button when push-capable", () => {
    renderSwitch({ pushCapable: true, peerWriteCode: 0x11 });
    expect(screen.getByText("▶ Push to peer")).toBeInTheDocument();
  });

  it("renders absent push with reason when not push-capable", () => {
    renderSwitch({ pushCapable: false, peerWriteCode: undefined });
    // The absent message renders ONCE (only inside renderPush).
    const msgs = screen.getAllByText(/no shared_peer_input_write_code/);
    expect(msgs.length).toBe(1);
    expect(screen.getByText("configure")).toBeInTheDocument();
  });

  it("shows writing state with hex code during pull", async () => {
    mocks.postSwitch.mockImplementationOnce(() => new Promise(() => {}));
    renderSwitch();
    fireEvent.click(screen.getByText("◀ Pull here"));
    await waitFor(() => {
      expect(screen.getByText(/◌ writing 0x15/)).toBeInTheDocument();
    });
  });

  it("shows failed state with error when pull fails", async () => {
    mocks.postSwitch.mockRejectedValueOnce(new Error("fetch failed"));
    renderSwitch();
    fireEvent.click(screen.getByText("◀ Pull here"));
    await waitFor(() => {
      const els = screen.getAllByText(/the daemon did not answer/);
      expect(els.length).toBeGreaterThanOrEqual(1);
    });
  });

  it("shows unverified copy naming written code", async () => {
    // Inject a BG-1 ownership event with verified:false.
    renderSwitch(
      { localWriteCode: 0x15, observedInputCode: 0x10 },
      [{
        time: "12:00:00",
        event: {
          event: "ownership",
          display: "test",
          owned: true,
          written_code: 0x15,
          cause: "pull",
          verified: false,
          degraded: false,
        },
      }],
    );
    await waitFor(() => {
      expect(screen.getByText(/⚠ wrote, not confirmed/)).toBeInTheDocument();
    });
    expect(screen.getByText(/wrote 0x15.*panel reports 0x10/)).toBeInTheDocument();
  });

  it("shows pull button even when peer-owned", () => {
    renderSwitch({ owned: false });
    // Pull must be rendered regardless of ownership verdict.
    expect(screen.getByText("◀ Pull here")).toBeInTheDocument();
  });

  it("shows push released on success", async () => {
    mocks.postPush.mockResolvedValueOnce(undefined);
    renderSwitch({ pushCapable: true, peerWriteCode: 0x11 });
    fireEvent.click(screen.getByText("▶ Push to peer"));
    await waitFor(() => {
      expect(screen.getByText("✓ sent")).toBeInTheDocument();
    });
  });

  it("shows push ignored state from BG-1 event (verified:false)", async () => {
    renderSwitch(
      { switchCapable: true, pushCapable: true, peerWriteCode: 0x11 },
      [{
        time: "12:00:00",
        event: {
          event: "ownership",
          display: "test",
          owned: false,
          written_code: 0x11,
          cause: "push",
          verified: false,
          degraded: false,
        },
      }],
    );
    await waitFor(() => {
      expect(screen.getByText(/⚠ peer did not take it/)).toBeInTheDocument();
    });
  });

  it("shows push degraded state from BG-1 event", async () => {
    renderSwitch(
      { switchCapable: true, pushCapable: true, peerWriteCode: 0x11 },
      [{
        time: "12:00:00",
        event: {
          event: "ownership",
          display: "test",
          owned: false,
          written_code: 0x11,
          cause: "push",
          verified: true,
          degraded: true,
        },
      }],
    );
    await waitFor(() => {
      expect(screen.getByText(/✓ sent · unverified/)).toBeInTheDocument();
    });
  });
});
