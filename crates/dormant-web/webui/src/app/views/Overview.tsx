/**
 * Overview view — at-a-glance pipeline health with panel tiles.
 *
 * Replaces Dashboard.  Route `#/overview` (alias `#/dashboard`).
 * Four stat tiles, one panel tile per display with the "held by"
 * candidate list, a three-column signal-flow grid (Sensors | Zones |
 * Rules), and a recent-activity feed.
 */
import { useNavigate } from "../nav";
import { useLiveState, useEventLog } from "../hooks/useLiveState";
import { Card, StatusChip, WearCard, useConfirmDialog, statusLabel, phaseChipLabel } from "../components";
import { badgeForEvent, messageForEvent } from "./eventFormat";
import type { SensorSnapshot, ZoneSnapshot, DisplaySnapshot, DisplayConfig, RuleConfig } from "../../api/types";
import { postBlank, postWake, postSwitch, postPush } from "../../api/client";
import { useCallback, useState } from "react";
import "./Overview.css";


interface StatCardProps {
  label: string;
  value: string | number;
  sub: string;
  dotColor: string;
  subColor?: string;
}

function StatCard({ label, value, sub, dotColor, subColor }: StatCardProps) {
  return (
    <Card>
      <div className="stat-card" style={{ "--stat-dot": dotColor } as React.CSSProperties}>
        <div className="stat-card__header">
          <span className="stat-card__dot" />
          <span className="stat-card__label">{label}</span>
        </div>
        <div className="stat-card__value">{value}</div>
        <div className="stat-card__sub" style={subColor ? { color: subColor } : undefined}>
          {sub}
        </div>
      </div>
    </Card>
  );
}


interface SectionHeaderProps {
  title: string;
  caption?: string;
  right?: React.ReactNode;
}

function SectionHeader({ title, caption, right }: SectionHeaderProps) {
  return (
    <div className="section-header">
      <h2 className="section-header__title">{title}</h2>
      {caption && <span className="section-header__caption">{caption}</span>}
      <div className="section-header__line" />
      {right}
    </div>
  );
}


interface SensorRowProps {
  sensor: SensorSnapshot;
  typeLabel: string;
}

function SensorRow({ sensor, typeLabel }: SensorRowProps) {
  const noDataSinceStart = sensor.state === "unavailable" && !(sensor.reported ?? false);

  return (
    <div className="sensor-row">
      <span className={`sensor-row__dot sensor-row__dot--${sensor.state}`} />
      <div className="sensor-row__info">
        <div className="sensor-row__id">{sensor.id}</div>
        <div className="sensor-row__type">{typeLabel}</div>
      </div>
      <div className="sensor-row__state">
        <div className={`sensor-row__state-label sensor-row__state-label--${sensor.state}`}>
          {statusLabel(sensor.state)}
        </div>
        {noDataSinceStart && (
          <div
            className="sensor-row__no-data-hint"
            title="No data has been received for this sensor since the daemon started."
          >
            no data since start
          </div>
        )}
        <div className="sensor-row__age">{sensor.last_seen_secs_ago}s ago</div>
      </div>
    </div>
  );
}


interface ZoneRowProps {
  zone: ZoneSnapshot;
  modeLabel: string;
  membersLabel: string;
}

function ZoneRow({ zone, modeLabel, membersLabel }: ZoneRowProps) {
  const present = zone.present;
  const state = present === true ? "present" : present === false ? "absent" : "unavailable";

  return (
    <div className="dash-zone-row">
      <div className="dash-zone-row__top">
        <span className={`sensor-row__dot sensor-row__dot--${state}`} />
        <span className="sensor-row__id">{zone.id}</span>
        <span className={`dash-zone-row__state dash-zone-row__state--${state}`}>
          {statusLabel(state)}
        </span>
      </div>
      <div className="dash-zone-row__meta">
        <span className="dash-zone-row__mode">{modeLabel}</span>
        <span className="dash-zone-row__members">{membersLabel}</span>
      </div>
    </div>
  );
}


// ── "held by" candidate derivation ──────────────────────────────────────────

/** One candidate reason the display is being held awake. */
interface HolderCandidate {
  reason: string;
  color: string;
}

