/**
 * Keymap section — the `[keymap]` TOML table.
 *
 * Single field: `claim_hotkey`, rendered as a text input with a <kbd>
 * preview showing the platform key code.  The tray registers it via
 * Carbon `RegisterEventHotKey` (macOS) or the platform equivalent;
 * the web surface is purely a config editor.
 */
import type { KeymapConfig } from "../../api/types";
import { TextField } from "./fields";
import type { PatchStore } from "./patch";
import FormSection from "./FormSection";

interface KeymapSectionProps {
  keymap?: KeymapConfig;
  store: PatchStore;
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

/**
 * Produce a readable <kbd>-style label from a raw hotkey token.
 *
 * Recognises Carbon-style key codes (e.g. `kVK_F3` → `F3`) and
 * passes through raw codepoints / unknown tokens as-is.
 */
function hotkeyLabel(raw?: string | null): string {
  if (!raw) return "—";
  const m = raw.match(/^kVK_(\w+)$/i);
  if (m) return m[1]; // e.g. kVK_F3 → F3
  return raw;
}

const HELP: Record<string, string> = {
  claim_hotkey:
    "Key code for the global hotkey that pulls the panel. The tray daemon registers it; the web surface does not (and does not need Accessibility permission). macOS: Carbon kVK_* code. Linux: XKB key name.",
};

export default function KeymapSection({
  keymap = {},
  store,
  onDirty,
  fieldErrors,
}: KeymapSectionProps) {
  const hotkey = keymap.claim_hotkey ?? null;

  return (
    <FormSection title="Keymap">
      <div className="cf-card">
        <div className="cf-card__header">
          <span className="cf-card__name">[keymap]</span>
        </div>

        <div className="cf-card__fields">
          <div className="cf-field cf-field--row">
          <TextField
            path={["keymap", "claim_hotkey"]}
            label="claim_hotkey"
            value={hotkey ?? ""}
            locked={false}
            help={HELP.claim_hotkey}
            placeholder="e.g. kVK_F3"
            error={fieldErrors["keymap.claim_hotkey"]}
            onEdit={(_, v) => {
              store.trackEdit(["keymap", "claim_hotkey"], v);
              onDirty();
            }}
          />
          </div>

          <span className="cf-field__hint">
            <kbd style={{
              fontFamily: "var(--font-mono)",
              fontSize: "var(--text-xs)",
              background: "var(--bg-sunken)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius-sm)",
              padding: "2px 7px",
            }}>
              {hotkeyLabel(hotkey)}
            </kbd>
          </span>
        </div>
      </div>
    </FormSection>
  );
}
