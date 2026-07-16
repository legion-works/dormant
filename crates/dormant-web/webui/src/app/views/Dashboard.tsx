/**
 * Dashboard view — at-a-glance pipeline health.
 *
 * Data: /api/state (runtime) joined with /api/config (types, zones,
 * display blank_mode/chain).  Four stat cards, a three-column signal
 * flow grid (Sensors | Zones | Displays), and a recent-activity feed.
 *
 * Visual authority: design/web-ui/Dormant Dashboard.dc.html lines 99-188.
 */
import { useNavigate } from "../nav";
import { useLiveState, useEventLog } from "../hooks/useLiveState";
import { Card, StatusChip, WearCard, useConfirmDialog, statusLabel, phaseChipLabel } from "../components";
import { badgeForEvent, messageForEvent } from "./eventFormat";
import type { SensorSnapshot, ZoneSnapshot, DisplaySnapshot } from "../../api/types";
import { postBlank, postWake } from "../../api/client";
import { useCallback, useState } from "react";
import "./Dashboard.css";


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
  // "No data since start" hint: this sensor is currently unavailable AND has
  // never delivered a single event (any state) since the daemon started —
  // as opposed to an unavailable sensor that has reported before (e.g. it
  // went stale, or the broker sent a live/retained "offline"). Legacy
  // snapshots predate `reported` entirely (the key is absent, not `false`);
  // `?? false` treats that identically to "never reported", so the hint
  // shows for an unavailable legacy sensor too — see Dashboard.test.tsx's
  // "legacy snapshot" test for the pinned rationale.
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


interface DashDisplayRowProps {
  id: string;
  snap: DisplaySnapshot;
  /** Blank mode from config (e.g. "power_off"). */
  blankMode: string;
  /** Configured controller names from config. */
  controllers: string[];
  /** `true` when this display's wear summary is under advisory (from
   * `GET /api/wear`, keyed by display name — same source WearCard reads). */
  wearAdvisory: boolean;
}

/** Live status-chip row beneath the id/phase — same derivation the
 * Displays cards use for paused/inhibited (`snap.paused`/`snap.inhibited`),
 * extended with blank-failed (`FailureBanner`'s `last_blank_failed ?? false`
 * predicate) and wear advisory (`WearCard`'s `summary.advisory`). */
function DashDisplayChips({ snap, wearAdvisory }: { snap: DisplaySnapshot; wearAdvisory: boolean }) {
  const blankFailed = snap.last_blank_failed ?? false;
  if (!snap.paused && !snap.inhibited && !blankFailed && !wearAdvisory) return null;
  return (
    <div className="dash-display-row__chips">
      {snap.paused && <StatusChip kind="paused" />}
      {snap.inhibited && <StatusChip kind="inhibited" />}
      {blankFailed && <StatusChip kind="blank_failed" />}
      {wearAdvisory && <StatusChip kind="wear_advisory" />}
    </div>
  );
}

function DashDisplayRow({ id, snap, blankMode, controllers, wearAdvisory }: DashDisplayRowProps) {
  // Force-blank/wake moved to the "Quick actions" section (single shared
  // confirm + one `{ display, action }` in-flight state, not a hook per
  // row) — this row is informational only now.
  return (
    <div className="dash-display-row">
      <div className="dash-display-row__top">
        <span className="dash-display-row__id">{id}</span>
        <StatusChip kind={snap.phase} label={phaseChipLabel(snap.phase, snap.stage)} />
      </div>
      <DashDisplayChips snap={snap} wearAdvisory={wearAdvisory} />
      <div className="dash-display-row__meta">
        <span className="dash-display-row__blank">{blankMode}</span>
        {controllers.length > 0 && (
          <span className="dash-display-row__ctl">{controllers.join(" → ")}</span>
        )}
      </div>
    </div>
  );
}


type QuickAction = "blank" | "wake";

interface QuickActionInFlight {
  display: string;
  action: QuickAction;
}

interface QuickActionsProps {
  displayIds: string[];
}

/**
 * Paired Blank/Wake chips for every display. Force blank (destructive —
 * can strand the panel dark) shares one confirmation dialog; Force wake
 * is non-destructive and un-gated (P1-F). Single `{ display, action } |
 * null` in-flight state (not one hook per display) disables only the
 * button that is actually running; a `Record<string, string>` error map
 * surfaces each display's own failure without hiding any other display's
 * actions.
 *
 * Recovery actions are never hidden based on `StateSnapshot.phase` —
 * live state can lag the operator's physical panel, so both Blank and
 * Wake stay available for every display regardless of its last-known
 * phase.
 */
