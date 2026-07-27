/**
 * Rollback recovery card — shown in Config → Protection when the daemon
 * is running on last-known-good config (§BG-5 / W5-2).
 *
 * Displays the recovery command as copy-only text and labels the two
 * fingerprints per W5-2's required operator-facing provenance:
 * "on disk now" (lkg_fp) and "the generation that booted" (failed_fp).
 */
import { useLiveState } from "../hooks/useLiveState";
import FormSection from "./FormSection";

export default function RollbackRecoveryCard() {
  const { snapshot } = useLiveState();
  const rollback = snapshot?.rollback;

  if (!rollback) return null;

  return (
    <FormSection title="Rollback recovery">
      <div
        className="cf-card"
        style={{
          borderColor: "color-mix(in oklab, var(--accent-warm) 40%, transparent)",
          background: "color-mix(in oklab, var(--accent-warm) 6%, transparent)",
        }}
      >
        {/* Recovery command */}
        {rollback.recovery_command && (
          <div className="cf-field" style={{ marginBottom: "10px" }}>
            <label className="cf-field__label" style={{ color: "var(--text-muted)" }}>
              Recovery command
            </label>
            <code
              className="rollback-banner__recovery-cmd"
              style={{ display: "block", marginTop: "4px" }}
              title="Copy to restart the daemon after fixing the config"
              onClick={(e) => {
                const el = e.currentTarget;
                void navigator.clipboard.writeText(el.textContent ?? "");
                el.classList.add("rollback-banner__recovery-cmd--copied");
                setTimeout(
                  () => el.classList.remove("rollback-banner__recovery-cmd--copied"),
                  1200,
                );
              }}
            >
              {rollback.recovery_command}
            </code>
          </div>
        )}

        {/* Fingerprints with required labels */}
        <div className="cf-field">
          <label className="cf-field__label" style={{ color: "var(--text-muted)" }}>
            Config fingerprints
          </label>
          <div
            style={{
              marginTop: "4px",
              fontFamily: "var(--font-mono)",
              fontSize: "var(--text-2xs)",
              color: "var(--text-faint)",
              display: "flex",
              flexDirection: "column",
              gap: "4px",
            }}
          >
            <div>
              <span style={{ color: "var(--text-muted)" }}>on disk now:</span>{" "}
              {rollback.lkg_fp}
            </div>
            <div>
              <span style={{ color: "var(--text-muted)" }}>
                the generation that booted:
              </span>{" "}
              {rollback.failed_fp}
            </div>
          </div>
        </div>

        <div
          className="cf-field__hint"
          style={{ marginTop: "10px", paddingTop: "8px", borderTop: "1px solid var(--border)" }}
        >
          {rollback.detail}
        </div>
      </div>
    </FormSection>
  );
}
