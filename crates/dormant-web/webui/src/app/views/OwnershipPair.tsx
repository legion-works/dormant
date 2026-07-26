/**
 * Ownership pair — the centrepiece of a shared display's switching surface.
 *
 * One component, three sizes:
 * - "full"  — Switching view: both machines' four codes, panel state,
 *            poll cadence, agreement verdict with all detail
 * - "compact" — Displays detail › Sharing section
 * - "marker" — Overview tile (minimal: just observed code + verdict)
 *
 * Source: views/switching.md §3
 */
import type { DisplayConfig, DisplaySnapshot, KvmStatus, CoordinationConfig } from "../../api/types";

export type OwnershipSize = "full" | "compact" | "marker";

export interface OwnershipPairProps {
  displayId: string;
  snap: DisplaySnapshot;
  config?: DisplayConfig;
  coordination?: CoordinationConfig;
  kvm?: KvmStatus | null;
  size: OwnershipSize;
}

/** Hex-echo for a u8 code — reuse W0 pattern. */
function hexCode(code: number | undefined | null): string {
  if (code == null) return "—";
  return `0x${code.toString(16).padStart(2, "0")} (${code})`;
}

/** Agreement verdict from observed_input_code vs configured codes. */
function agreementVerdict(
  observed: number | null | undefined,
  config?: DisplayConfig,
): { label: string; class: string } {
  if (observed == null) {
    return { label: "unreadable", class: "op-verdict--unreadable" };
  }
  const localRead = config?.shared_input_code;
  const peerRead = config?.shared_peer_input_code;
  if (localRead != null && observed === localRead) {
    return { label: "✓ agrees — ours", class: "op-verdict--ours" };
  }
  if (peerRead != null && observed === peerRead) {
    return { label: "peer's input", class: "op-verdict--peer" };
  }
  return { label: "⚠ third input — neither machine owns the panel", class: "op-verdict--third" };
}

/** Poll cadence label from coordination config. */
function pollCadence(coord?: CoordinationConfig): string {
  const interval = coord?.poll_interval ?? "2s";
  const confirmations = coord?.loss_confirmations ?? 3;
  const statePoll = coord?.state_poll_interval ?? `max(30s, ${interval})`;
  return `ownership every ${interval} · ${confirmations} reads to flip · panel state every ${statePoll}`;
}

export default function OwnershipPair({
  displayId,
  snap,
  config,
  coordination,
  kvm: _kvm,
  size,
}: OwnershipPairProps) {
  const observed = snap.observed_input_code;
  const verdict = agreementVerdict(observed, config);
  const isOurs = snap.owned === true;
  const localRead = config?.shared_input_code;
  const localWrite = config?.shared_input_write_code ?? localRead;
  const peerRead = config?.shared_peer_input_code;
  const peerWrite = config?.shared_peer_input_write_code;
  const panelPower = snap.panel_state?.power;
  const panelBrightness = snap.panel_state?.brightness;
  const cadence = pollCadence(coordination);

  // Marker size — minimal.
  if (size === "marker") {
    return (
      <span className="ownership-pair ownership-pair--marker">
        <span className={`op-verdict ${verdict.class}`}>{verdict.label}</span>
        <span className="op-observed">input {hexCode(observed)}</span>
      </span>
    );
  }

  // Compact size — Displays detail.
  if (size === "compact") {
    return (
      <div className="ownership-pair ownership-pair--compact">
        <div className="op-header">
          <span className={`op-owned ${isOurs ? "op-owned--ours" : ""}`}>
            {isOurs ? "● holds the panel" : "○ deferred"}
          </span>
          <span className={`op-verdict ${verdict.class}`}>{verdict.label}</span>
          <span className="op-observed">observed {hexCode(observed)}</span>
        </div>
        <div className="op-machines">
          <div className="op-machine">
            <div className="op-machine__role">this machine</div>
            <div className="op-machine__code">read {hexCode(localRead)}</div>
            <div className="op-machine__code">write {hexCode(localWrite)}</div>
          </div>
          <div className="op-machine">
            <div className="op-machine__role">peer</div>
            <div className="op-machine__code">read {hexCode(peerRead)}</div>
            <div className="op-machine__code">write {hexCode(peerWrite)}</div>
          </div>
        </div>
      </div>
    );
  }

  // Full size — Switching view.
  return (
    <div className="ownership-pair ownership-pair--full">
      {/* Header: display id + owned state + observed code */}
      <div className="op-header">
        <span className="op-display-id">{displayId}</span>
        <span className={`op-owned ${isOurs ? "op-owned--ours" : ""}`}>
          {isOurs ? "● OURS" : "○ PEER"}
        </span>
        <span className="op-observed">input {hexCode(observed)}</span>
      </div>

      {/* Two-column machine layout */}
      <div className="op-machines">
        <div className="op-machine op-machine--local">
          <div className={`op-machine__status ${isOurs ? "op-machine__status--ours" : ""}`}>
            {isOurs ? "● holds the panel" : "○ inactive"}
          </div>
          <div className="op-machine__code">read {hexCode(localRead)}</div>
          <div className="op-machine__code">
            write {hexCode(localWrite)}
            {localWrite === localRead && localRead != null && (
              <span className="op-machine__hint"> (same as read)</span>
            )}
          </div>
        </div>

        <div className="op-machine op-machine--peer">
          <div className={`op-machine__status ${!isOurs ? "op-machine__status--peer" : ""}`}>
            {!isOurs ? "● holds the panel" : "○ inactive"}
          </div>
          <div className="op-machine__code">read {hexCode(peerRead)}</div>
          <div className="op-machine__code">
            write {hexCode(peerWrite)}
            {peerWrite == null && " (unset)"}
          </div>
        </div>
      </div>

      {/* Agreement verdict */}
      <div className={`op-verdict ${verdict.class}`}>
        {verdict.label}
      </div>

      {/* Panel state + poll cadence */}
      <div className="op-panel">
        <span className="op-panel__state">
          panel{" "}
          {panelPower === "on" ? "● ON" : panelPower === "standby" ? "○ STANDBY" : "unknown"}
          {panelBrightness != null && ` · brightness ${panelBrightness}`}
        </span>
        <span className="op-panel__cadence">{cadence}</span>
      </div>
    </div>
  );
}
