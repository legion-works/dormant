/**
 * Reconnecting WebSocket hook for the /api/events stream.
 *
 * Stub for Task 8 — the connection lifecycle, backoff, and message
 * dispatch are wired; per-event handling is filled in by Task 14
 * (Events view) and the live-patch layer.
 *
 * Design:
 * - One long-lived WS connection to /api/events.
 * - Exponential backoff (1s → 30s, capped) on disconnect / error.
 * - The caller supplies an `onMessage` callback that receives the
 *   parsed JSON (unknown shape — the Events view validates/narrows).
 * - The hook exposes `connected: boolean` so the Shell can show
 *   the connection dot and the Events badge.
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
  const { onMessage, onConnect, onDisconnect } = opts;
  const [connected, setConnected] = useState(false);
  const wsRef = useRef<WebSocket | null>(null);
  const backoffRef = useRef(BACKOFF_MIN_MS);
  const closedRef = useRef(false);

  const connect = useCallback(() => {
    if (closedRef.current) return;

    const ws = new WebSocket(WS_URL);
    wsRef.current = ws;

    ws.onopen = () => {
      setConnected(true);
      backoffRef.current = BACKOFF_MIN_MS;
      onConnect?.();
    };

    ws.onmessage = (ev: MessageEvent) => {
      try {
        const data: unknown = JSON.parse(ev.data as string);
        onMessage(data);
      } catch {
        // Ignore unparsable frames — the caller may surface a warning.
      }
    };

    ws.onclose = () => {
      if (wsRef.current === ws) {
        setConnected(false);
        onDisconnect?.();
        scheduleReconnect();
      }
    };

    ws.onerror = () => {
      // onclose fires after onerror — close it explicitly for clean state.
      ws.close();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onMessage, onConnect, onDisconnect]);

  const scheduleReconnect = useCallback(() => {
    if (closedRef.current) return;
    const delay = backoffRef.current;
    backoffRef.current = Math.min(delay * 2, BACKOFF_MAX_MS);
    setTimeout(connect, delay);
  }, [connect]);

  useEffect(() => {
    connect();
    return () => {
      closedRef.current = true;
      wsRef.current?.close();
    };
  }, [connect]);

  const close = useCallback(() => {
    closedRef.current = true;
    wsRef.current?.close();
    setConnected(false);
  }, []);

  return { connected, close };
}