function QuickActions({ displayIds }: QuickActionsProps) {
  const [inFlight, setInFlight] = useState<QuickActionInFlight | null>(null);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const { confirm, dialog } = useConfirmDialog();

  // Force blank is destructive (can strand the panel dark) and stays
  // gated; force wake is non-destructive — un-gated per P1-F.
  const run = useCallback(async (display: string, action: QuickAction) => {
    const verb = action === "blank" ? "Force blank" : "Force wake";
    if (action === "blank") {
      const accepted = await confirm({
        title: `${verb} ${display}?`,
        description: `Immediately blanks ${display}, bypassing the normal presence rules.`,
        confirmLabel: verb,
        tone: "danger",
      });
      if (!accepted) return;
    }

    setErrors((prev) => {
      if (!(display in prev)) return prev;
      const next = { ...prev };
      delete next[display];
      return next;
    });
    setInFlight({ display, action });
    try {
      if (action === "blank") await postBlank(display);
      else await postWake(display);
    } catch (err: unknown) {
      setErrors((prev) => ({
        ...prev,
        [display]: err instanceof Error ? err.message : `${verb} failed`,
      }));
    } finally {
      setInFlight(null);
    }
  }, [confirm]);

  if (displayIds.length === 0) return null;

  return (
    <>
      <SectionHeader title="Quick actions" caption="force blank (confirmed) or wake" />
      <Card opaque>
        <div className="quick-actions">
          {displayIds.map((id) => {
            const blanking = inFlight?.display === id && inFlight.action === "blank";
            const waking = inFlight?.display === id && inFlight.action === "wake";
            const rowError = errors[id];
            return (
              <div className="quick-actions__group" key={id}>
                <span className="quick-actions__id">{id}</span>
                <div className="quick-actions__chips">
                  <button
                    type="button"
                    className="quick-chip"
                    onClick={() => void run(id, "blank")}
                    disabled={blanking}
                  >
                    {blanking ? "Blanking…" : `Blank ${id}`}
                  </button>
                  <button
                    type="button"
                    className="quick-chip"
                    onClick={() => void run(id, "wake")}
                    disabled={waking}
                  >
                    {waking ? "Waking…" : `Wake ${id}`}
                  </button>
                </div>
                {rowError && <div className="quick-actions__error">{rowError}</div>}
              </div>
            );
          })}
        </div>
      </Card>
      {dialog}
    </>
  );
}


export default function Dashboard() {
  const { loading, error, snapshot, config, sensorConfigs, zoneConfigs, displayConfigs, wear } = useLiveState();
  const { events } = useEventLog();
  const navigate = useNavigate();

  if (loading) {
    return <div className="dash-loading">Loading daemon state…</div>;
  }

  if (error) {
    return <div className="dash-error">Daemon unreachable: {error}</div>;
  }

  if (!snapshot || !config) {
    return <div className="dash-error">No data received from daemon.</div>;
  }

  const { sensors, zones, displays } = snapshot;

  const activeDisplays = displays.filter(([, d]) => d.phase === "active" || d.phase === "waking").length;
  const blankedDisplays = displays.length - activeDisplays;
  const onlineSensors = sensors.filter((s) => s.state !== "unavailable").length;
  const unavailableSensors = sensors.length - onlineSensors;
  const occupiedZones = zones.filter((z) => z.present === true).length;
  const vacantZones = zones.filter((z) => z.present === false).length;
  const dotGreen = "var(--success)";
  const dotAmber = "var(--warning)";

  const stats: StatCardProps[] = [
    { label: "Displays", value: displays.length, sub: `${activeDisplays} active · ${blankedDisplays} blanked`, dotColor: dotGreen },
    { label: "Sensors", value: `${onlineSensors}/${sensors.length}`, sub: unavailableSensors > 0 ? `${unavailableSensors} unavailable` : "all online", dotColor: unavailableSensors > 0 ? dotAmber : dotGreen },
    { label: "Zones", value: `${occupiedZones}/${zones.length}`, sub: `${occupiedZones} occupied · ${vacantZones} vacant`, dotColor: "var(--blue-400)" },
    { label: "OLED guard", value: "Active", sub: "protecting on vacancy", dotColor: dotGreen },
  ];

  const sensorTypeLabel = (sensor: SensorSnapshot): string => {
    const cfg = sensorConfigs[sensor.id];
    if (!cfg) return "—";
    const t = (cfg as { type: string }).type;
    if (t === "mqtt") return "MQTT";
    if (t === "ha") return "HA WebSocket";
    if (t === "usb-ld2410") return "LD2410 radar";
    return t;
  };

  const recentSlice = events.slice(0, 6);

  return (
    <div className="dashboard">
      {/* Stat row */}
      <div className="stat-row">
        {stats.map((s) => (
          <StatCard key={s.label} {...s} />
        ))}
      </div>

      {/* Quick actions */}
      <QuickActions displayIds={displays.map(([id]) => id)} />

      {/* Signal flow */}
      <SectionHeader title="Signal flow" caption="sensors → zones → displays" />

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
            const modeLabel = zc ? zc.mode.toUpperCase() : "—";
            const memberCount = zc?.members.length ?? 0;
            const membersLabel = zc
              ? zc.members.slice(0, 3).join(" · ") + (memberCount > 3 ? " …" : "")
              : "—";
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

        {/* Displays column */}
        <Card>
          <div className="column-header">Displays</div>
          {displays.map(([id, snap]) => {
            const dc = displayConfigs[id];
            const blankMode = dc?.blank_mode ?? "—";
            const controllers = dc?.controllers ?? [];
            const wearAdvisory = wear?.displays.some((d) => d.display_name === id && d.advisory) ?? false;
            return (
              <DashDisplayRow
                key={id}
                id={id}
                snap={snap}
                blankMode={blankMode}
                controllers={controllers}
                wearAdvisory={wearAdvisory}
              />
            );
          })}
        </Card>
      </div>

      {/* Panel exposure — WearCard renders its own title + caption. */}
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
