/**
 * Displays view — list mode with per-display cards, or a single-display
 * detail mode (wear heat map + summaries + guarded controls) when
 * `selectedDisplay` is set.
 *
 * Data: /api/state (phase, inhibited, paused, cmd_gen, controllers[])
 * + /api/config (blank_mode, zone/rule via display_rules reverse lookup)
 * + /api/wear/:display (wear detail, keyed by display_name — provider-owned).
 *
 * Visual authority: design/web-ui/Dormant Dashboard.dc.html lines 190-248.
 *
 * List-card actions (Force blank/wake, Pause/Resume) share ONE
 * `useConfirmDialog` instance across every card — only one confirmation
 * can be pending at a time, so every card's action row hides while any
 * dialog is open (`dialogOpen`), leaving the dialog's own button as the
 * sole element with that accessible name.
 */
import { useLiveState } from "../hooks/useLiveState";
import { Card, StatusChip, HealthChip, phaseChipLabel, useConfirmDialog } from "../components";
import { postBlank, postWake, postPause, postResume } from "../../api/client";
import { useCallback, useEffect, useState } from "react";
import type { DisplaySnapshot } from "../../api/types";
import DisplayDetail from "./DisplayDetail";
import "./Displays.css";


interface DisplayCardProps {
  id: string;
  snap: DisplaySnapshot;
  blankMode: string;
  zone: string;
  rule: string | undefined;
  dialogOpen: boolean;
  error?: string;
  onOpenDetail: (id: string) => void;
  onBlank: (id: string) => void;
  onWake: (id: string) => void;
  onPause: (id: string, rule: string) => void;
  onResume: (id: string, rule: string) => void;
}

function DisplayCard({
  id,
  snap,
  blankMode,
  zone,
  rule,
  dialogOpen,
  error,
  onOpenDetail,
  onBlank,
  onWake,
  onPause,
  onResume,
}: DisplayCardProps) {
  const isShared = snap.scope === "shared";
  const panelLabel = (() => {
    switch (snap.panel_state?.power) {
      case "on": return "ON";
      case "standby": return "OFF";
      default: return "unknown";
    }
  })();

  // A peer can own a shared panel, so its local phase cannot describe hardware state.
  const previewGlyph = (() => {
    if (isShared) {
      switch (snap.panel_state?.power) {
        case "on": return "● ON";
        case "standby": return "○ OFF";
        default: return "? unknown";
      }
    }
    switch (snap.phase) {
      case "active": return "● ON";
      case "grace": return "◐ grace";
      case "blanking": return "◑ …";
      case "blanked": return "○ OFF";
      case "waking": return "◔ wake";
      case "staged": return "◑ staged";
      case "render_pending": return "◐ render";
      default: return snap.phase;
    }
  })();

  const phaseIsAlive = isShared
    ? snap.panel_state?.power === "on"
    : snap.phase === "active" || snap.phase === "waking";
  const isPaused = snap.paused;
  const blankLabel = isShared
    ? "Blank shared panel — affects all connected machines"
    : "Force blank";

  const blankModeLabel = blankMode.split("_").map((w) => w[0].toUpperCase() + w.slice(1)).join(" ");

  return (
    <Card className="display-card">
      <div className="display-card__body">
        {/* Screen preview */}
        <div className={`display-preview${phaseIsAlive ? " display-preview--alive" : ""}`}>
          <span className="display-preview__glyph">{previewGlyph}</span>
        </div>

        {/* Details */}
        <div className="display-card__details">
          <div className="display-card__title-row">
            <span className="display-card__id">{id}</span>
            <StatusChip kind={snap.phase} label={phaseChipLabel(snap.phase, snap.stage)} />
            {isPaused && <StatusChip kind="paused" />}
            {snap.inhibited && <StatusChip kind="inhibited" />}
            <button
              type="button"
              className="display-card__open-detail"
              aria-label={`Open ${id} detail`}
              onClick={() => onOpenDetail(id)}
            >
              Open detail →
            </button>
          </div>

          {/* Metric grid */}
          <div className="display-card__metrics">
            <div className="display-metric">
              <div className="display-metric__label">Blank mode</div>
              <div className="display-metric__value">{blankModeLabel}</div>
            </div>
            <div className="display-metric">
              <div className="display-metric__label">Driven by zone</div>
              <div className="display-metric__value">{zone || "—"}</div>
            </div>
            <div className="display-metric">
              <div className="display-metric__label">Rule</div>
              <div className="display-metric__value">{rule ?? "—"}</div>
            </div>
            <div className="display-metric">
              <div className="display-metric__label">Cmd gen</div>
              <div className="display-metric__value">{snap.cmd_gen}</div>
            </div>
            {isShared && (
              <>
                <div className="display-metric">
                  <div className="display-metric__label">Ownership</div>
                  <div className="display-metric__value">{snap.owned ? "owner" : "deferred"}</div>
                </div>
                <div className="display-metric">
                  <div className="display-metric__label">Panel</div>
                  <div className="display-metric__value">{panelLabel}</div>
                </div>
              </>
            )}
          </div>

          {/* Controller chain */}
          {snap.controllers.length > 0 && (
            <div className="display-card__controllers">
              <div className="display-card__controllers-label">Controller chain (fallback order)</div>
              <div className="display-card__controllers-row">
                {snap.controllers.map((c) => (
                  <HealthChip key={c.name} health={c} />
                ))}
              </div>
            </div>
          )}

          {/* Action error */}
          {error && (
            <div className="display-card__action-error">
              {error}
            </div>
          )}
        </div>

        {/* Actions column */}
        {!dialogOpen && (
          <div className="display-card__actions">
            <button
              type="button"
              className="display-action display-action--blank"
              onClick={() => onBlank(id)}
            >
              {blankLabel}
            </button>
            <button
              type="button"
              className="display-action display-action--wake"
              onClick={() => onWake(id)}
            >
              Force wake
            </button>
            {isPaused ? (
              <button
                type="button"
                className="display-action display-action--resume"
                onClick={() => rule && onResume(id, rule)}
                disabled={!rule}
              >
                Resume rule
              </button>
            ) : (
              <button
                type="button"
                className="display-action display-action--pause"
                onClick={() => rule && onPause(id, rule)}
                disabled={!rule}
              >
                Pause rule
              </button>
            )}
          </div>
        )}
      </div>
    </Card>
  );
}


