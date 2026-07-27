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
import type { DisplayConfig, DisplaySnapshot, CoordinationConfig } from "../../api/types";
import "./OwnershipPair.css";

export type OwnershipSize = "full" | "compact" | "marker";

export interface OwnershipPairProps {
  displayId: string;
  snap: DisplaySnapshot;
  config?: DisplayConfig;
  coordination?: CoordinationConfig;
  size: OwnershipSize;
}

/** Hex-echo for a u8 code. */
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

// Poll cadence helper removed in v3 fidelity restructure;
// the full-variant OwnershipPair now centers the panel-state box
// inline rather than in a separate footer row.

export default function OwnershipPair({
  displayId,
  snap,
  config,
  coordination,
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
  // coordination passed for future cadence UI; consumed by compact variant path.
  void coordination;

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

  // Full size — Switching view (screens/02 L245-310).
  // Three-region grid: this machine | panel-state box | peer, plus header/agreement/footer.
  return (
    <div className="ownership-pair ownership-pair--full">
      {/* Header: display id + OURS/PEER badge + observed code */}
      <div className="op-header">
        <span className="op-display-id">{displayId}</span>
        <span className={`op-owned op-owned--pill${isOurs ? " op-owned--pill-ours" : ""}`}>
          {isOurs ? "OURS" : "PEER"}
        </span>
        <span className="op-observed">observed {hexCode(observed)}</span>
      </div>

      {/* Three-region grid: left (this machine) | center (panel state) | right (peer) */}
      <div className="op-three-col">
        {/* LEFT: This machine */}
        <div className="op-three-col__left">
          <div className="op-three-col__label">This machine</div>
          <div className="op-three-col__status">
            {isOurs ? "holds the panel" : "not driving"}
          </div>
          <div className="op-three-col__codes">
            <div className="op-three-col__code-block">
              <span className="op-three-col__code-label">READ</span>
              <span className="op-three-col__code-value">{hexCode(localRead)}</span>
              <span className="op-three-col__code-dec">{localRead}</span>
            </div>
            <div className="op-three-col__code-block">
              <span className="op-three-col__code-label">WRITE</span>
              <span className="op-three-col__code-value">{hexCode(localWrite)}</span>
              <span className="op-three-col__code-dec">{localWrite}</span>
            </div>
          </div>
        </div>

        {/* CENTER: Panel state box */}
        <div className="op-three-col__center">
          <div className={`op-panel-box${panelPower === "on" ? " op-panel-box--on" : ""}`}>
            <span className="op-panel-box__state">
              {panelPower === "on" ? "● ON" : "○ OFF"}
            </span>
            {panelBrightness != null && (
              <span className="op-panel-box__brightness">brightness {panelBrightness}</span>
            )}
          </div>
          <div className={`op-verdict op-verdict--compact ${verdict.class}`}>
            {verdict.label}
          </div>
          <div className="op-three-col__cadence-hint">panel state · every 30s</div>
        </div>

        {/* RIGHT: Peer */}
        <div className="op-three-col__right">
          <div className="op-three-col__label op-three-col__label--right">Peer</div>
          <div className="op-three-col__status op-three-col__status--muted">
            dp-2 · not driving
          </div>
          <div className="op-three-col__codes op-three-col__codes--right">
            <div className="op-three-col__code-block">
              <span className="op-three-col__code-label">READ</span>
              <span className="op-three-col__code-value">{hexCode(peerRead)}</span>
              <span className="op-three-col__code-dec">{peerRead}</span>
            </div>
            <div className="op-three-col__code-block">
              <span className="op-three-col__code-label">WRITE</span>
              <span className="op-three-col__code-value">{hexCode(peerWrite)}</span>
              <span className="op-three-col__code-dec">{peerWrite}</span>
            </div>
          </div>
        </div>
      </div>

      {/* Agreement line (centered) */}
      <div className={`op-agreement ${verdict.class}`}>
        {verdict.label}
      </div>
    </div>
  );
}
