/**
 * Coordination section — the `[coordination]` TOML table, six keys
 * governing the soft-KVM input-switch protocol between machines.
 *
 * W1-5: 230px label column via cf-field--row.
 */
import { useState } from "react";
import type { KvmStatus, CoordinationConfig } from "../../api/types";
import { DurationField, NumberField, BoolField } from "./fields";
import type { PatchStore } from "./patch";
import FormSection from "./FormSection";

interface CoordinationSectionProps {
  coordination?: CoordinationConfig;
  store: PatchStore;
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
  kvm?: KvmStatus | null;
}

function parseDurationMs(dur?: string): number {
  if (!dur) return 0;
  const m = dur.match(/^(\d+)(ms|s|m|h)$/);
  if (!m) return 0;
  const n = Number(m[1]);
  switch (m[2]) {
    case "ms": return n;
    case "s": return n * 1000;
    case "m": return n * 60_000;
    case "h": return n * 3_600_000;
    default: return 0;
  }
}

function formatMs(ms: number): string {
  if (ms >= 60_000) return `${(ms / 60_000).toFixed(0)}m`;
  if (ms >= 1000) return `${(ms / 1000).toFixed(1)}s`;
  return `${ms}ms`;
}

const HELP: Record<string, string> = {
  poll_interval: "How often the daemon polls the panel's input code (VCP 0x60). Minimum 1s.",
  state_poll_interval: "How often coordination state is refreshed. Defaults to max(30s, poll_interval) when unset; must be ≥ poll_interval.",
  loss_confirmations: "Number of consecutive poll results that report the peer's code before the daemon concedes ownership. 1–10.",
  activity_follow: "When true, a local keystroke/mouse event pulls the panel to this machine automatically.",
  arm_after: "How long after the last local activity the follow-bind arms. Disabled when activity_follow is false.",
  cooldown: "Cooldown after a successful pull before the next pull is accepted. Activity pulls only — hotkey/CLI/tray/web bypass this.",
};

export default function CoordinationSection({ coordination = {}, store, onDirty, fieldErrors, kvm }: CoordinationSectionProps) {
  const [showAdvanced, setShowAdvanced] = useState(false);
  const active = kvm != null;

  const pollInterval = coordination.poll_interval ?? "2s";
  const statePollInterval = coordination.state_poll_interval ?? "";
  const lossConfirmations = coordination.loss_confirmations ?? 3;
  const activityFollow = coordination.activity_follow ?? false;
  const armAfter = coordination.arm_after ?? "7s";
  const cooldown = coordination.cooldown ?? "3s";

  const pollMs = parseDurationMs(pollInterval);
  const confirmations = typeof lossConfirmations === "number" ? lossConfirmations : 3;
  const derivedLatency = pollMs * confirmations;

  const lossError = typeof coordination.loss_confirmations === "number"
    && (coordination.loss_confirmations < 1 || coordination.loss_confirmations > 10)
    ? `must be 1–10, got ${coordination.loss_confirmations}` : undefined;

  const root = ["coordination"];

  function edit(key: string, value: unknown) {
    store.trackEdit([...root, key], value);
    onDirty();
  }

  return (
    <FormSection title="Coordination">
      <div className="cf-card">
        <div className="cf-card__header">
          <span className="cf-card__name">[coordination]</span>
          <span className={`cf-card__type${active ? "" : " cf-card__type--inert"}`}>
            {active ? "● active" : "○ inert"}
          </span>
        </div>

        <div className="cf-card__fields">
          <div className="cf-field cf-field--row">
            <DurationField path={[...root, "poll_interval"]} label="poll_interval"
              value={pollInterval} locked={false} help={HELP.poll_interval} placeholder="2s"
              onEdit={(_, v) => edit("poll_interval", v)} />
          </div>

          <div className="cf-field cf-field--row">
            <NumberField path={[...root, "loss_confirmations"]} label="loss_confirmations"
              value={lossConfirmations} locked={false} help={HELP.loss_confirmations}
              error={lossError ?? fieldErrors["coordination.loss_confirmations"]} placeholder="3"
              onEdit={(_, v) => edit("loss_confirmations", v)} />
            {derivedLatency > 0 && (
              <span className="cf-field__hint" style={{ fontStyle: "italic" }}>
                {"~"}{formatMs(derivedLatency)}{" to commit"} — poll_interval × loss_confirmations
              </span>
            )}
          </div>

          <div className="cf-field cf-field--row">
            <BoolField path={[...root, "activity_follow"]} label="activity_follow"
              value={activityFollow} locked={false} help={HELP.activity_follow}
              onEdit={(_, v) => edit("activity_follow", v)} />
          </div>

          <div className="cf-field cf-field--row">
            <DurationField path={[...root, "arm_after"]} label="arm_after"
              value={armAfter} locked={false} help={HELP.arm_after} placeholder="7s"
              onEdit={(_, v) => edit("arm_after", v)} />
          </div>

          <div className="cf-field cf-field--row">
            <DurationField path={[...root, "cooldown"]} label="cooldown"
              value={cooldown} locked={false} help={HELP.cooldown} placeholder="3s"
              onEdit={(_, v) => edit("cooldown", v)} />
          </div>

          <button type="button" className="cf-section__toggle" style={{ marginTop: "8px" }}
            onClick={() => setShowAdvanced((o) => !o)}>
            <span className={`cf-section__chevron${showAdvanced ? " cf-section__chevron--open" : ""}`}>{"▸"}</span>
            <span style={{ fontSize: "11px", color: "var(--text-muted)" }}>Advanced</span>
          </button>

          {showAdvanced && (
            <div className="cf-field cf-field--row">
              <DurationField path={[...root, "state_poll_interval"]} label="state_poll_interval"
                value={statePollInterval} locked={false} help={HELP.state_poll_interval}
                placeholder="max(30s, poll_interval)" error={fieldErrors["coordination.state_poll_interval"]}
                onEdit={(_, v) => edit("state_poll_interval", v)} />
            </div>
          )}
        </div>
      </div>
    </FormSection>
  );
}
