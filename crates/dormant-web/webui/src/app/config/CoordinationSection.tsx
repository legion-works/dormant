/**
 * Coordination section — the `[coordination]` TOML table, six keys
 * governing the soft-KVM input-switch protocol between machines.
 *
 * W1-5: 230px label column via cf-field--row.
 */
import { useState, useCallback } from "react";
import type { KvmStatus, CoordinationConfig } from "../../api/types";
import { DurationField, NumberField, BoolField } from "./fields";
import type { PatchStore } from "./patch";
import FormSection from "./FormSection";
import { readSectionAdvanced, writeSectionAdvanced } from "./density";

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
  reprobe_failure_threshold: "Number of consecutive failed input reads before on-demand DDC healing. 1–10; defaults to 3.",
  reprobe_interval: "Floor between DDC healing attempts. Failed attempts back off to 60s then 120s; the first successful read resets this floor.",
  activity_follow: "When true, a local keystroke/mouse event pulls the panel to this machine automatically.",
  arm_after: "How long after the last local activity the follow-bind arms. Disabled when activity_follow is false.",
  cooldown: "Cooldown after a successful pull before the next pull is accepted. Activity pulls only — hotkey/CLI/tray/web bypass this.",
};

export default function CoordinationSection({ coordination = {}, store, onDirty, fieldErrors, kvm }: CoordinationSectionProps) {
  const [showAdvanced, setShowAdvanced] = useState(() => readSectionAdvanced("coordination"));
  const [, rerender] = useState(0); // re-render on dirty for live chip update

  const toggleAdvanced = useCallback(() => {
    setShowAdvanced((prev) => { writeSectionAdvanced("coordination", !prev); return !prev; });
  }, []);
  const active = kvm != null;

  const root = ["coordination"];

  // Read live values — pending edits override fetched config.
  const pollInterval = (store.getEdit([...root, "poll_interval"]) as string | undefined)
    ?? coordination.poll_interval ?? "2s";
  const statePollInterval = (store.getEdit([...root, "state_poll_interval"]) as string | undefined)
    ?? coordination.state_poll_interval ?? "";
  const lossConfirmations = (store.getEdit([...root, "loss_confirmations"]) as number | undefined)
    ?? coordination.loss_confirmations ?? 3;
  const reprobeFailureThreshold = (store.getEdit([...root, "reprobe_failure_threshold"]) as number | undefined)
    ?? coordination.reprobe_failure_threshold ?? 3;
  const reprobeInterval = (store.getEdit([...root, "reprobe_interval"]) as string | undefined)
    ?? coordination.reprobe_interval ?? "30s";
  const activityFollow = (store.getEdit([...root, "activity_follow"]) as boolean | undefined)
    ?? coordination.activity_follow ?? false;
  const armAfter = (store.getEdit([...root, "arm_after"]) as string | undefined)
    ?? coordination.arm_after ?? "7s";
  const cooldown = (store.getEdit([...root, "cooldown"]) as string | undefined)
    ?? coordination.cooldown ?? "3s";

  const pollMs = parseDurationMs(pollInterval);
  const confirmations = typeof lossConfirmations === "number" ? lossConfirmations : 3;
  const derivedLatency = pollMs * confirmations;

  const lossError = typeof lossConfirmations === "number"
    && (lossConfirmations < 1 || lossConfirmations > 10)
    ? `must be 1–10, got ${lossConfirmations}` : undefined;
  const reprobeThresholdError = typeof reprobeFailureThreshold === "number"
    && (reprobeFailureThreshold < 1 || reprobeFailureThreshold > 10)
    ? `must be 1–10, got ${reprobeFailureThreshold}` : undefined;

  function edit(key: string, value: unknown) {
    store.trackEdit([...root, key], value);
    onDirty();
    rerender((n) => n + 1);
  }

  return (
    <FormSection id="coordination" title="Coordination">
      <div className="cf-card">
        <div className="cf-card__header">
          <span className="cf-card__name">[coordination]</span>
          <span className={`cf-card__type${active ? "" : " cf-card__type--inert"}`}>
            {active ? "● active" : "○ inert"}
          </span>
        </div>

        <div className="cf-card__fields">
          <div className="cf-field cf-field--row" data-field-id="coordination.poll_interval">
            <DurationField path={[...root, "poll_interval"]} label="poll_interval"
              value={pollInterval} locked={false} help={HELP.poll_interval} placeholder="2s"
              onEdit={(_, v) => edit("poll_interval", v)} />
          </div>

          <div className="cf-field cf-field--row" data-field-id="coordination.loss_confirmations">
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

          <div className="cf-field cf-field--row" data-field-id="coordination.activity_follow">
            <BoolField path={[...root, "activity_follow"]} label="activity_follow"
              value={activityFollow} locked={false} help={HELP.activity_follow}
              onEdit={(_, v) => edit("activity_follow", v)} />
          </div>

          <div className="cf-field cf-field--row" data-field-id="coordination.arm_after">
            <DurationField path={[...root, "arm_after"]} label="arm_after"
              value={armAfter} locked={false} help={HELP.arm_after} placeholder="7s"
              onEdit={(_, v) => edit("arm_after", v)} />
          </div>

          <div className="cf-field cf-field--row" data-field-id="coordination.cooldown">
            <DurationField path={[...root, "cooldown"]} label="cooldown"
              value={cooldown} locked={false} help={HELP.cooldown} placeholder="3s"
              onEdit={(_, v) => edit("cooldown", v)} />
          </div>

          <button type="button" className="cf-section__toggle" style={{ marginTop: "8px" }}
            onClick={toggleAdvanced}>
            <span className={`cf-section__chevron${showAdvanced ? " cf-section__chevron--open" : ""}`}>{"▸"}</span>
            <span style={{ fontSize: "11px", color: "var(--text-muted)" }}>Advanced</span>
          </button>

          {showAdvanced && (
            <>
              <div className="cf-field cf-field--row" data-field-id="coordination.state_poll_interval">
                <DurationField path={[...root, "state_poll_interval"]} label="state_poll_interval"
                  value={statePollInterval} locked={false} help={HELP.state_poll_interval}
                  placeholder="max(30s, poll_interval)" error={fieldErrors["coordination.state_poll_interval"]}
                  onEdit={(_, v) => edit("state_poll_interval", v)} />
              </div>
              <div className="cf-field cf-field--row" data-field-id="coordination.reprobe_failure_threshold">
                <NumberField path={[...root, "reprobe_failure_threshold"]} label="reprobe_failure_threshold"
                  value={reprobeFailureThreshold} locked={false} help={HELP.reprobe_failure_threshold}
                  error={reprobeThresholdError ?? fieldErrors["coordination.reprobe_failure_threshold"]} placeholder="3"
                  onEdit={(_, v) => edit("reprobe_failure_threshold", v)} />
              </div>
              <div className="cf-field cf-field--row" data-field-id="coordination.reprobe_interval">
                <DurationField path={[...root, "reprobe_interval"]} label="reprobe_interval"
                  value={reprobeInterval} locked={false} help={HELP.reprobe_interval} placeholder="30s"
                  error={fieldErrors["coordination.reprobe_interval"]}
                  onEdit={(_, v) => edit("reprobe_interval", v)} />
              </div>
            </>
          )}
        </div>
      </div>
    </FormSection>
  );
}
