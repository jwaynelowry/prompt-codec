# prompt-codec dashboard v2 — Mission Control

Date: 2026-07-19
Status: visual design approved by Wayne against the interactive preview
(`.superpowers/brainstorm/93441-1784489385/dashboard-v2-preview.html` — the
authority for look/layout); full prompt-text history storage explicitly
approved by Wayne 2026-07-19. Role decision: savings monitor + history,
strictly read-only.

## Goals

Replace the draft dashboard with the approved Mission Control design:
glanceable lifetime savings, savings-over-time charts, source/proxy
breakdowns, and a browsable request history where any request opens a
side-by-side before/after prompt diff. No operational controls.

## Non-goals

- No config mutation, no auth beyond the existing host guard, no multi-user.
- No external assets ever (single self-contained HTML served by the proxy).
- No change to compression, forwarding, or telemetry-counter semantics.

## Data layer (extends the existing SQLite, same degrade-never-break posture)

New tables in the Feature-1 DB (schema_version bump 1→2 with the documented
migration branch in `DiskTier::open`):

- `requests(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER, port INTEGER,
  route TEXT, model TEXT, before_tokens INTEGER, after_tokens INTEGER,
  source TEXT CHECK(source IN ('llm','cache','rules')), duration_ms INTEGER,
  upstream_cached INTEGER, before_text TEXT, after_text TEXT)` — one row per
  compressed request, written best-effort after the compression stage
  (before/after text per Wayne's approval; `model` from the request body's
  model field when present).
  Cap: `dashboard.max_history` (default 5000) rows per DB, pruned
  oldest-first with the same probabilistic + capped-delete pattern as
  rewrites pruning.
- `savings_hourly(hour_ts INTEGER, port INTEGER, requests INTEGER,
  before_tokens INTEGER, after_tokens INTEGER, upstream_cached INTEGER,
  PRIMARY KEY(hour_ts, port))` — UPSERT-incremented per request; retention
  2200 rows/port (~90 days), pruned with the same pattern.

All writes are fire-and-forget on the existing degrade path (a broken disk
loses history, never a request). When `cache.persist: false`, history and
charts are session-only-empty (documented); the in-memory recent ring
(cap raised to 50) still feeds the table view.

## Endpoints (all read-only, host-guarded, same router layer)

- `GET /dashboard` — the v2 page (`include_str!`, self-contained).
- `GET /dashboard/data?range=24h|7d|30d|all` — one JSON payload:
  `totals` (this port, as today), `all_ports` (every `totals_json:*` row +
  aggregate), `series` (hourly buckets for the range: saved tokens +
  upstream cached per bucket), `breakdown` (source shares llm/cache/rules
  and per-port saved totals, computed from `savings_hourly`/`requests`),
  `recent` (latest 50 from `requests` — numbers only, no text).
- `GET /dashboard/request/{id}` — `{before_text, after_text, meta…}` for the
  diff pane, fetched lazily on row click.
- Existing `/health` unchanged.

## Frontend (the approved preview, made live)

Single `dashboard.html` v2: dark Mission Control theme matching the preview's
palette/layout exactly — header with proxy identity + live/warm badge, five
stat cards, savings-over-time SVG area chart with 24h/7d/30d/all range
switcher (green = saved tokens, blue dashed = upstream cache reads),
"where savings come from" + "by proxy" bar panels, request-history table
(click row → fetch diff → red/green side-by-side panes with meta line),
restyled test drive (unchanged behavior). Hand-rolled SVG, vanilla JS,
2 s polling for stats/recent; diff fetched on demand. Escape-safe DOM
building throughout (textContent — prompt text is user/model content).

## Config additions

`dashboard.max_history` (default 5000). KNOWN_SECTIONS gains a `dashboard`
section; anti-drift test covers it.

## Testing

- Unit: hourly bucket upsert math, requests prune cap, migration v1→v2 on an
  existing DB file (opens, adds tables, preserves rewrites/meta).
- Integration: /dashboard/data shape incl. range filtering and all_ports;
  /dashboard/request/{id} returns stored text; pagination cap; host guard on
  the new endpoints; persist=false → empty history but working page;
  streaming-fidelity tests untouched.
- Live acceptance: real request through the GLM demo appears in history with
  working diff; range switcher renders; both proxies show in "by proxy".

## Acceptance

1. Gates green (test x2/clippy/fmt); existing 128 tests untouched.
2. The served page visually matches the approved preview (side-by-side check).
3. Click-to-diff works on real traffic; history survives proxy restart.
4. v1 DBs migrate in place without losing rewrites or totals.