export default function Displays() {
  const {
    loading,
    error,
    snapshot,
    displayConfigs,
    displayRules,
    wearDetails,
    selectedDisplay,
    selectDisplay,
  } = useLiveState();
  const { confirm, dialog } = useConfirmDialog();
  const [actionErrors, setActionErrors] = useState<Record<string, string>>({});

  const displays = snapshot?.displays ?? [];
  const selectedSnap = selectedDisplay
    ? displays.find(([id]) => id === selectedDisplay)?.[1]
    : undefined;

  // If the selected display disappears from the snapshot (e.g. removed by
  // a config reload), fall back to the list rather than getting stuck on
  // a detail view for an id that no longer exists.
  useEffect(() => {
    if (selectedDisplay && !selectedSnap) {
      selectDisplay(null);
    }
  }, [selectedDisplay, selectedSnap, selectDisplay]);

  const clearActionError = useCallback((id: string) => {
    setActionErrors((prev) => {
      if (!(id in prev)) return prev;
      const next = { ...prev };
      delete next[id];
      return next;
    });
  }, []);

  const handleBlank = useCallback(async (id: string) => {
    const accepted = await confirm({
      title: `Force blank ${id}?`,
      description: `Immediately blanks ${id}, bypassing the normal presence rules.`,
      confirmLabel: "Force blank",
      tone: "danger",
    });
    if (!accepted) return;
    clearActionError(id);
    try {
      await postBlank(id);
    } catch (err: unknown) {
      setActionErrors((prev) => ({ ...prev, [id]: err instanceof Error ? err.message : "Force blank failed" }));
    }
  }, [confirm, clearActionError]);

  // Force wake is non-destructive — it just lights the panel — so it is
  // un-gated per the proto's friction model (P1-F); Force blank keeps its
  // confirm because it can strand the panel dark.
  const handleWake = useCallback(async (id: string) => {
    clearActionError(id);
    try {
      await postWake(id);
    } catch (err: unknown) {
      setActionErrors((prev) => ({ ...prev, [id]: err instanceof Error ? err.message : "Force wake failed" }));
    }
  }, [clearActionError]);

  const handlePause = useCallback(async (id: string, rule: string) => {
    const accepted = await confirm({
      title: `Pause ${rule}?`,
      description: `Pauses rule "${rule}" until manually resumed.`,
      confirmLabel: "Pause rule",
    });
    if (!accepted) return;
    clearActionError(id);
    try {
      await postPause({ rule });
    } catch (err: unknown) {
      setActionErrors((prev) => ({ ...prev, [id]: err instanceof Error ? err.message : "Pause rule failed" }));
    }
  }, [confirm, clearActionError]);

  // Resume is non-destructive — un-gated per P1-F, same rationale as wake.
  const handleResume = useCallback(async (id: string, rule: string) => {
    clearActionError(id);
    try {
      await postResume({ rule });
    } catch (err: unknown) {
      setActionErrors((prev) => ({ ...prev, [id]: err instanceof Error ? err.message : "Resume rule failed" }));
    }
  }, [clearActionError]);

  if (loading) {
    return <div className="displays-loading">Loading daemon state…</div>;
  }

  if (error) {
    return <div className="displays-error">Daemon unreachable: {error}</div>;
  }

  if (!snapshot) {
    return <div className="displays-error">No data received from daemon.</div>;
  }

  if (selectedDisplay && selectedSnap) {
    return (
      <DisplayDetail
        id={selectedDisplay}
        snapshot={selectedSnap}
        config={displayConfigs[selectedDisplay]}
        rule={displayRules[selectedDisplay]}
        wear={wearDetails[selectedDisplay]}
        onBack={() => selectDisplay(null)}
      />
    );
  }

  return (
    <div className="displays">
      {displays.map(([id, snap]) => {
        const dc = displayConfigs[id];
        const dr = displayRules[id];
        return (
          <DisplayCard
            key={id}
            id={id}
            snap={snap}
            blankMode={dc?.blank_mode ?? "—"}
            zone={dr?.zone ?? "—"}
            rule={dr?.rule}
            dialogOpen={!!dialog}
            error={actionErrors[id]}
            onOpenDetail={selectDisplay}
            onBlank={handleBlank}
            onWake={handleWake}
            onPause={handlePause}
            onResume={handleResume}
          />
        );
      })}
      {dialog}
    </div>
  );
}
