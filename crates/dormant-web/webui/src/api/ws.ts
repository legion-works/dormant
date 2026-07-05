/**
 * Reconnecting WebSocket hook for the /api/events stream.
 *
 * One long-lived WS connection with exponential backoff (1s → 30s, capped)
 * on disconnect / error.  The caller supplies an `onMessage` callback that
 * receives the parsed JSON (unknown shape — the Events view narrows it).
 * The hook exposes `connected: boolean` so the Shell can show the
 * connection dot.
 *
 * Callback identity does NOT trigger reconnect cycles (refs, not deps).
 */
import { useRef, useEffect, useState, useCallback } from "react";

interface UseEventsOptions {
  onMessage: (data: unknown) => void;
  onConnect?: () => void;
  onDisconnect?: () => void;
}

interface UseEventsReturn {
  connected: boolean;
  /** Manually close and stop reconnecting. */
  close: () => void;
}

const WS_URL = (() => {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}/api/events`;
})();

const BACKOFF_MIN_MS = 1_000;
const BACKOFF_MAX_MS = 30_000;

export function useEvents(opts: UseEventsOptions): UseEventsReturn {
  const [connected, setConnected] = useState(false);

  const onMessageRef = useRef(opts.onMessage);
  onMessageRef.current = opts.onMessage;
  const onConnectRef = useRef(opts.onConnect);
  onConnectRef.current = opts.onConnect;
  const onDisconnectRef = useRef(opts.onDisconnect);
  onDisconnectRef.current = opts.onDisconnect;

  const wsRef = useRef<WebSocket | null>(null);
  const backoffRef = useRef(BACKOFF_MIN_MS);
  const closedRef = useRef(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // connect is stored in a ref so scheduleReconnect can call the latest
  // version without creating a circular useCallback dependency.
  const connectRef = useRef<() => void>(() => {});

  useEffect(() => {
    function scheduleReconnect() {
      if (closedRef.current) return;
      const delay = backoffRef.current;
      backoffRef.current = Math.min(delay * 2, BACKOFF_MAX_MS);
      timerRef.current = setTimeout(() => connectRef.current(), delay);
    }

    function connect() {
      if (closedRef.current) return;

      const ws = new WebSocket(WS_URL);
      wsRef.current = ws;

      ws.onopen = () => {
        setConnected(true);
        backoffRef.current = BACKOFF_MIN_MS;
        onConnectRef.current?.();
      };

      ws.onmessage = (ev: MessageEvent) => {
        try {
          const data: unknown = JSON.parse(ev.data as string);
          onMessageRef.current(data);
        } catch {
          // Ignore unparsable frames — the caller may surface a warning.
        }
      };

      ws.onclose = () => {
        if (wsRef.current === ws) {
          setConnected(false);
          onDisconnectRef.current?.();
          scheduleReconnect();
        }
      };

      ws.onerror = () => {
        ws.close();
      };
    }

    connectRef.current = connect;

    closedRef.current = false;
    connect();

    return () => {
      closedRef.current = true;
      wsRef.current?.close();
      if (timerRef.current != null) clearTimeout(timerRef.current);
    };
  }, []);

  const close = useCallback(() => {
    closedRef.current = true;
    wsRef.current?.close();
    if (timerRef.current != null) clearTimeout(timerRef.current);
    setConnected(false);
  }, []);

  return { connected, close };
}