/** Derive the list of candidate reasons a display is held awake.
 *  The snapshot's `inhibited` boolean is a single value — when more than
 *  one inhibitor is possible, the UI renders a *candidate list* prefixed
 *  with ▸ rather than fabricating a specific cause.
 *
 *  `ruleCfgs` is a map from rule id to its config (inhibitors, activity_idle_threshold).
 *  Passed from the parent which has access to `config.inventory.rules`. */
function heldByCandidates(
  id: string,
  snap: DisplaySnapshot,
  displayRules: Record<string, { rule: string; zone: string }>,
  zones: ZoneSnapshot[],
  ruleCfgs: Record<string, { inhibitors?: string[]; activity_idle_threshold?: unknown }>,
): HolderCandidate[] | null {
  const candidates: HolderCandidate[] = [];

  // 1) Zone present — the display's rule's zone has present === true.
  const dr = displayRules[id];
  if (dr) {
    const zone = zones.find((z) => z.id === dr.zone);
    if (zone && zone.present === true) {
      candidates.push({ reason: `\u2022 ${dr.zone} \u00b7 present`, color: "var(--success)" });
    } else if (zone && zone.present === false && snap.phase === "grace") {
      candidates.push({ reason: `\u2022 ${dr.zone} \u00b7 vacant \u00b7 grace`, color: "var(--warning)" });
    }
  }

  // 2) Inhibitors — read the rule's actual config, never fabricate.
  if (snap.inhibited) {
    const ruleCfg = dr ? ruleCfgs[dr.rule] : undefined;
    const activityThreshold = ruleCfg?.activity_idle_threshold;
    const inhibitors = ruleCfg?.inhibitors ?? [];

    // Collect the configured inhibitor names.
    const configured: string[] = [];
    if (activityThreshold != null) {
      configured.push("activity");
    }
    for (const inhibitor of inhibitors) {
      // audio-playback, call, or any other named inhibitor.
      configured.push(inhibitor);
    }

    if (configured.length === 1) {
      // Exactly one configured — state it flatly (no ▸ prefix).
      candidates.push({ reason: `\u2022 ${configured[0]}`, color: "var(--accent-warm)" });
    } else if (configured.length > 1) {
      // Multiple configured — list as candidates (▸ prefix).
      for (const inhibitor of configured) {
        candidates.push({ reason: `\u25b8 ${inhibitor}`, color: "var(--accent-warm)" });
      }
    } else {
      // inhibited=true but zero inhibitors configured — honest unknown.
      candidates.push({ reason: "\u25b8 unknown inhibitor", color: "var(--accent-warm)" });
    }
  }

  // 3) Paused
  if (snap.paused) {
    candidates.push({ reason: "\u25b8 paused", color: "var(--accent-warm)" });
  }

  // 4) Peer holds the panel
  if (snap.scope === "shared" && !snap.owned) {
    candidates.push({ reason: "\u25b8 peer holds the panel", color: "var(--text-muted)" });
  }

  // 5) Manual-only — no rule
  if (!dr) {
    candidates.push({ reason: "\u25b8 manual-only \u00b7 no rule", color: "var(--text-muted)" });
  }

  if (candidates.length === 0) return null;
  return candidates;
}

/** Decide whether the held-by block should use a candidate list or a flat cause.
 *  Returns the appropriate header text. */
function heldByHeader(candidates: HolderCandidate[]): string | null {
  if (!candidates || candidates.length === 0) return null;

  // If the only candidates are ambiguous (▸ prefixed), show "held awake by"
  // If there are specific causes (• prefixed), show them too.
  const hasAmbiguous = candidates.some((c) => c.reason.startsWith("\u25b8"));
  const hasSpecific = candidates.some((c) => c.reason.startsWith("\u2022"));

  if (hasAmbiguous && hasSpecific) return "held awake by";
  if (hasAmbiguous) return "held awake by";
  if (hasSpecific) return "held by";
  return null;
}


// ── Panel tile ──────────────────────────────────────────────────────────────

interface PanelTileProps {
  id: string;
  snap: DisplaySnapshot;
  dc: DisplayConfig | undefined;
  displayRules: Record<string, { rule: string; zone: string }>;
  zones: ZoneSnapshot[];
  ruleCfgs: Record<string, { inhibitors?: string[]; activity_idle_threshold?: unknown }>;
  kvm: { switch_capable_displays: string[]; push_capable_displays: string[] } | null;
  dimmed: boolean;
}

