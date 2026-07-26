/**
 * Switching view — shared-display KVM ownership surface.
 *
 * One block per display in `kvm.switch_capable_displays`. Assembles:
 * - OwnershipPair (W2-1) — the ownership verdict and codes
 * - SwitchState (W2-2) — pull/push state machine
 * - How it switches rows — deep-linked to Config › Switching fields
 * - Hooks panel (W1-4) — read-only hook slot inspector
 *
 * Route: #/switching. Redirects to #/displays when switching is not
 * available (kvm == null || switch_capable_displays.length == 0).
 *
 * Source: views/switching.md
 */
import { useLiveState } from "../hooks/useLiveState";
import OwnershipPair from "./OwnershipPair";
import SwitchState from "./SwitchState";
import HooksInspector from "../config/HooksInspector";

export default function Switching() {
  const { snapshot, config, displayConfigs } = useLiveState();
  const kvm = snapshot?.kvm;
  const switchable = kvm?.switch_capable_displays ?? [];
  const coordination = config?.inventory?.coordination;

  // Redirect guard: if switching is not available, redirect to displays.
  if (!kvm || switchable.length === 0) {
    window.location.hash = "#/displays";
    return null;
  }

  return (
    <div className="switching-view">
      {switchable.map((displayId) => {
        const snap = snapshot?.displays?.find(([id]) => id === displayId)?.[1];
        const dc = displayConfigs[displayId];
        if (!snap) return null;

        const localWrite = dc?.shared_input_write_code ?? dc?.shared_input_code;
        const peerWrite = dc?.shared_peer_input_write_code;

        return (
          <div key={displayId} className="switching-block">
            {/* Ownership pair — full size */}
            <OwnershipPair
              displayId={displayId}
              snap={snap}
              config={dc}
              coordination={coordination}
              kvm={kvm}
              size="full"
            />

            {/* Pull/push controls */}
            <SwitchState
              displayId={displayId}
              switchCapable={kvm.switch_capable_displays.includes(displayId)}
              pushCapable={kvm.push_capable_displays.includes(displayId)}
              localWriteCode={localWrite}
              peerWriteCode={peerWrite}
              owned={snap.owned}
              observedInputCode={snap.observed_input_code}
            />

            {/* How it switches */}
            <div className="switching-how">
              <h3 className="switching-section-title">HOW IT SWITCHES</h3>

              {/* Activity follow */}
              <div className="switching-row">
                <span className="switching-row__label">Activity follow</span>
                <span className={`switching-row__value${kvm.activity_following ? " switching-row__value--on" : ""}`}>
                  {kvm.activity_following ? "● on" : "○ off"}
                </span>
                {coordination && (
                  <span className="switching-row__detail">
                    arm {coordination.arm_after ?? "7s"} · cooldown {coordination.cooldown ?? "3s"}
                  </span>
                )}
                <a className="switching-row__edit" href="#/config/switching">edit</a>
              </div>

              {/* Claim hotkey */}
              <div className="switching-row">
                <span className="switching-row__label">Claim hotkey</span>
                <span className="switching-row__value">
                  {kvm.keymap?.claim_hotkey ? (
                    <kbd>{kvm.keymap.claim_hotkey}</kbd>
                  ) : (
                    "not bound"
                  )}
                </span>
                <a className="switching-row__edit" href="#/config/switching">edit</a>
              </div>

              {/* Ownership poll */}
              <div className="switching-row">
                <span className="switching-row__label">Ownership poll</span>
                <span className="switching-row__value">
                  every {coordination?.poll_interval ?? "2s"} · {coordination?.loss_confirmations ?? 3} agreeing reads to flip
                </span>
                <a className="switching-row__edit" href="#/config/switching">edit</a>
              </div>

              {/* Panel state poll */}
              <div className="switching-row">
                <span className="switching-row__label">Panel state poll</span>
                <span className="switching-row__value">
                  every {coordination?.state_poll_interval ?? `max(30s, ${coordination?.poll_interval ?? "2s"})`}
                  {(coordination?.state_poll_interval == null) && " (default: max(30s, poll_interval))"}
                </span>
                <a className="switching-row__edit" href="#/config/switching">edit</a>
              </div>

              {/* Ignored devices */}
              <div className="switching-row">
                <span className="switching-row__label">Ignored devices</span>
                <span className="switching-row__value">
                  {config?.inventory?.input_filter?.ignore_devices?.length
                    ? `${config.inventory.input_filter.ignore_devices.length} globs`
                    : "none"}
                </span>
                <a className="switching-row__edit" href="#/config/switching">edit</a>
              </div>
            </div>

            {/* Hooks inspector (W1-4 read-only) */}
            <HooksInspector hooks={dc?.hooks} displayId={displayId} />

            {/* Shared-but-not-switch-capable warning */}
            {dc?.scope === "shared" && !kvm.switch_capable_displays.includes(displayId) && (
              <div className="switching-warning">
                {displayId} is marked shared but is not switch-capable — no controller
                reported VCP 0x60 write support. Run{" "}
                <a href="#/doctor">doctor ddcci</a>.
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
