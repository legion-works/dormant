/**
 * Controller-health chip for the Displays view.
 *
 * Renders a controller name with a healthy/unhealthy dot and
 * a primary/fallback role label.  Used in the "Controller chain"
 * section of display detail cards.
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
    <span className={`health-chip${healthy ? "" : " health-chip--unhealthy"}`}>
      <span className="health-chip__dot" />
      <span className="health-chip__name">{health.name}</span>
      <span className="health-chip__role">{roleLabel}</span>
    </span>
  );
}
