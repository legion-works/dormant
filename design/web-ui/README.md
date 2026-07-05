# Handoff: dormant Web UI

## Overview

A browser-based control panel for **dormant** — the Rust daemon that blanks OLED
monitors/TVs when presence sensors report an empty room and wakes them on return.
Today dormant is driven only by the `dormantctl` CLI over a Unix-domain IPC socket.
This design adds a **read/write web dashboard** on top of that same IPC surface: a
live view of the sensor → zone → rule → display pipeline plus operator controls
(force blank/wake, pause/resume, reload).

It also includes a **brand/asset kit** (logo, favicon, icon set, README hero, GitHub
social card) for the open-source repo.

## About the Design Files

The files in this bundle are **design references authored in HTML** (as
streaming "Design Components" — `.dc.html`). They are prototypes that show the
intended look, layout, and behavior. **They are not production code to ship
directly.**

The task is to **recreate these designs in a real front-end**, wired to the running
daemon. dormant currently has **no web front-end and no HTTP server** — only a
line-delimited-JSON Unix-socket IPC. So implementation has two parts:

1. **A web front-end.** No JS framework exists in the repo yet, so pick one that
   suits a small, self-hosted, single-operator dashboard. A lightweight SPA
   (React/Vite, SolidJS, Svelte, or even plain TS) is appropriate — this is a
   homelab tool, not a large app. Match the visual spec below pixel-for-pixel.
2. **A daemon-side HTTP/WS bridge.** Add a small server (a new `dormant-web` crate,
   or a feature-gated module in `dormantd`) that connects to the existing IPC
   socket and exposes it over HTTP + WebSocket for the browser. **Do not invent a
   new data model — reuse the existing `dormant-core` IPC types verbatim** (see
   "Backend Integration"). The wire format already exists and is serde-stable.

## Fidelity

**High-fidelity (hifi).** Final colors, typography, spacing, and interaction
behavior are specified. Recreate the UI pixel-perfectly. All hex/oklch values,
font sizes, and radii in this document are authoritative. The mock uses seeded
sample data; in production every value comes from the daemon.

---

## Backend Integration (read this first)

The browser must never touch the Unix socket directly. Add an HTTP/WS bridge that
speaks the **existing** IPC protocol defined in the codebase:

- `crates/dormant-core/src/ipc_proto.rs` — `IpcRequest`, `IpcResponse`
- `crates/dormant-core/src/rules.rs` — `StateSnapshot`, `SensorSnapshot`,
  `ZoneSnapshot`, `DisplaySnapshot`, `DaemonEvent`
- `crates/dormant-core/src/types.rs` — `SensorState`, `BlankMode`, id newtypes

### IPC request/response types (source of truth — do not redefine)

`IpcRequest` (tagged `{"req": "..."}`), each maps to one UI action:

| UI action | IpcRequest | Notes |
|---|---|---|
| Load / poll state | `{"req":"status"}` | returns `IpcResponse.snapshot: StateSnapshot` |
| Live event stream | `{"req":"events"}` | subscribe; server pushes `DaemonEvent` frames |
| Force blank a display | `{"req":"blank","display":"<id>"}` | |
| Force wake a display | `{"req":"wake","display":"<id>"}` | |
| Pause a rule | `{"req":"pause","rule":"<id>","duration_s":<u64?>}` | `rule` omitted → all rules; `duration_s` omitted → indefinite |
| Resume a rule | `{"req":"resume","rule":"<id>"}` | `rule` omitted → all rules |
| Reload config | `{"req":"reload"}` | |

`IpcResponse`: `{ "ok": bool, "error"?: string, "snapshot"?: StateSnapshot }`.

### StateSnapshot shape (drives the whole dashboard)

```jsonc
{
  "sensors": [
    { "id": "desk", "state": "present|absent|unavailable", "last_seen_secs_ago": 3 }
  ],
  "zones": [
    { "id": "office", "present": true }      // present may be null if unknown
  ],
  "displays": [
    ["main", {                                // Vec<(String, DisplaySnapshot)>
      "phase": "active|grace|blanking|blanked|waking",
      "inhibited": false,
      "paused": false,
      "cmd_gen": 41
    }]
  ],
  "pending_reload": null                       // Some(detail string) when a reload is pending
}
```

### DaemonEvent shape (drives the Events view; tagged `{"event": "..."}`)

