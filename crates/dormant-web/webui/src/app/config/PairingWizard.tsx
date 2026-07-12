/**
 * Samsung pairing wizard (spec §8, config-crud-wizard T6).
 *
 * Host input -> `POST /api/pair/samsung` -> 202 `{pair_id}` -> polls
 * `GET /api/pair/samsung/{id}` on an interval, rendering the
 * "accept the prompt on your TV" copy while `state === "pairing"`
 * (the route can't distinguish connecting from waiting-for-accept —
 * spec §8.2's "honesty limit") / `paired` / `timeout` / `error`. Hidden
 * entirely when `pairingEnabled` is false (`daemon.pairing_enabled`,
 * spec §2/§10) — the server is still the real boundary (403
 * `feature_disabled`), this is just the UI mirror.
 *
 * On `paired`, offers a "create a display?" hand-off pre-filling
 * `host` + `controllers: ["samsung-tizen"]` (spec §8.3) — the caller
 * (`SettingsForm`) wires `onDisplayCreateRequest` to open the Displays
 * section's create form with those values.
 */
import { useEffect, useRef, useState } from "react";
import { postPairSamsung, getPairStatus, ApiError } from "../../api/client";
import type { PairStatus } from "../../api/types";
import { SAMSUNG_TIZEN_CONTROLLER } from "./entityCrud";

interface PairingWizardProps {
  /** `daemon.pairing_enabled` (spec §2/§10). Defaults to true for pre-feature callers. */
  pairingEnabled?: boolean;
  /** Poll interval in ms — defaults to 1000 (spec §8.2 "~1s"); overridable for tests. */
  pollIntervalMs?: number;
  /** Called with the paired host + controllers when the operator accepts the post-pair hand-off (spec §8.3). */
  onDisplayCreateRequest?: (prefill: { host: string; controllers: string[] }) => void;
}

function errorMessage(err: unknown): string {
  if (err instanceof ApiError) {
    const body = err.body as { error?: string } | null;
    return body?.error ?? `pairing request failed (HTTP ${err.status})`;
  }
  return err instanceof Error ? err.message : String(err);
}

export default function PairingWizard({
  pairingEnabled = true,
  pollIntervalMs = 1000,
  onDisplayCreateRequest,
}: PairingWizardProps) {
  const [host, setHost] = useState("");
  const [pairId, setPairId] = useState<string | null>(null);
  const [status, setStatus] = useState<PairStatus | null>(null);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pairedHostRef = useRef("");

  // Poll loop — a self-scheduling setTimeout chain (not setInterval):
  // each tick only fires after the PREVIOUS getPairStatus call has
  // resolved and decided whether to continue, so there's never more
  // than one in-flight poll and no risk of a queued tick firing after
  // a terminal state (paired/timeout/error) has already stopped the
  // chain. Runs once per `pairId` — status changes don't restart it.
  useEffect(() => {
    if (!pairId) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;

    function scheduleNext() {
      timer = setTimeout(() => {
        if (cancelled) return;
        getPairStatus(pairId!)
          .then((s) => {
            if (cancelled) return;
            setStatus(s);
            if (s.state === "pairing") scheduleNext();
          })
          .catch(() => {
            // Transient poll failure (e.g. a momentary network blip) —
            // keep polling rather than surfacing a spurious error.
            if (!cancelled) scheduleNext();
          });
      }, pollIntervalMs);
    }

    scheduleNext();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
    };
  }, [pairId, pollIntervalMs]);

  async function start() {
    setError(null);
    setStarting(true);
    try {
      const res = await postPairSamsung(host);
      pairedHostRef.current = host;
      setPairId(res.pair_id);
      setStatus({ state: "pairing" });
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setStarting(false);
    }
  }

  function reset() {
    setPairId(null);
    setStatus(null);
    setError(null);
  }

  if (!pairingEnabled) return null;

  const pairedHost = pairedHostRef.current;

  return (
    <div className="cf-card cf-pairing-wizard" data-testid="pairing-wizard">
      <div className="cf-card__header">
        <span className="cf-card__name">Pair a Samsung TV</span>
      </div>

      {!pairId && (
        <div className="cf-field">
          <label className="cf-field__label" htmlFor="pairing-wizard-host">host</label>
          <div className="cf-field__input-row">
            <input
              id="pairing-wizard-host"
              className="cf-field__input"
              value={host}
              placeholder="192.168.1.50"
              onChange={(e) => setHost(e.target.value)}
            />
            <button
              type="button"
              className="cf-apply__btn cf-apply__btn--apply"
              onClick={start}
              disabled={!host || starting}
            >
              {starting ? "Starting…" : "Pair"}
            </button>
          </div>
          <span className="cf-field__hint">
            Starts a pairing handshake — accept the &quot;Allow&quot; prompt on the TV when it appears.
          </span>
          {error && <span className="cf-field__error">{error}</span>}
        </div>
      )}

      {pairId && status?.state === "pairing" && (
        <p className="cf-placeholder">
          Connecting to {pairedHost} — accept the &quot;Allow dormant&quot; prompt on your TV.
        </p>
      )}

      {pairId && status?.state === "timeout" && (
        <>
          <p className="cf-placeholder">
            Timed out waiting for {pairedHost}.{status.detail ? ` ${status.detail}` : ""}
          </p>
          <button type="button" className="cf-apply__btn" onClick={reset}>
            Try again
          </button>
        </>
      )}

      {pairId && status?.state === "error" && (
        <>
          <p className="cf-placeholder">
            Pairing with {pairedHost} failed.{status.detail ? ` ${status.detail}` : ""}
          </p>
          <button type="button" className="cf-apply__btn" onClick={reset}>
            Try again
          </button>
        </>
      )}

      {pairId && status?.state === "paired" && (
        <>
          <p className="cf-placeholder">Paired with {pairedHost}.</p>
          <div className="cf-card__actions">
            <button
              type="button"
              className="cf-apply__btn cf-apply__btn--apply"
              onClick={() =>
                onDisplayCreateRequest?.({
                  host: pairedHost,
                  controllers: [SAMSUNG_TIZEN_CONTROLLER],
                })
              }
            >
              Create a display for {pairedHost}?
            </button>
            <button type="button" className="cf-apply__btn" onClick={reset}>
              Pair another TV
            </button>
          </div>
        </>
      )}
    </div>
  );
}
