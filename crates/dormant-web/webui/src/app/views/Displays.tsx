/**
 * Displays view — list mode with per-display rows, or a single-display
 * detail mode when `selectedDisplay` is set.
 *
 * Data: /api/state (phase, inhibited, paused, cmd_gen, controllers[])
 * + /api/config (blank_mode, zone/rule via display_rules reverse lookup)
 * + /api/wear (wear_advisory chip).
 *
 * Row layout per views/displays.md §1: rows over cards, shared/private
 * grouping, "held by" column, full chip set (paused, inhibited,
 * blank_failed, wear_advisory).
 *
 * [keep] single-shared-`useConfirmDialog` pattern — every row's action
 * cluster hides while any dialog is open, leaving the dialog's own
 * button as the sole element with that accessible name.
 */
import { useLiveState } from "../hooks/useLiveState";
import { StatusChip, HealthChip, phaseChipLabel, useConfirmDialog } from "../components";
import { postBlank, postWake, postPause, postResume, postSwitch, postPush } from "../../api/client";
import { useCallback, useEffect, useState, useMemo } from "react";
import type { DisplaySnapshot } from "../../api/types";
import DisplayDetail from "./DisplayDetail";
import "./Displays.css";


interface DisplayRowProps {
  id: string;
  snap: DisplaySnapshot;
  zone: string;
  rule: string | undefined;
  wearAdvisory: boolean;
  dialogOpen: boolean;
  switchCapable: boolean;
  pushCapable: boolean;
  error?: string;
  onOpenDetail: (id: string) => void;
  onBlank: (id: string) => void;
  onWake: (id: string) => void;
  onPause: (id: string, rule: string) => void;
  onResume: (id: string, rule: string) => void;
  onSwitch: (id: string) => void;
  onPush: (id: string) => void;
}

/** Derive the "held by" one-liner for a display row.
 *
 *  · shared + peer-owned → "peer holds panel"
 *  · a rule is configured → "rule · zone"
 *  · otherwise → "manual-only"
 */
function heldByLabel(
  snap: DisplaySnapshot,
  zone: string,
  rule: string | undefined,
): string {
  if (snap.scope === "shared" && snap.owned === false) {
    return "peer holds panel";
  }
  if (rule) {
    return `${rule}${zone ? ` · ${zone}` : ""}`;
  }
  return "manual-only";
}

function DisplayRow({
  id,
  snap,
  zone,
  rule,
  wearAdvisory,
  dialogOpen,
  switchCapable,
  pushCapable,
  error,
  onOpenDetail,
  onBlank,
  onWake,
  onPause,
  onResume,
  onSwitch,
  onPush,
}: DisplayRowProps) {
  const isShared = snap.scope === "shared";
  const phaseDot = (() => {
    switch (snap.phase) {
      case "active": return "●";
      case "grace": return "◐";
      case "blanking": return "◑";
      case "blanked": return "○";
      case "waking": return "◔";
      case "staged": return "◑";
      case "render_pending": return "◐";
      default: return "·";
    }
  })();
  const blankFailed = snap.last_blank_failed ?? false;
  const isPaused = snap.paused;
  const sharedGlyphEl = isShared ? <span className="display-row__shared-glyph" title="shared">⇄</span> : null;

  return (
    <>
      <tr className={`display-row${isShared ? " display-row--shared" : ""}`}>
        {/* Panel */}
        <td className="display-row__panel">
          <span className="display-row__phase-dot">{phaseDot}</span>
          <span className="display-row__id">{id}</span>
          {sharedGlyphEl}
        </td>

        {/* State */}
        <td className="display-row__state">
          <div className="display-row__chips">
            <StatusChip kind={snap.phase} label={phaseChipLabel(snap.phase, snap.stage)} />
            {isPaused && <StatusChip kind="paused" />}
            {snap.inhibited && <StatusChip kind="inhibited" />}
            {blankFailed && <StatusChip kind="blank_failed" />}
            {wearAdvisory && <StatusChip kind="wear_advisory" />}
          </div>
        </td>

        {/* Held by */}
        <td className="display-row__held-by">
          <span className="display-row__held-by-text">
            {heldByLabel(snap, zone, rule)}
          </span>
        </td>

        {/* Chain */}
        <td className="display-row__chain">
          {snap.controllers.length > 0 ? (
            <div className="display-row__chain-inner">
              {snap.controllers.map((c, i) => (
                <span key={c.name} className="display-row__chain-item">
                  {i > 0 && <span className="display-row__chain-sep"> → </span>}
                  <HealthChip health={c} />
                </span>
              ))}
            </div>
          ) : (
            <span className="display-row__chain-empty">—</span>
          )}
        </td>

        {/* Actions */}
        {!dialogOpen && (
          <td className="display-row__actions">
            <button
              type="button"
              className="display-action"
              onClick={() => onBlank(id)}
            >
              Force blank
            </button>
            <button
              type="button"
              className="display-action display-action--wake"
              onClick={() => onWake(id)}
            >
              Force wake
            </button>
            {switchCapable && (
              <button
                type="button"
                className="display-action display-action--wake"
                onClick={() => onSwitch(id)}
              >
                Pull
              </button>
            )}
            {pushCapable && (
              <button
                type="button"
                className="display-action display-action--push"
                onClick={() => onPush(id)}
              >
                Push
              </button>
            )}
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
            <button
              type="button"
              className="display-action display-action--detail"
              onClick={() => onOpenDetail(id)}
            >
              Detail →
            </button>
          </td>
        )}
      </tr>
      {error && (
        <tr className="display-row__error-row">
          <td colSpan={5}>
            <div className="display-row__action-error">{error}</div>
          </td>
        </tr>
      )}
    </>
  );
}


