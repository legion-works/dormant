import { useEffect, useRef } from "react";
import "./ConfirmDialog.css";

export type ConfirmTone = "default" | "danger" | "warm" | "info";

export interface ConfirmOptions {
  title: string;
  description: string;
  confirmLabel: string;
  tone?: ConfirmTone;
}

/** Per-tone icon glyph + accent CSS variable name — mirrors the proto's
 * `dialogIcons`/`dialogKindC` mapping (Dormant Dashboard.dc.html). `default`
 * has no icon tile (matches the current no-icon look for un-toned confirms
 * like Pause/Delete-adjacent generic actions). */
const TONE_ICON: Record<ConfirmTone, string | null> = {
  default: null,
  danger: "\u2715",
  warm: "\u26A0",
  info: "\u24D8",
};

export interface ConfirmDialogProps extends ConfirmOptions {
  open: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

export default function ConfirmDialog({
  open,
  title,
  description,
  confirmLabel,
  tone = "default",
  busy = false,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const icon = TONE_ICON[tone];
  const dialogRef = useRef<HTMLElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    cancelRef.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busy) onCancel();
      if (event.key !== "Tab") return;
      const focusable = dialogRef.current?.querySelectorAll<HTMLButtonElement>(
        "button:not(:disabled)",
      );
      if (!focusable || focusable.length === 0) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [busy, onCancel, open]);

  if (!open) return null;

  return (
    <div
      className="confirm-backdrop"
      data-testid="confirm-backdrop"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !busy) onCancel();
      }}
    >
      <section
        ref={dialogRef}
        className={`confirm-dialog confirm-dialog--${tone}`}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-dialog-title"
        aria-describedby="confirm-dialog-description"
      >
        <div className="confirm-dialog__header">
          {icon && (
            <span className={`confirm-dialog__icon confirm-dialog__icon--${tone}`} aria-hidden="true">
              {icon}
            </span>
          )}
          <h2 id="confirm-dialog-title">{title}</h2>
        </div>
        <p id="confirm-dialog-description">{description}</p>
        <div className="confirm-dialog__actions">
          <button ref={cancelRef} type="button" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          <button
            type="button"
            className={`confirm-dialog__confirm confirm-dialog__confirm--${tone}`}
            onClick={onConfirm}
            disabled={busy}
          >
            {busy ? "Working…" : confirmLabel}
          </button>
        </div>
      </section>
    </div>
  );
}