function PanelTile({ id, snap, dc, displayRules, zones, ruleCfgs, kvm, dimmed }: PanelTileProps) {
  const navigate = useNavigate();
  const { confirm, dialog } = useConfirmDialog();
  const [inFlight, setInFlight] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const phaseLabel = phaseChipLabel(snap.phase, snap.stage);
  const controllers = dc?.controllers ?? [];
  const candidates = heldByCandidates(id, snap, displayRules, zones, ruleCfgs);
  const header = heldByHeader(candidates ?? []);
  const shared = snap.scope === "shared";
  const observedInput = snap.observed_input_code;
  const owned = snap.owned;
  const canPull = kvm != null && kvm.switch_capable_displays.includes(id);
  const canPush = kvm != null && kvm.push_capable_displays.includes(id);

  const chips: { kind: string; label?: string }[] = [];
  if (snap.paused) chips.push({ kind: "paused" });
  if (snap.inhibited && snap.paused) chips.push({ kind: "inhibited" }); // inhibited is redundant with candidate list; just show it when paused
  if (snap.last_blank_failed) chips.push({ kind: "blank_failed" });

  const runAction = useCallback(async (action: string) => {
    if (action === "blank") {
      const ok = await confirm({
        title: `Force blank ${id}?`,
        description: `Immediately blanks ${id}, bypassing the normal presence rules.`,
        confirmLabel: "Force blank",
        tone: "danger",
      });
      if (!ok) return;
    }
    setError(null);
    setInFlight(action);
    try {
      if (action === "blank") await postBlank(id, "hard");
      else if (action === "wake") await postWake(id);
      else if (action === "pull") await postSwitch(id);
      else if (action === "push") await postPush(id);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : `${action} failed`);
    } finally {
      setInFlight(null);
    }
  }, [id, confirm]);

  return (
    <Card opaque className={`panel-tile${dimmed ? " panel-tile--dimmed" : ""}`}>
      <div className="panel-tile__header">
        <span
          className="panel-tile__nav"
          onClick={() => navigate("displays")}
          role="link"
          tabIndex={0}
          onKeyDown={(e) => { if (e.key === "Enter") navigate("displays"); }}
        >
          {id}
        </span>
        {shared && <span className="panel-tile__shared-marker">⇄ SHARED</span>}
        <StatusChip kind={snap.phase} label={phaseLabel} />
      </div>

      {/* State box — bordered status display */}
      <div className={`panel-tile__state-box${snap.phase === "active" ? " panel-tile__state-box--active" : ""}`}>
        <span className="panel-tile__state-box-text">
          {snap.phase === "active" ? "● ON" : "○ OFF"}
        </span>
        {observedInput != null && shared && (
          <span className="panel-tile__state-box-input">
            input 0x{observedInput.toString(16).padStart(2, "0")} · {owned ? "ours" : "not ours"}
          </span>
        )}
      </div>

      {chips.length > 0 && (
        <div className="panel-tile__chips">
          {chips.map((c) => <StatusChip key={c.kind} kind={c.kind} label={c.label} />)}
        </div>
      )}

      {candidates && candidates.length > 0 && (
        <div className="panel-tile__held-by">
          <div className="panel-tile__held-by-header">{header}</div>
          {candidates.map((c, i) => (
            <div key={i} className="panel-tile__held-by-row" style={{ color: c.color }}>
              {c.reason}
            </div>
          ))}
        </div>
      )}

      <div className="panel-tile__controllers">
        {controllers.length > 0 ? controllers.join(" \u2192 ") : "no controllers"}
      </div>

      <div className="panel-tile__actions">
        <button
          type="button"
          className="panel-tile__action-chip"
          onClick={() => void runAction("blank")}
          disabled={inFlight != null}
        >
          {inFlight === "blank" ? "Blanking…" : "Blank"}
        </button>
        <button
          type="button"
          className="panel-tile__action-chip panel-tile__action-chip--wake"
          onClick={() => void runAction("wake")}
          disabled={inFlight != null}
        >
          {inFlight === "wake" ? "Waking…" : "Wake"}
        </button>
        {canPull && (
          <button
            type="button"
            className="panel-tile__action-chip panel-tile__action-chip--pull"
            onClick={() => void runAction("pull")}
            disabled={inFlight != null}
          >
            {inFlight === "pull" ? "Pulling…" : "Pull"}
          </button>
        )}
        {canPush && (
          <button
            type="button"
            className="panel-tile__action-chip panel-tile__action-chip--push"
            onClick={() => void runAction("push")}
            disabled={inFlight != null}
          >
            {inFlight === "push" ? "Pushing…" : "Push"}
          </button>
        )}
      </div>

      {error && <div className="panel-tile__error">{error}</div>}
      {dialog}
    </Card>
  );
}