- `sensor_changed`  → `{ sensor, state }`
- `zone_changed`    → `{ zone, present, cause }`
- `display_phase`   → `{ display, phase, cause }`
- `config_reloaded` → `{}`
- `wake_retry`      → `{ display, attempt }`

> Note what the snapshot **does not** carry today: sensor `type` (mqtt/ha_ws/…),
> zone `mode`/`members`, and each display's `blank_mode`/controller-chain/zone/rule
> are **config**, not runtime state. The design shows these (e.g. "MQTT",
> "QUORUM≥2", "ddcci → fallback"). To populate them, either (a) have the bridge
> also parse/serve the loaded `Config` (`dormant-core/src/config/schema.rs`), or
> (b) extend the snapshot. Simplest: expose a second endpoint `GET /api/config`
> returning the parsed config inventory. Flag this to the maintainer — it is the
> one place the UI needs data beyond the current IPC surface.

### Suggested HTTP surface for the bridge

- `GET  /api/status`            → `StateSnapshot` (proxy of `status`)
- `GET  /api/config`            → parsed config inventory (see note above)
- `WS   /api/events`            → stream of `DaemonEvent` JSON frames
- `POST /api/displays/:id/blank`
- `POST /api/displays/:id/wake`
- `POST /api/rules/:id/pause`   → body `{ "duration_s": number|null }`
- `POST /api/rules/:id/resume`
- `POST /api/reload`

