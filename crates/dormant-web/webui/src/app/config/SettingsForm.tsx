/**
 * Settings form — editable config sections with apply/discard flow.
 *
 * Manages a PatchStore instance, dirty tracking, and the apply
 * lifecycle (call POST /api/config/apply, handle responses and
 * errors).
 */
import { useState, useRef, useCallback, useEffect } from "react";
import type { ConfigResponse, ApplyResponse, ApplyErrorBody } from "../../api/types";
import { getConfig, postConfigApply, ApiError } from "../../api/client";
import { createPatchStore } from "./patch";
import type { PatchStore } from "./patch";
import DaemonSection from "./DaemonSection";
import WearSection from "./WearSection";
import NotificationsSection from "./NotificationsSection";
import WatchdogSection from "./WatchdogSection";
import AudioSection from "./AudioSection";
import SensorsSection from "./SensorsSection";
import ZonesSection from "./ZonesSection";
import RulesSection from "./RulesSection";
import DisplaysSection from "./DisplaysSection";
import PairingWizard from "./PairingWizard";
import ApplyBar from "./ApplyBar";
import type { ApplyOutcome } from "./ApplyBar";
import { isEntityCrudEnabled, isPairingEnabled } from "./entityCrud";

interface SettingsFormProps {
  config: ConfigResponse;
  /**
   * Called when the dirty state changes so the parent can guard
   * navigation (tab-switch, route change).  Passes null when clean.
   */
  onNavigationGuard?: (guard: { dirtyCount: number; discard: () => void } | null) => void;
}

/** Extract field-level errors from a 422 ApplyErrorBody by matching detail strings. */
function extractFieldErrors(body: ApplyErrorBody): Record<string, string | undefined> {
  const map: Record<string, string | undefined> = {};
  for (const e of body.errors ?? []) {
    // detail strings often contain path-like fragments (e.g. "sensors.desk-mmwave.port: …")
    const match = e.detail.match(/^([\w._-]+(?:\.[\w._-]+)*)\s*[:|-]/);
    if (match) {
      map[match[1]] = e.detail;
    }
    // Also try "what" field as a loose key
    if (e.what && !Object.values(map).some((v) => v === e.detail)) {
      // Store under the what key if it looks path-y
      if (/\./.test(e.what)) {
        map[e.what] = e.detail;
      }
    }
  }
  return map;
}

