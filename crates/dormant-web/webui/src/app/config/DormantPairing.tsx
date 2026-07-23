import { useEffect, useState } from "react";
import {
  getInstancePairPeers,
  getInstancePairStatus,
  postInstancePair,
  postJoinInstancePair,
} from "../../api/client";
import type { InstancePairOpen, InstancePairPeers } from "../../api/types";

function message(error: unknown): string {
  return error instanceof Error ? error.message : "Pairing request failed";
}

/** Operator controls for local dormant-instance pairing. */
export default function DormantPairing({ enabled }: { enabled: boolean }) {
  const [peers, setPeers] = useState<InstancePairPeers>({ discovered: [], paired: [] });
  const [name, setName] = useState("");
  const [opened, setOpened] = useState<InstancePairOpen | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [code, setCode] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function refresh() {
    if (!enabled) return;
    try {
      setPeers(await getInstancePairPeers());
    } catch (caught) {
      setError(message(caught));
    }
  }

  useEffect(() => {
    void refresh();
  }, [enabled]);

  useEffect(() => {
    if (!opened) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    function poll() {
      timer = setTimeout(() => {
        if (!opened) return;
        getInstancePairStatus(opened.pair_id)
          .then((next) => {
            if (cancelled) return;
            setStatus(next.state);
            if (next.state === "pairing") poll();
            else { setOpened(null); void refresh(); }
          })
          .catch(() => { if (!cancelled) poll(); });
      }, 1000);
    }
    poll();
    return () => { cancelled = true; if (timer) clearTimeout(timer); };
  }, [opened]);

  async function open() {
    setError(null);
    try {
      setOpened(await postInstancePair(name));
      setStatus("pairing");
    } catch (caught) {
      setError(message(caught));
    }
  }

  async function join() {
    const peer = peers.discovered.find((candidate) => candidate.instance_id === selected);
    if (!peer) return;
    setError(null);
    try {
      await postJoinInstancePair(peer.display_name, peer.instance_id, code);
      setCode("");
      await refresh();
    } catch (caught) {
      setError(message(caught));
    }
  }

  if (!enabled) return null;
  return (
    <section className="config-section" aria-labelledby="dormant-pairing-heading">
      <h2 id="dormant-pairing-heading">Instance pairing</h2>
      <label>
        Local display name
        <input value={name} onChange={(event) => setName(event.target.value)} />
      </label>
      <button type="button" disabled={!name} onClick={() => void open()}>Open pairing window</button>
      {opened && <p role="status">Code: <strong>{opened.code}</strong> · Expires in {Math.max(0, Math.ceil((Date.parse(opened.expires_at) - Date.now()) / 1000))}s</p>}
      {status && status !== "pairing" && <p role="status">Pairing {status}. Retry to open another window.</p>}
      <h3>Discovered instances</h3>
      <button type="button" onClick={() => void refresh()}>Retry discovery</button>
      {peers.discovered.length === 0 ? <p>None discovered. Retry after a peer opens pairing.</p> : (
        <ul>{peers.discovered.map((peer) => (
          <li key={peer.instance_id}>
            <label><input type="radio" name="instance" checked={selected === peer.instance_id} onChange={() => setSelected(peer.instance_id)} /> {peer.display_name} ({peer.instance_id})</label>
          </li>
        ))}</ul>
      )}
      <label>
        Pairing code
        <input value={code} onChange={(event) => setCode(event.target.value)} />
      </label>
      <button type="button" disabled={!selected || !code} onClick={() => void join()}>Confirm and join</button>
      <h3>Paired instances</h3>
      <ul>{peers.paired.map((peer) => <li key={peer.instance_id}>{peer.display_name} · paired {peer.paired_at}</li>)}</ul>
      {error && <p role="alert">{error}</p>}
    </section>
  );
}