Bind to loopback by default; this is an unauthenticated local operator tool
(document that, matching dormant's "pre-alpha, not for unattended production" posture).

---

## Screens / Views

> **Authority note:** this section describes **layout and structure**, which are
> unchanged. Any specific color, font, or pixel value written inline below predates
> the Legion Works reskin — for all **color / typography / spacing tokens, the
> updated "Design Tokens" section and the `.dc.html` files are the source of
> truth** (e.g. sidebar is now 244px not 236px; fonts are Space Grotesk / Geist /
> JetBrains Mono, not IBM Plex; surfaces are Legion navy/glass, not `#0e1113`;
> accent is dormant-green `#C3E88D` with cyan chrome). Read structure here; take
> values from Design Tokens + the prototypes.

The app is a fixed two-pane layout: a **244px left sidebar** + a fluid **main
column**. Five views swap in the main column via sidebar nav. Everything is one
page; nav sets an active tab (mirror it in the URL, e.g. `/dashboard`, `/displays`).

### Global chrome

**Sidebar (236px, `#0e1113`, right border `1px rgba(255,255,255,0.07)`):**
- Brand block (padding `19px 20px 17px`, bottom border): 26px crescent mark +
  wordmark "dormant" (16px/700, letter-spacing −0.01em) with sub-line
  "v0.1.0 · pre-alpha" (IBM Plex Mono, 10.5px, `#59615c`).
- Nav (padding `12px`): five items, each `display:flex; gap:11px; padding:9px 12px;
  border-radius:8px; font-size:13.5px`. Active item: `font-weight:600`, color
  `#e7eae8`, background `rgba(255,255,255,0.06)`, icon tinted green. Inactive:
  weight 400, color `#a6ada8`, icon `#8a938e`. Hover: `background
  rgba(255,255,255,0.05)`. Items: Dashboard (▦), Displays (▤, badge "3"),
  Events (≣, badge "live" in green), Config ({ }), Doctor (✚). Badges: mono
  10.5px, `padding:1px 6px; border-radius:20px`; "live" badge uses green text on
  `oklch(0.80 0.15 155 / 0.14)`, count badges use `#8a938e` on `#191d1f`.
- Footer (margin-top:auto, top border): a pulsing green dot (expanding ring
  animation) + "dormantd running" (mono 11.5px); below, mono 10.5px `#59615c`:
  "pid 48213 · up 6h 12m" and the socket path.

**Top bar (height 60px, bottom border `1px rgba(255,255,255,0.07)`, padding 0 26px):**
- Left: current tab title (15.5px/600) + mono sub-line (11px `#59615c`).
- Right, gapped 10px: config-path pill (mono 11.5px, `#8a938e`, border
  `1px rgba(255,255,255,0.07)`, radius 7px, padding `6px 11px`, "cfg" prefix in
  `#59615c`); a live clock pill (mono 12px, green dot + `HH:MM:SS`, bg `#131618`);
  a **Reload config** button (green: text `#cfe9d8`, bg `oklch(0.80 0.15 155 /
  0.13)`, border `oklch(0.80 0.15 155 / 0.35)`, radius 7px, padding `7px 13px`,
  hover bg `/0.22`), with a "↻" mono glyph.

**Pending-reload banner** (only when `snapshot.pending_reload != null`): appears
under the top bar, margin `16px 26px 0`, padding `11px 15px`, bg `oklch(0.82 0.13
78 / 0.10)`, border `oklch(0.82 0.13 78 / 0.35)`, radius 9px, amber "⚠" +
"Config reload pending — {detail}". Fade-in animation (`translateY(-3px)`→0, 0.3s).

**Content scroll area:** `flex:1; overflow-y:auto; padding:22px 26px 40px`.

---

### 1. Dashboard
**Purpose:** at-a-glance health of the whole pipeline; quick blank/wake.

**Layout, top → bottom:**
- **Stat row** — 4-col grid, gap 14px. Each card: bg `#131618`, border
  `1px rgba(255,255,255,0.07)`, radius 11px, padding `16px 17px`. Contents: a mono
  11px uppercase label (letter-spacing 0.06em) preceded by a 6px colored dot;
  a 27px/600 value (letter-spacing −0.02em, margin-top 11px); a 12px `#8a938e`
  sub-line (margin-top 7px). The four cards:
  - **Displays** — value = display count; sub "N active · M blanked"; dot green.
  - **Sensors** — value = "online/total"; sub "K unavailable"; dot green if all
    online else amber.
  - **Zones** — value = "occupied/total"; sub "X occupied · Y vacant"; dot blue.
  - **OLED guard** — value "Active"; sub "protecting on vacancy"; dot green.
- **Section header** "Signal flow" — mono 12px/600 uppercase `#8a938e`, a mono
  11px `#59615c` caption "sensors → zones → displays", then a 1px hairline filling
  remaining width (`rgba(255,255,255,0.06)`).
- **Three columns** (1fr 1fr 1fr, gap 14px, align-items:start), each a card with a
  mono uppercase header row (padding `12px 15px`, bottom hairline):
  - **Sensors** — one row per sensor: a 9px status dot (green=present /
    blue=absent / amber=unavailable; present dot has an expanding "sonar" ring
    animation), the sensor id (13px/500) with a mono 10.5px `#59615c` type label
    under it (MQTT / LD2410 radar / HA WebSocket / motion), and right-aligned the
    state word (mono 11px, tinted to match) over "Ns ago" (mono 10.5px `#59615c`).
  - **Zones** — per zone: 9px dot (green occupied / blue vacant) + id (13px/500) +
    right-aligned state word ("occupied"/"vacant", tinted). Second line (indented
    18px): a mode chip (mono 10px `#59615c` on `#191d1f`, radius 5px — "ANY",
    "ALL", "QUORUM≥2", "WEIGHTED") + members joined by " · " (mono 10.5px `#8a938e`).
  - **Displays** — per display: id (13px/500) + right-aligned phase pill (see
    Displays view for pill spec). Second line: two equal buttons "blank" / "wake"
    (mono 11px, `#c8cdc9` on `#191d1f`, border `rgba(255,255,255,0.09)`, radius 6px,
    padding `6px 0`, hover border `rgba(255,255,255,0.2)`).
- **"Recent activity"** header (same style; right side has a "view all →" button,
  mono 11px `#8a938e`, hover `#e7eae8`, switches to Events tab) + a card listing
  the latest 5 events (row spec identical to Events view but no fixed grid columns).

### 2. Displays
**Purpose:** full per-display detail and operator control.

Vertical stack of cards, gap 14px. Each card: bg `#131618`, border, radius 12px,
padding `20px 22px`, laid out as `[preview] [details flex:1] [actions 130px]`.
- **Preview** (96×60, radius 8px): a mini "screen". When phase is active/waking:
  bg `oklch(0.80 0.15 155 / 0.08)`, border `/0.3`, glyph in green. Otherwise bg
  `#0a0c0d`, border `rgba(255,255,255,0.08)`, dim glyph. Glyphs by phase: active
  "● ON", grace "◐ grace", blanking "◑ …", blanked "○ OFF", waking "◔ wake" (mono 9.5px).
- **Details:** title row = display id (16px/600) + **phase pill** + optional
  "paused"/"inhibited" pills.
  - Phase pill: `inline-flex; gap:6px; mono 11px; padding:2px 9px; radius:20px`,
    text+dot colored by phase, bg = phase color at `/0.13`. Phase colors: active/
    waking = green `oklch(0.80 0.15 155)`; grace/blanking = amber `oklch(0.82 0.13
    78)`; blanked = blue `oklch(0.74 0.09 240)`.
  - "paused" pill: amber text on amber `/0.12`. "inhibited" pill: blue text on
    blue `/0.12`. Both mono 11px, radius 20px.
  - **Metric grid** (margin-top 15px, 4 columns, gap 26px): each = mono 10px
    uppercase `#59615c` label + mono 12.5px `#c8cdc9` value. Fields: Blank mode
    (`power_off` / `screen_off_audio_on` / `brightness_zero`), Driven by zone,
    Rule, Cmd gen (`cmd_gen` from snapshot).
  - **Controller chain** (margin-top 15px): uppercase mono label "Controller chain
    (fallback order)", then a flex row of controller chips. Chip: `inline-flex;
    gap:7px; mono 11.5px; padding:5px 10px; radius:7px; bg #191d1f`. Dot green if
    healthy, amber if not; border `rgba(255,255,255,0.09)` (healthy) or amber
    `/0.35`; trailing "primary"/"fallback" tag in mono 10px `#59615c`.
- **Actions column (130px, gap 8px):**
  - "Force blank" — neutral (text `#e7eae8`, bg `#191d1f`, border
    `rgba(255,255,255,0.12)`, hover border `/0.25`).
  - "Force wake" — green (text `#cfe9d8`, bg green `/0.13`, border `/0.35`, hover `/0.22`).
  - "Pause rule" / "Resume rule" — toggles; when paused it renders "Resume rule"
    amber (text `#e8d8b0`, bg amber `/0.12`, border `/0.35`); otherwise neutral
    ("Pause rule", `#a6ada8`). All buttons: 12.5px/500, radius 7px, padding `9px 0`.

### 3. Events
**Purpose:** live daemon event stream (WS-backed).

- Header row: green pulsing dot + "live · subscribed to daemon event stream" (mono
  12px `#c8cdc9`), right-aligned "N events" (mono 11px `#59615c`).
- List card (bg `#0e1113`, border, radius 11px). Each row is a 3-col grid
  `82px 118px 1fr`, gap 14px, padding `9px 16px`, bottom hairline, mono 12px:
  timestamp `#59615c` · **type badge** · message `#c8cdc9`.
- Type badge: mono 10.5px, `padding:2px 8px; radius:5px`, centered, colored by
  event type — `zone_change` green, `sensor_change` blue, `display_phase` neutral
  grey, `wake_retry` red `oklch(0.68 0.19 25)`, `config_reload`/`pause` amber,
  `resume` green. Badge bg = its color at `/0.13` (grey uses `oklch(0.7 0 0)/0.13`).
- Newest first; prepend as WS frames arrive; cap the rendered list (mock caps 40).

### 4. Config
**Purpose:** show the loaded config and its validation.

Two columns `1.1fr 0.9fr`, gap 16px, align-items:start.
- **Left — file viewer** (bg `#0e1113`, border, radius 11px): header row (mono
  11.5px `#8a938e`) with a "📄" + path + right-aligned "✓ valid · v1" in green.
  Body: the TOML rendered **line-by-line as block elements** (do not rely on a
  single `<pre>` with newlines — see Gotchas). Mono 12px, line-height 1.85. Syntax
  colors: comments/section-headers `#59615c`, keys blue `oklch(0.74 0.09 240)`,
  `=` `#8a938e`, string values green `oklch(0.80 0.15 155)`, numeric values amber
  `oklch(0.82 0.13 78)`.
- **Right column, two stacked cards:**
  - "Parsed inventory" (bg `#131618`): rows of `label (mono 12px #8a938e, 90px)` +
    `value (13px #e7eae8)` + right-aligned count (mono 11px green). Rows: sensors,
    zones, displays, rules.
  - Validation note: bg green `/0.06`, border `/0.25`, radius 11px, green "✓" +
    12.5px `#b8d8c4` body confirming no unknown keys / all references resolve.

### 5. Doctor
**Purpose:** environment & integration diagnostics (mirrors `dormantctl doctor`).

- **Summary row** — 3 equal cards (bg `#131618`): big 26px/600 count in status
  color + mono 11px uppercase `#8a938e` label. Passing (green), Warnings (amber),
  Failing (red).
- **Checks list** (bg `#0e1113`, border, radius 11px): each row = a 20px status
  circle (icon "✓"/"!"/"✕" in status color on status `/0.13`), title (13.5px/500),
  mono 11.5px `#8a938e` detail line, and a right-aligned uppercase status tag pill
  (mono 10.5px, radius 20px). Example checks in mock: config valid, IPC socket
  reachable, credentials file 0600, DDC/CI device present, MQTT broker connection
  (pass); sensor stale (warn); KWin DPMS controller not wired (fail). In
  production these come from the doctor command (`dormantctl/src/cmd_doctor.rs`).

---

## Interactions & Behavior

- **Nav:** click sets active tab; reflect in URL/history. "view all →" on
  Dashboard jumps to Events.
- **Force blank / wake:** POST to the display endpoint. Optimistically set the
  display's phase to `blanked` / `active` and bump `cmd_gen`; reconcile from the
  next snapshot/`display_phase` event. Append a synthetic operator line to the
  local event list too (the daemon will also emit the authoritative event).
- **Pause / resume rule:** POST; toggle the paused pill. If you support a duration,
  add a small menu (indefinite / 30m / 2h) that sets `duration_s`.
- **Reload:** POST `reload`; immediately show the pending-reload banner; clear it on
  the `config_reloaded` event (or when `pending_reload` clears in the next snapshot).
- **Live updates:** subscribe to the WS event stream; also poll `GET /api/status`
  (mock effectively refreshes every 1s) to age "last seen" counters and keep the
  clock ticking. The green present-sensor dot has a continuous "sonar" ring
  animation. The "live" nav badge and Events header dot pulse.
- **Animations (exact) — quick, no bounce (Legion Works motion):**
  - `dmDrift` — 34s ease-in-out infinite alternate: slow aurora background-position
    drift (the DS `.lw-aurora--drift`).
  - `dmLive` — 1.8s ease infinite: opacity 1 → 0.4 → 1, on the Events "live" dot only.
  - `dmRise` — 0.36s ease-out: opacity 0 / `translateY(8px)` → 1 / none
    (pending-reload banner; the DS `.lw-enter` entrance).
  - present-sensor / daemon-running dots: **static** color + `box-shadow` glow — no
    sonar loop (the DS avoids decorative infinite loops). Honor
    `prefers-reduced-motion` (disable drift + pulse).
- **Hover states:** listed per-component above (buttons brighten border or bg).
- **Empty/error states (add for production):** show a clear "daemon unreachable"
  state if `GET /api/status` fails (the socket may be down); disable action
  buttons while a request is in flight; surface `IpcResponse.error` as a toast.

## State Management

Minimal client state:
- `activeTab: 'dashboard'|'displays'|'events'|'config'|'doctor'`
- `snapshot: StateSnapshot` (from `GET /api/status`, refreshed on poll + events)
- `config: ConfigInventory` (from `GET /api/config`)
- `events: DaemonEvent[]` (prepended from WS; capped)
- `clock: string` (local, 1s interval)
- `pendingReload: string | null` (from snapshot; also set optimistically on reload)
- in-flight flags per action for disabling buttons

Data fetching: one status poll (≈1s) + one long-lived WS for events + one config
fetch on load and after `config_reloaded`. All mutations are POSTs that resolve to
`IpcResponse`; on `ok:false` show the error and roll back optimistic UI.

## Design Tokens

> **This design follows the Legion Works design system** (Tokyo Night–grounded
> Liquid Glass, the house style for IceTea / Legion fleet daemons — dormant is one
> of them). In production, **link the Legion Works `styles.css` and consume its CSS
> custom properties** (`--bg-base`, `--accent`, `--glass-*`, `--font-*`, `--radius-*`,
> etc.) rather than hardcoding the hex values below. The values are listed so the UI
> can be matched exactly and to make the dormant-specific choices explicit. **What
> makes dormant distinct within the fleet:** its signature signal color is Tokyo
> Night green `#C3E88D` (= present / awake / active), and it keeps its own
> crescent-moon mark. Cyan `#86E1FC` is used only as Legion "system chrome" (reload
> button, clock, live pulse, the small Legion mark) — the visual thread that says
> "part of the Legion fleet."

**Colors** (Legion Works tokens; `var()` name in parens)
- Base bg: `#16161E` (`--bg-base`); data/list surface: `#1a1b26`; raised panel:
  `#222436` (`--bg-raised`); sunken: `#101018`
- **Glass panels** (stat cards, columns, display cards, doctor/config summary):
  fill `rgba(36,40,58,0.72)`, border `1px rgba(169,177,214,0.14)` (`--border`),
  radius 14px (`--radius-lg`), shadow `0 18px 50px -12px rgba(0,0,0,0.62)`
  (`--shadow-lg`). The DS's true Liquid Glass adds `backdrop-filter: blur(22px)
  saturate(160%)` + a specular top edge — apply that in production over the
  `.lw-aurora` field; the prototype uses the solid navy fill for capture/export
  reliability (see Gotchas).
- Sidebar: `rgba(16,16,24,0.5)` + backdrop blur; right border `--border`
- Borders: `rgba(169,177,214,0.14)` (`--border`), `0.28` (`--border-strong`); row
  hairlines `rgba(169,177,214,0.06)`; accent border = cyan @48% (`--border-accent`)
- Text: strong `#c8d3f5` (`--text-strong`); body `#a9b1d6` (`--text-body`); muted
  `#828bb8` (`--text-muted`); faint `#737aa2` (`--text-faint`); dim `#545c7e`
- **dormant green** — present / active / waking / ok: `#C3E88D` (`--success`/
  `--green-400`); light-mode crescent variant `#3aa06a`
- Legion cyan — system chrome only (reload, clock, live pulse, links): `#86E1FC`
  (`--accent`/`--cyan-400`)
- Signal blue — absent / blanked: `#82AAFF` (`--blue-400`)
- Warning yellow — grace / blanking / unavailable: `#FFC777` (`--warning`/`--amber-300`)
- IceTea amber — paused (the one warm human touch): `#FF966C` (`--accent-warm`/`--amber-500`)
- Geth purple — inhibited: `#C099FF` (`--purple-400`)
- Danger red — fail / wake_retry: `#FF757F` (`--danger`/`--red-400`)
- Status-tint convention: status color at `0.12`–`0.14` alpha for the matching soft
  background; the DS provides `--success-muted` / `--warning-muted` / etc. for this.
- A `[data-theme="nightowl"]` sibling and `[data-theme="light"]` (Tokyo Day) retint
  everything — inherit them for free by using the DS variables.

**Typography** (Legion Works families — Google Fonts; DS `var()` in parens)
- Display / headings / wordmark / metrics: **Space Grotesk** (`--font-display`),
  600, tight tracking (page title 18px, stat value 30px, display id 17px)
- UI & body: **Geist** (`--font-ui`), 400/500 (buttons, nav, body 13–14px)
- Machine-truth — ids, config, event log, timestamps, labels: **JetBrains Mono**
  (`--font-mono`), 400/500. Eyebrows/labels are UPPERCASE, `letter-spacing 0.14em`
  (`--tracking-caps`), 10.5–11px, muted.

**Spacing / radius / motion** (DS scale)
- Content padding 22–26px; card padding 15–22px; grid gaps 12–16px.
- Radii: cards/panels 14px (`--radius-lg`), large glass 20px (`--radius-xl`);
  buttons/inputs 10px (`--radius-md`); chips/pills 999px (`--radius-pill`); small 6–8px.
- Sidebar width 244px; top bar height 64px; display action column 132px; display
  preview 98×62.
- Motion: quick, **no bounce** — `--dur-fast 120ms` / `--dur-mid 200ms`, ease-out
  `cubic-bezier(0.22,0.61,0.36,1)`. Ambient: a slow 34s aurora drift (`dmDrift`,
  the DS `.lw-aurora--drift`) and a fade-rise entrance. The Events "live" dot has a
  gentle 1.8s opacity pulse; present-sensor / running dots use a static color +
  `box-shadow` glow (no sonar loop — the DS avoids decorative infinite loops).
  Honor `prefers-reduced-motion` (the DS zeroes durations).

**Background:** the whole app sits on the DS **aurora field** — `background-color:
#16161E` + layered radial-gradient blooms (cyan top-left, blue top-right, one warm
amber low-right, purple bottom). Glass panels float over it; never put glass over a
flat solid. Dense data surfaces (event log, config viewer) use opaque `#1a1b26`
(the DS says don't glass long data lists).

## Brand Assets

The identity bridges dormant's own meaning with the Legion fleet: a **crescent-moon
mark** (dormant = asleep) in **dormant green `#C3E88D`** with a small **Legion-cyan
`#86E1FC` presence node** — literally "dormant, part of Legion." Wordmark is **Space
Grotesk** 600. See `Brand Assets.dc.html` for the full board (logo lockups
dark/light, favicon/app-icon sizes, the Legion palette with dormant-green
highlighted, Space Grotesk / Geist / JetBrains Mono specimens, a 12-icon set, a
1232×340 README hero, and a 1200×630 GitHub OG card — `github.com/iceteaSA/dormant`).

Ready-to-use vector files are in `assets/`:
- `assets/mark.svg` — crescent (green, transparent via SVG mask) + cyan node; drops
  on any background. Favicon/app-icon source.
- `assets/favicon.svg` — mark on a `#16161E` rounded-square tile.
- `assets/logo.svg` — horizontal lockup (mark + Space Grotesk "dormant" wordmark).
- `assets/legion-mark.svg` — the shared **Legion Works** consensus mark, used small
  as the "part of the Legion fleet" tie-in (sidebar footer, OG credit).

Icon set — recreate with **Lucide** (the DS icon system) or inline SVG in that style
(1.75px stroke, dormant-green, round caps/joins, geometric — no illustrative
detail): presence, mqtt, ha-ws, ld2410, motion, input-idle, display, zone, rule,
power_off, screen_off·audio, brightness_0. Definitions are in `renderVals()` of
`Brand Assets.dc.html`. Nav icons (dashboard/displays/events/config/doctor) are
already Lucide-style in the dashboard.

## Files

In this bundle:
- `Dormant Dashboard.dc.html` — the full dashboard prototype (all 5 views + live
  behavior). Primary reference.
- `Brand Assets.dc.html` — brand & asset board.
- `assets/mark.svg`, `assets/logo.svg`, `assets/favicon.svg` — dormant production
  vectors; `assets/legion-mark.svg` — shared Legion Works fleet mark (tie-in).

In the dormant repo (read these to wire the backend — do not re-model the data):
- `crates/dormant-core/src/ipc_proto.rs` — `IpcRequest` / `IpcResponse`
- `crates/dormant-core/src/rules.rs` — `StateSnapshot` & `DaemonEvent` (+ the
  `Snapshot` variants)
- `crates/dormant-core/src/types.rs` — `SensorState`, `BlankMode`, id newtypes
- `crates/dormant-core/src/config/schema.rs` — `Config` (for the `/api/config`
  inventory the UI needs)
- `crates/dormantd/` — where the IPC server lives (natural home, or a sibling, for
  the HTTP/WS bridge)
- `crates/dormantctl/src/cmd_doctor.rs` — the doctor checks the Doctor view mirrors

## Gotchas / notes for the implementer

- **The prototype is inline-styled** (a constraint of the authoring format). In a
  real codebase, **link Legion Works `styles.css` and use its CSS variables** —
  don't hardcode the hex values. The tokens section maps each value to its `var()`.
- **Liquid Glass vs. capture:** the prototype paints panels with a solid navy fill
  (`rgba(36,40,58,0.72)`) instead of the DS's `backdrop-filter` glass, because
  `backdrop-filter` inside a scrolling container is dropped by headless
  screenshot/PDF/PPTX capture. In the real app (no capture constraint) use the DS
  `.lw-glass` recipe (blur 22px + saturate 160% + specular top edge) over the
  `.lw-aurora` field for the true look.
- **Config code block:** render each TOML line as its own block element. A single
  `<pre>` populated from JSX/templated text can lose newlines — the prototype
  splits lines deliberately.
- **Snapshot vs config gap:** the dashboard shows sensor types, zone modes/members,
  and per-display blank_mode/controllers/zone/rule that are **not** in
  `StateSnapshot` today. Serve the parsed `Config` (or extend the snapshot) — see
  Backend Integration. This is the one required backend addition beyond proxying.
- dormant is **pre-alpha**; keep the "pre-alpha" framing in the UI and bind the
  server to loopback / document the lack of auth.