export function SettingsForm({ config: initialConfig, onNavigationGuard }: SettingsFormProps) {
  const storeRef = useRef<PatchStore>(createPatchStore());
  const store = storeRef.current;

  const [config, setConfig] = useState<ConfigResponse>(initialConfig);
  const lastFingerprintRef = useRef(initialConfig.fingerprint);
  const [dirtyVersion, setDirtyVersion] = useState(0);
  const dirtyCount = dirtyVersion === 0 ? 0 : store.buildPatches().length + 0; // force recalc on each version

  const [applying, setApplying] = useState(false);
  const [outcome, setOutcome] = useState<ApplyOutcome | null>(null);
  const [conflict, setConflict] = useState(false);
  const [fieldErrors, setFieldErrors] = useState<Record<string, string | undefined>>({});
  const [bannerErrors, setBannerErrors] = useState<string[]>([]);
  // Pairing wizard "create display?" hand-off (spec §8.3) — set when the
  // operator accepts it, consumed by DisplaysSection to auto-open its
  // create form pre-filled, then cleared so a later manual Add doesn't
  // reuse stale values.
  const [pairingPrefill, setPairingPrefill] = useState<Record<string, unknown> | null>(null);

  // Re-sync config when the prop changes (e.g. after parent re-fetches).
  // Uses a ref to track the last-synced fingerprint rather than reading
  // config.fingerprint from state inside the effect, keeping the dep
  // array minimal and avoiding a state-on-state dependency loop.
  useEffect(() => {
    if (initialConfig.fingerprint !== lastFingerprintRef.current) {
      lastFingerprintRef.current = initialConfig.fingerprint;
      setConfig(initialConfig);
    }
  }, [initialConfig]);

  const onDirty = useCallback(() => {
    setDirtyVersion((v) => v + 1);
  }, []);

  const handleDiscard = useCallback(() => {
    store.reset();
    setDirtyVersion(0);
    setOutcome(null);
    setConflict(false);
    setFieldErrors({});
    setBannerErrors([]);
    // Re-fetch to get the latest config from the server
    getConfig()
      .then((cfg) => setConfig(cfg))
      .catch(() => {});
  }, [store]);

  const handleReload = useCallback(() => {
    store.reset();
    setDirtyVersion(0);
    setOutcome(null);
    setConflict(false);
    setFieldErrors({});
    setBannerErrors([]);
    getConfig()
      .then((cfg) => setConfig(cfg))
      .catch(() => {});
  }, [store]);

  const handleApply = useCallback(async () => {
    setApplying(true);
    setOutcome(null);
    setConflict(false);
    setFieldErrors({});
    setBannerErrors([]);

    const patches = store.buildPatches();

    try {
      const res: ApplyResponse = await postConfigApply({
        fingerprint: config.fingerprint,
        patches,
      });

      if (res.reload === "reloaded") {
        setOutcome({ kind: "reloaded" });
        // Patches accepted — clear dirty state and re-fetch for new fingerprint
        store.reset();
        setDirtyVersion(0);
        getConfig()
          .then((cfg) => setConfig(cfg))
          .catch(() => {});
      } else if (res.reload === "rejected") {
        // File was written — the daemon then refused the reload.
        // Refetch to get the new on-disk fingerprint so subsequent
        // applies use the current version.  Preserve the dirty store:
        // the user's edits are now the on-disk baseline.
        getConfig()
          .then((cfg) => setConfig(cfg))
          .catch(() => {});
        setOutcome({ kind: "rejected", detail: res.detail, fileWritten: true });
      } else {
        // pending or superseded — disk is final when the response returns
        setOutcome({ kind: res.reload as "pending" | "superseded", detail: res.detail });
        // Refetch immediately: the file was already written before the response
        getConfig()
          .then((cfg) => setConfig(cfg))
          .catch(() => {});
      }
    } catch (err: unknown) {
      if (err instanceof ApiError) {
        if (err.status === 409) {
          // Another writer changed the file — refetch to get the new
          // fingerprint.  Preserve the dirty store: "Keep editing"
          // means the user keeps their edits against the fresh baseline.
          getConfig()
            .then((cfg) => setConfig(cfg))
            .catch(() => {});
          setConflict(true);
        } else if (err.status === 422) {
          const body = err.body as ApplyErrorBody;
          if (body && body.errors) {
            setFieldErrors(extractFieldErrors(body));
            // Remaining errors not mapped to fields go to the banner
            const mapped = new Set(Object.values(extractFieldErrors(body)).filter(Boolean));
            const unmapped = body.errors
              .filter((e) => !mapped.has(e.detail))
              .map((e) => e.detail || e.what);
            setBannerErrors(unmapped);
          }
        } else {
          setOutcome({ kind: "rejected", detail: err.message });
        }
      } else {
        setOutcome({ kind: "rejected", detail: String(err) });
      }
    } finally {
      setApplying(false);
    }
  }, [config.fingerprint, store]);

  // beforeunload guard — registered while dirty, removed when clean
  // Must be declared after handleDiscard (it's a dependency of the next effect).
  useEffect(() => {
    if (dirtyCount > 0) {
      const handler = (e: BeforeUnloadEvent) => {
        e.preventDefault();
        e.returnValue = "";
      };
      window.addEventListener("beforeunload", handler);
      return () => window.removeEventListener("beforeunload", handler);
    }
  }, [dirtyCount]);

  // Tell the parent about dirty state for in-app tab/route guards
  useEffect(() => {
    if (onNavigationGuard) {
      if (dirtyCount > 0) {
        onNavigationGuard({ dirtyCount, discard: handleDiscard });
      } else {
        onNavigationGuard(null);
      }
    }
  }, [dirtyCount, onNavigationGuard, handleDiscard]);

  const inv = config.inventory;
  const entityCrudEnabled = isEntityCrudEnabled(inv.daemon);
  const pairingEnabled = isPairingEnabled(inv.daemon);
  const sensorIds = Object.keys(inv.sensors);
  const zoneIds = Object.keys(inv.zones);
  const displayIds = Object.keys(inv.displays);

  return (
    <div className="cf-form">
      <DaemonSection
        daemon={inv.daemon}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
      />

      <WearSection
        wear={inv.wear}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
      />

      <NotificationsSection
        notifications={inv.notifications}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
      />

      <WatchdogSection
        watchdog={inv.watchdog}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
      />
      <AudioSection
        audio={inv.audio}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
      />

      <SensorsSection
        sensors={inv.sensors}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
        entityCrudEnabled={entityCrudEnabled}
        zones={inv.zones}
      />

      <ZonesSection
        zones={inv.zones}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
        entityCrudEnabled={entityCrudEnabled}
        sensorIds={sensorIds}
        rules={inv.rules}
      />

      <RulesSection
        rules={inv.rules}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
        entityCrudEnabled={entityCrudEnabled}
        zoneIds={zoneIds}
        displayIds={displayIds}
      />

      <DisplaysSection
        displays={inv.displays}
        store={store}
        redactedPaths={config.redacted_paths}
        onDirty={onDirty}
        fieldErrors={fieldErrors}
        entityCrudEnabled={entityCrudEnabled}
        rules={inv.rules}
        createPrefill={pairingPrefill}
      />

      <PairingWizard
        pairingEnabled={pairingEnabled}
        onDisplayCreateRequest={(prefill) => setPairingPrefill(prefill)}
      />

      {/* Banner-level errors not mapped to fields */}
      {bannerErrors.length > 0 && (
        <div className="cf-apply__banner cf-apply__banner--rejected">
          {bannerErrors.map((e, i) => (
            <span key={i}>{e}{i < bannerErrors.length - 1 ? " · " : ""}</span>
          ))}
        </div>
      )}

      <ApplyBar
        dirtyCount={dirtyCount}
        applying={applying}
        outcome={outcome}
        conflict={conflict}
        onApply={handleApply}
        onDiscard={handleDiscard}
        onReload={handleReload}
        onDismissConflict={() => setConflict(false)}
      />
    </div>
  );
}
