/**
 * Controller-health chip for the Displays view.
 *
 * Renders a controller name with a healthy/unhealthy dot and
 * a primary/fallback role label.  Used in the "Controller chain"
 * section of display detail cards.
 *
 * When unhealthy and `detail` is present, the chip exposes it as
 * a `title` attribute (hover tooltip) and appends the detail as a
 * visible suffix so a degraded display's cause is surfaced without
 * opening the doctor report.
 */
import type { ControllerHealth } from "../../api/types";
import "./HealthChip.css";

interface HealthChipProps {
  health: ControllerHealth;
}

export default function HealthChip({ health }: HealthChipProps) {
  const healthy = health.healthy;
  const roleLabel = health.role === "primary" ? "primary" : "fallback";

  return (
    <span
      className={`health-chip${healthy ? "" : " health-chip--unhealthy"}`}
      title={health.detail ?? undefined}
    >
      <span className="health-chip__dot" />
      <span className="health-chip__name">{health.name}</span>
      <span className="health-chip__role">{roleLabel}</span>
      {!healthy && health.detail && (
        <span className="health-chip__detail">
          {" "}
          — {health.detail.length > 120
            ? `${health.detail.slice(0, 120)}…`
            : health.detail}
        </span>
      )}
    </span>
  );
}
