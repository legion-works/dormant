import { useCallback, useEffect, useRef, useState } from "react";
import ConfirmDialog, { type ConfirmOptions } from "./ConfirmDialog";

interface PendingConfirm {
  options: ConfirmOptions;
  resolve: (accepted: boolean) => void;
}

export function useConfirmDialog() {
  const [pending, setPending] = useState<PendingConfirm | null>(null);
  const pendingRef = useRef<PendingConfirm | null>(null);

  const confirm = useCallback((options: ConfirmOptions) => {
    if (pendingRef.current) return Promise.resolve(false);
    return new Promise<boolean>((resolve) => {
      const next = { options, resolve };
      pendingRef.current = next;
      setPending(next);
    });
  }, []);

  const finish = useCallback((accepted: boolean) => {
    const current = pendingRef.current;
    pendingRef.current = null;
    setPending(null);
    current?.resolve(accepted);
  }, []);

  useEffect(
    () => () => {
      pendingRef.current?.resolve(false);
      pendingRef.current = null;
    },
    [],
  );

  return {
    confirm,
    dialog: pending ? (
      <ConfirmDialog
        open
        {...pending.options}
        onConfirm={() => finish(true)}
        onCancel={() => finish(false)}
      />
    ) : null,
  };
}