export default function Displays() {
  const {
    loading,
    error,
    snapshot,
    displayConfigs,
    displayRules,
    wear,
    wearDetails,
    wearError,
    selectedDisplay,
    selectDisplay,
  } = useLiveState();
  const { confirm, dialog } = useConfirmDialog();
  const [actionErrors, setActionErrors] = useState<Record<string, string>>({});

  const displays = snapshot?.displays ?? [];
  const selectedSnap = selectedDisplay
    ? displays.find(([id]) => id === selectedDisplay)?.[1]
    : undefined;

  // Build a wear-advisory set keyed by config display id.
  const wearAdvisorySet = useMemo(() => {
    const set = new Set<string>();
    if (wear) {
      for (const s of wear.displays) {
        if (s.advisory) set.add(s.config_display_id ?? s.display_name);
      }
    }
    return set;
  }, [wear]);

  // Partition into shared and private for grouping — cheap inline for small lists.
  const shared: [string, DisplaySnapshot][] = [];
  const privateDisplays: [string, DisplaySnapshot][] = [];
  for (const d of displays) {
    const dc = displayConfigs[d[0]];
    if (dc?.scope === "shared" || d[1].scope === "shared") {
      shared.push(d);
    } else {
      privateDisplays.push(d);
    }
  }

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
    const dc = displayConfigs[id];
    const isShared = dc?.scope === "shared";
    const accepted = await confirm({
      title: `Force blank ${id}?`,
      description: isShared
        ? `Blank shared panel — affects all connected machines.`
        : `Immediately blanks ${id}, bypassing the normal presence rules.`,
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
  }, [confirm, clearActionError, displayConfigs]);

  const handleWake = useCallback(async (id: string) => {
    clearActionError(id);
    try {
      await postWake(id);
    } catch (err: unknown) {
      setActionErrors((prev) => ({ ...prev, [id]: err instanceof Error ? err.message : "Force wake failed" }));
    }
  }, [clearActionError]);

  const handleSwitch = useCallback(async (id: string) => {
    clearActionError(id);
    try {
      await postSwitch(id);
    } catch (err: unknown) {
      setActionErrors((prev) => ({ ...prev, [id]: err instanceof Error ? err.message : "Switch failed" }));
    }
  }, [clearActionError]);

  const handlePush = useCallback(async (id: string) => {
    clearActionError(id);
    try {
      await postPush(id);
    } catch (err: unknown) {
      setActionErrors((prev) => ({ ...prev, [id]: err instanceof Error ? err.message : "Push failed" }));
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
        wearError={wearError}
        onBack={() => selectDisplay(null)}
      />
    );
  }

  const kvm = snapshot.kvm;

  function renderRows(displaysList: [string, DisplaySnapshot][]) {
    return displaysList.map(([id, snap]) => {
      const dr = displayRules[id];
      const switchCapable = kvm != null && kvm.switch_capable_displays.includes(id);
      const pushCapable = kvm != null && kvm.push_capable_displays.includes(id);
      return (
        <DisplayRow
          key={id}
          id={id}
          snap={snap}
          zone={dr?.zone ?? "—"}
          rule={dr?.rule}
          wearAdvisory={wearAdvisorySet.has(id)}
          dialogOpen={!!dialog}
          switchCapable={switchCapable}
          pushCapable={pushCapable}
          error={actionErrors[id]}
          onOpenDetail={selectDisplay}
          onBlank={handleBlank}
          onWake={handleWake}
          onPause={handlePause}
          onResume={handleResume}
          onSwitch={handleSwitch}
          onPush={handlePush}
        />
      );
    });
  }

  const showHeaders = shared.length > 0 && privateDisplays.length > 0;

  return (
    <div className="displays">
      {/* SHARED group */}
      {shared.length > 0 && (
        <>
          {showHeaders && <h2 className="displays-group-header">SHARED</h2>}
          <table className="displays-table">
            <thead>
              <tr className="displays-table__head">
                <th>Panel</th>
                <th>State</th>
                <th>Held by</th>
                <th>Chain</th>
                {!dialog && <th>Actions</th>}
              </tr>
            </thead>
            <tbody>{renderRows(shared)}</tbody>
          </table>
        </>
      )}

      {/* PRIVATE group */}
      {privateDisplays.length > 0 && (
        <>
          {showHeaders && <h2 className="displays-group-header">PRIVATE</h2>}
          <table className="displays-table">
            <thead>
              <tr className="displays-table__head">
                <th>Panel</th>
                <th>State</th>
                <th>Held by</th>
                <th>Chain</th>
                {!dialog && <th>Actions</th>}
              </tr>
            </thead>
            <tbody>{renderRows(privateDisplays)}</tbody>
          </table>
        </>
      )}

      {displays.length === 0 && (
        <div className="displays-empty">
          No displays configured.
          <a href="#/config/displays" className="displays-empty__link"> Add a display →</a>
        </div>
      )}

      {dialog}
    </div>
  );
}
