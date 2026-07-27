/**
 * Switching view — shared-display KVM ownership surface.
 *
 * One block per shared display, with full controls for switch-capable
 * displays and a warning card for shared-but-not-switch-capable ones.
 *
 * Assembles:
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
import { Card } from "../components";
import "./Switching.css";

export default function Switching() {
  const { snapshot, config, displayConfigs } = useLiveState();
  const kvm = snapshot?.kvm;
  const coordination = config?.inventory?.coordination;

  // Redirect guard: if switching is not available, redirect to displays.
  if (!kvm || (kvm.switch_capable_displays?.length ?? 0) === 0) {
    window.location.hash = "#/displays";
    return null;
  }

  // Collect shared displays for the warning section.
  const allShared = Object.entries(displayConfigs)
    .filter(([, dc]) => dc.scope === "shared")
    .map(([id]) => id);
  const notSwitchable = allShared.filter(
    (id) => !kvm.switch_capable_displays.includes(id),
  );

  return (
    <div className="switching-view">
      {/* Shared-but-not-switch-capable warnings */}
      {notSwitchable.length > 0 && (
        <Card className="switching-warning">
          {notSwitchable.map((id) => (
            <div key={id}>
              {id} is marked shared but is not switch-capable — no controller
              reported VCP 0x60 write support. Run{" "}
              <a href="#/doctor">doctor ddcci</a>.
            </div>
          ))}
        </Card>
      )}

      {kvm.switch_capable_displays.map((displayId) => {
        const snap = snapshot?.displays?.find(([id]) => id === displayId)?.[1];
        const dc = displayConfigs[displayId];
        if (!snap) return null;

        const localWrite = dc?.shared_input_write_code ?? dc?.shared_input_code;
        const peerWrite = dc?.shared_peer_input_write_code;

        return (
          <div key={displayId} className="switching-block">
            {/* Ownership panel — full width */}
            <OwnershipPair
              displayId={displayId}
              snap={snap}
              config={dc}
              coordination={coordination}
              size="full"
            />

            {/* Pull/push controls — action bar below ownership */}
            <SwitchState
              displayId={displayId}
              switchCapable={kvm.switch_capable_displays.includes(displayId)}
              pushCapable={kvm.push_capable_displays.includes(displayId)}
              localWriteCode={localWrite}
              peerWriteCode={peerWrite}
              owned={snap.owned}
              observedInputCode={snap.observed_input_code}
              coordination={coordination}
            />

            {/* Two-column grid: How it switches (left) + Hooks (right) — screens/02 */}
            <div className="switching-two-col">
              {/* How it switches — deep-linked to config fields */}
              <div className="switching-how">
                <h3 className="switching-section-title">How it switches</h3>

                <div className="switching-row">
                  <div className="switching-row__left">
                    <span className="switching-row__label">Activity follow</span>
                    <span className="switching-row__detail">
                      arm {coordination?.arm_after ?? "7s"} · cooldown {coordination?.cooldown ?? "3s"}
                    </span>
                  </div>
                  <span className={`switching-row__value${kvm.activity_following ? " switching-row__value--on" : ""}`}>
                    {kvm.activity_following ? (
                      <><span className="switching-row__dot switching-row__dot--on" />on</>
                    ) : (
                      "off"
                    )}
                  </span>
                  <a className="switching-row__edit" href="#/config/switching#coordination.activity_follow">edit</a>
                </div>

                <div className="switching-row">
                  <div className="switching-row__left">
                    <span className="switching-row__label">Claim hotkey</span>
                    <span className="switching-row__detail">
                      registered by the tray
                    </span>
                  </div>
                  <span className="switching-row__value">
                    {kvm.keymap?.claim_hotkey ? (
                      <kbd>{kvm.keymap.claim_hotkey}</kbd>
                    ) : (
                      "not bound"
                    )}
                  </span>
                  <a className="switching-row__edit" href="#/config/switching#keymap.claim_hotkey">edit</a>
                </div>

                <div className="switching-row">
                  <div className="switching-row__left">
                    <span className="switching-row__label">Ownership poll</span>
                    <span className="switching-row__detail">
                      every {coordination?.poll_interval ?? "2s"} · {coordination?.loss_confirmations ?? 3} agreeing reads to flip
                    </span>
                  </div>
                  <span className="switching-row__detail switching-row__extra">
                    ~{(coordination?.poll_interval ? parseInt(coordination.poll_interval, 10) * (coordination?.loss_confirmations ?? 3) : 6)}s handoff
                  </span>
                  <a className="switching-row__edit" href="#/config/switching#coordination.poll_interval">edit</a>
                </div>

                <div className="switching-row">
                  <div className="switching-row__left">
                    <span className="switching-row__label">Ignored devices</span>
                    <span className="switching-row__detail">
                      {config?.inventory?.input_filter?.ignore_devices?.length
                        ? `${config.inventory.input_filter.ignore_devices.length} globs`
                        : "none"}
                    </span>
                  </div>
                  <a className="switching-row__edit" href="#/config/switching#input_filter.ignore_devices">edit</a>
                </div>
              </div>

              {/* Hooks inspector (read-only) */}
              <div className="switching-hooks">
                <div className="switching-hooks__header">
                  <span className="switching-section-title">Hooks</span>
                  <span className="switching-hooks__meta">read-only</span>
                </div>
                <HooksInspector hooks={dc?.hooks} displayId={displayId} />
              </div>
            </div>
          </div>
        );
      })}
    </div>
  );
}
