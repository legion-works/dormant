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
import { useDashboardData } from "../hooks/useDashboardData";
import { Card, StatusChip, statusLabel } from "../components";
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
}

function DashDisplayRow({ id, snap, blankMode, controllers }: DashDisplayRowProps) {
  const [blanking, setBlanking] = useState(false);
  const [waking, setWaking] = useState(false);

  const handleBlank = useCallback(async () => {
    setBlanking(true);
    try { await postBlank(id); } catch { /* ignore — phase updates from next poll */ }
    setBlanking(false);
  }, [id]);

  const handleWake = useCallback(async () => {
    setWaking(true);
    try { await postWake(id); } catch { /* ignore */ }
    setWaking(false);
  }, [id]);

  return (
    <div className="dash-display-row">
      <div className="dash-display-row__top">
        <span className="dash-display-row__id">{id}</span>
        <StatusChip kind={snap.phase} />
      </div>
      <div className="dash-display-row__meta">
        <span className="dash-display-row__blank">{blankMode}</span>
        {controllers.length > 0 && (
          <span className="dash-display-row__ctl">{controllers.join(" → ")}</span>
        )}
      </div>
      <div className="dash-display-row__actions">
        <button className="dash-btn dash-btn--neutral" onClick={handleBlank} disabled={blanking}>
          {blanking ? "…" : "blank"}
        </button>
        <button className="dash-btn dash-btn--neutral" onClick={handleWake} disabled={waking}>
          {waking ? "…" : "wake"}
        </button>
      </div>
    </div>
  );
}


interface RecentEvent {
  time: string;
  type: string;
  color: string;
  bg: string;
  text: string;
}

function RecentRow({ ev }: { ev: RecentEvent }) {
  return (
    <div className="recent-row">
      <span className="recent-row__time">{ev.time}</span>
      <span className="recent-row__badge" style={{ color: ev.color, backgroundColor: ev.bg }}>
        {ev.type}
      </span>
      <span className="recent-row__text">{ev.text}</span>
    </div>
  );
}


export default function Dashboard() {
  const { loading, error, snapshot, config, sensorConfigs, zoneConfigs, displayConfigs } = useDashboardData();
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

  // Recent events synthesized from latest state changes for the activity feed.
  const recentEvents: RecentEvent[] = [];
  for (const s of sensors) {
    if (s.last_seen_secs_ago < 60) {
      recentEvents.push({
        time: `${s.last_seen_secs_ago}s ago`,
        type: "sensor",
        color: "var(--blue-400)",
        bg: "color-mix(in oklab, var(--blue-400) 13%, transparent)",
        text: `${s.id} → ${s.state}`,
      });
    }
  }
  for (const [, d] of displays) {
    if (d.cmd_gen > 0) {
      recentEvents.push({
        time: "now",
        type: "display",
        color: "var(--text-faint)",
        bg: "var(--bg-sunken)",
        text: `display → ${d.phase} (gen ${d.cmd_gen})`,
      });
    }
  }
  // Cap and sort newest first.
  recentEvents.sort((a, b) => {
    if (a.time === "now" && b.time !== "now") return -1;
    if (b.time === "now" && a.time !== "now") return 1;
    return 0;
  });
  const recentSlice = recentEvents.slice(0, 5);

  return (
    <div className="dashboard">
      {/* Stat row */}
      <div className="stat-row">
        {stats.map((s) => (
          <StatCard key={s.label} {...s} />
        ))}
      </div>

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
            return (
              <DashDisplayRow
                key={id}
                id={id}
                snap={snap}
                blankMode={blankMode}
                controllers={controllers}
              />
            );
          })}
        </Card>
      </div>

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
          recentSlice.map((ev, i) => <RecentRow key={i} ev={ev} />)
        )}
      </Card>
    </div>
  );
}