// ── Main view ───────────────────────────────────────────────────────────────

export default function Overview() {
  const { loading, error, snapshot, config, sensorConfigs, zoneConfigs, displayConfigs, displayRules, connected } = useLiveState();
  const { events } = useEventLog();
  const navigate = useNavigate();

  if (loading) {
    return <div className="overview-loading">Loading daemon state…</div>;
  }

  if (error) {
    return <div className="overview-error">Daemon unreachable: {error}</div>;
  }

  if (!snapshot || !config) {
    return <div className="overview-error">No data received from daemon.</div>;
  }

  const { sensors, zones, displays } = snapshot;
  const kvm = snapshot.kvm ?? null;

  const activeDisplays = displays.filter(([, d]) => d.phase === "active" || d.phase === "waking").length;
  const blankedDisplays = displays.length - activeDisplays;
  const onlineSensors = sensors.filter((s) => s.state !== "unavailable").length;
  const unavailableSensors = sensors.length - onlineSensors;
  const occupiedZones = zones.filter((z) => z.present === true).length;
  const vacantZones = zones.filter((z) => z.present === false).length;
  const dotGreen = "var(--success)";
  const dotAmber = "var(--warning)";

  // Protected count: displays referenced by ≥1 rule in config.display_rules.
  const protectedCount = Object.keys(displayRules).filter((id) => displays.some(([did]) => did === id)).length;
  const manualOnly = displays.length - protectedCount;

  const stats: StatCardProps[] = [
    { label: "Displays", value: displays.length, sub: `${activeDisplays} active \u00b7 ${blankedDisplays} blanked`, dotColor: dotGreen },
    { label: "Sensors", value: `${onlineSensors}/${sensors.length}`, sub: unavailableSensors > 0 ? `${unavailableSensors} unavailable` : "all online", dotColor: unavailableSensors > 0 ? dotAmber : dotGreen },
    { label: "Zones", value: `${occupiedZones}/${zones.length}`, sub: `${occupiedZones} occupied \u00b7 ${vacantZones} vacant`, dotColor: "var(--blue-400)" },
    { label: "Protected", value: protectedCount, sub: `${manualOnly} manual-only`, dotColor: protectedCount > 0 ? dotGreen : "var(--text-muted)" },
  ];

  const sensorTypeLabel = (sensor: SensorSnapshot): string => {
    const cfg = sensorConfigs[sensor.id];
    if (!cfg) return "\u2014";
    const t = (cfg as { type: string }).type;
    if (t === "mqtt") return "MQTT";
    if (t === "ha") return "HA WebSocket";
    if (t === "usb-ld2410") return "LD2410 radar";
    return t;
  };

  const recentSlice = events.filter((se) => (se.event as { event: string }).event !== "_history_separator").slice(0, 6);

  const displayRulesMap: Record<string, { rule: string; zone: string }> = displayRules;

  // Build rules column rows from config.inventory.rules + display_rules.
  // Also build ruleCfgs for use by heldByCandidates.
  const ruleRows: { id: string; zone: string; displayIds: string[]; grace?: string; inhibitors?: string[] }[] = [];
  const ruleCfgsForTiles: Record<string, { inhibitors?: string[]; activity_idle_threshold?: unknown }> = {};
  const ruleCfgInventory = config.inventory.rules as Record<string, RuleConfig> | undefined;
  if (ruleCfgInventory) {
    for (const [ruleId, _ruleCfg] of Object.entries(ruleCfgInventory)) {
      // Convert to typed access (S5 fix).
      const r = _ruleCfg;
      ruleRows.push({
        id: ruleId,
        zone: r.zone,
        displayIds: r.displays ?? [],
        grace: r.grace_period ? String(r.grace_period) : undefined,
        inhibitors: r.inhibitors,
      });
      ruleCfgsForTiles[ruleId] = {
        inhibitors: r.inhibitors,
        activity_idle_threshold: r.activity_idle_threshold,
      };
    }
  }
  // Add manual-only displays (not covered by any rule) as a pseudo-row.
  const ruleCovered = new Set(ruleRows.flatMap((r) => r.displayIds));
  const manualDisplayIds = displays.map(([id]) => id).filter((id) => !ruleCovered.has(id));

  // Dim tiles when WS is disconnected.
  const dimmed = !connected;

  return (
    <div className="overview">
      {/* Stat row */}
      <div className="stat-row">
        {stats.map((s) => (
          <StatCard key={s.label} {...s} />
        ))}
      </div>

      {/* Panel tiles */}
      {displays.length === 0 ? (
        <div className="overview-empty">
          No displays configured.{" "}
          <button className="overview-empty__link" onClick={() => navigate("config")}>Configure displays</button>
        </div>
      ) : (
        <div className="panel-grid">
          {displays.map(([id, snap]) => (
            <PanelTile
              key={id}
              id={id}
              snap={snap}
              dc={displayConfigs[id]}
              displayRules={displayRulesMap}
              zones={zones}
              ruleCfgs={ruleCfgsForTiles}
              kvm={kvm ? { switch_capable_displays: kvm.switch_capable_displays, push_capable_displays: kvm.push_capable_displays } : null}
              dimmed={dimmed}
            />
          ))}
        </div>
      )}

      {/* Signal flow — three columns: Sensors │ Zones │ Rules */}
      <SectionHeader title="Signal flow" caption="sensors → zones → rules" />

      <div className="signal-grid">
        {/* Sensors column */}
        <Card>
          <div className="column-header">Sensors</div>
          {sensors.map((s) => (
            <SensorRow key={s.id} sensor={s} typeLabel={sensorTypeLabel(s)} />
          ))}
        </Card>

        {/* Zones column */}
        <Card>
          <div className="column-header">Zones</div>
          {zones.map((z) => {
            const zc = zoneConfigs[z.id];
            const modeLabel = zc ? zc.mode.toUpperCase() : "\u2014";
            const memberCount = zc?.members.length ?? 0;
            const membersLabel = zc
              ? zc.members.slice(0, 3).join(" \u00b7 ") + (memberCount > 3 ? " \u2026" : "")
              : "\u2014";
            return (
              <ZoneRow
                key={z.id}
                zone={z}
                modeLabel={modeLabel}
                membersLabel={membersLabel}
              />
            );
          })}
        </Card>

        {/* Rules column */}
        <Card>
          <div className="column-header">Rules</div>
          {ruleRows.map((r) => (
            <div key={r.id} className="rule-row">
              <div className="rule-row__id">
                {r.id}
                {r.grace && <span className="rule-row__grace">grace {r.grace}</span>}
              </div>
              <div className="rule-row__zone-displays">{r.zone} → {r.displayIds.join(", ")}</div>
              {r.inhibitors && r.inhibitors.length > 0 && (
                <div className="rule-row__meta">
                  {r.inhibitors.map((inhib) => (
                    <span key={inhib} className="rule-row__inhibitor-chip">{inhib}</span>
                  ))}
                </div>
              )}
            </div>
          ))}
          {manualDisplayIds.length > 0 && (
            <div className="rule-row rule-row--manual">
              <div className="rule-row__id">manual-only</div>
              <div className="rule-row__zone-displays">{manualDisplayIds.join(", ")}</div>
              <div className="rule-row__meta"><span className="rule-row__grace">no rule configured</span></div>
            </div>
          )}
        </Card>
      </div>

      {/* Panel exposure */}
      <Card opaque>
        <WearCard />
      </Card>

      {/* Recent activity */}
      <SectionHeader
        title="Recent activity"
        right={
          <button
            className="section-header__link"
            onClick={() => navigate("events")}
          >
            view all →
          </button>
        }
      />

      <Card opaque>
        {recentSlice.length === 0 ? (
          <div className="recent-empty">No recent events from the daemon.</div>
        ) : (
          recentSlice.map((se, i) => {
            const badge = badgeForEvent(se.event);
            const msg = messageForEvent(se.event);
            return (
              <div key={`${se.time}-${i}`} className="recent-row">
                <span className="recent-row__time">{se.time}</span>
                <span
                  className="recent-row__badge"
                  style={{ color: badge.color, backgroundColor: badge.bg }}
                >
                  {badge.label}
                </span>
                <span className="recent-row__text">{msg}</span>
              </div>
            );
          })
        )}
      </Card>
    </div>
  );
}
