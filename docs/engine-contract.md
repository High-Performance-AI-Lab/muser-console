# muser engine contract notes (read-only source: muser@753c2a5)

Distilled from `docs/telemetry.md`, `docs/metrics-schema.{md,json}`,
`docs/one-button-onboarding.md`, `crates/muser-server/src/{axum_httpd,metrics,nodes_api}.rs`,
and `web/muser-dashboard.html` in the engine repo. The full pin is
`753c2a5cf6797db82e6c2e9ff77cc4a0a552966b`; the historical pins this one
supersedes are `d4fe7159` and `42e9af6e`. This file is console-side
documentation; the engine repo is authoritative and read-only.

## Endpoints the console consumes

| route | transport | auth (engine-side) | notes |
|---|---|---|---|
| `GET /snapshot` | JSON | management read | bare `MetricsSnapshot`, no envelope |
| `GET /metrics` | Prometheus text 0.0.4 | management read | measured-only subset, see below |
| `GET /telemetry` | SSE | management read | 1 Hz, `event: snapshot`, data `{"v":1,"type":"snapshot","seq":N,"t":uptime_s,"data":<snapshot>}`; no retry hint, no comments |
| `GET /stream` | WebSocket | dashboard cookie OR single-use `?ticket=` (30 s, from `POST /v1/ws-tickets`) | hello `{"v":2,"type":"hello","schema":"muser.telemetry.v2","snapshot_interval_s":10,"ping_interval_s":5}`; 1 Hz ticks: full `snapshot` at seq%10==0, else `section_delta` (top-level-key whole-section diff); WS Ping payload `"muser"` at seq%5==0; full snapshot is the only resync |
| `GET /v1/nodes` | JSON | management read + bind/peer policy | `{nodes:[...], running_job, registry, daemon_probe_timeout_ms}`; probes are parallel and `daemon_alive` is a live, at-most-two-address 1 s TCP probe of port 29591. A healthy/enrolled registry entry older than enrollment v2 is reported as `needs-reenrollment` rather than silently treated as current. |
| `POST /v1/nodes` | JSON | management write + bind/peer policy | Content-Type must be exactly `application/json`; 202 `{name, progress}`, 409 single-job-slot, 415/400/503 |
| `GET /v1/nodes/{name}/progress` | SSE | management read | replay-then-tail of `muser.node-progress.v2` lines (`data:` only frames), `: ping` every 10 s, terminal `retry: 3600000` + `event: end`; client dropped after 5 s of backpressure |
| `POST /v1/dashboard/login` | JSON | TLS + bearer + exact Origin | 400 `tls_required` on plain HTTP; mints in-memory 1 h session cookie `muser_session` (Secure/HttpOnly/SameSite=Strict) + `{"csrf_token","expires_in":3600}` |
| `POST /v1/ws-tickets` | JSON | management write | `{"ticket","expires_in":30,"single_use":true}` |

Auth: `Authorization: Bearer <key>` — exact `"Bearer "` prefix, key = engine
`--api-key-file` bytes trimmed of ASCII whitespace, constant-time compare.
"Management read" = bearer OR Host-bound cookie; "write" additionally needs
exact `Origin` + `x-csrf-token` when using the cookie. Without an api-key
file the engine rejects all management routes. 401 shape:
`WWW-Authenticate: Bearer` + `{"error":{"type":"authentication_required","message":"a valid bearer API key is required"}}`.
All errors use the envelope `{"error":{"type":...,"message":...}}` in
that source order. The engine workspace enables serde_json
`preserve_order`; the console must do the same for console-owned error and
ticket bodies rather than relying on alphabetic map order.

For `GET`/`POST /v1/nodes`, a non-loopback peer is rejected unless the
engine itself was admitted in LAN mode. The engine reads the actual socket
peer and ignores forwarding headers. `/v1/nodes/{name}/progress` separately
uses management-read auth; it replays a live job or retained transcript and
returns 404 if neither exists. The console does not broaden any of these
policies: it reaches a loopback engine as a loopback peer and injects only
that instance's server-side key.

## MetricsSnapshot: published schema and current runtime

Top-level required: `schema_version, generated_at, uptime_s, nodes, kv,
economics, transfers, wire, sessions, tricks`. Optional: `engine_clock_s,
cluster, specdec, _events, _honesty, _telemetry_viewers, _telemetry_requests,
_active_connections, _queue_depth, _overload_rejections, _lock_recoveries,
_decode, _remote, _phases`. Nearly every object is
`additionalProperties:false`; the only open shapes are `_honesty`,
`SessionEvent.detail`, and `section_delta.data`. `generated_at` /
`_events[].ts` are RFC3339 (format not auto-enforced by validators — parse
explicitly in tests).

The published file has no blob drift: `docs/metrics-schema.json` is blob
`fa9511c` at all three historical/current pins and remains imported verbatim
as `schema/metrics-schema.json`. Current runtime serialization deliberately
exceeds that still-strict document in exactly these measured fields consumed
by this console:

- each retained transfer receipt adds `active_drain_gbps` and
  `_active_drain_ns`; the rate is installed payload bits divided by the
  receiver's active post-first-byte segment-drain span, distinct from the
  end-to-end `throughput_gbps`;
- top-level `_dflash_acceptance` carries process-monotonic drafted, accepted,
  round, finished-disabled, disabled-position-sum, and gate-closure counters,
  plus last-value effective draft sink/window geometry.

`schema/runtime-extensions-schema.json` is the console-owned companion
contract. A current live snapshot must validate against it. For conformance
to the published contract, tests remove only those three named additions
(`_dflash_acceptance` and the two transfer drain fields) and validate the
remaining snapshot against the verbatim import. The imported schema is never
relaxed or rewritten to conceal the discrepancy.

Honesty: single top-level `_honesty` sidecar mirroring the tree with leaves
`"measured" | "target" | "mock"`. Hard facts at the current pin:
`nodes` always mock (nodes[] is always `[]`), `wire.egress_gbps` mock,
`economics.derived.{gflops_avoided,joules_saved}` mock,
`cluster.weights_bytes` measured iff a model is loaded,
`economics.{restore_speedup,derived.seconds_saved}` measured iff a timed
restore happened; everything else measured. `kv.capacity_bytes` is a
constant 0 inside a section tagged measured — do not render it as capacity.
`tricks` is `[]` until a sealed qualification packet exists. Runtime
extension counters are direct engine counters, not console derivations;
last-value DFlash geometry and transfer receipts must not be clock-sampled
into an apparently continuous measurement after their underlying event.

The `_events` member is structurally present but unavailable at this pin. The
engine serializes its in-memory `EventLog`, yet no production inference or
logical-session path appends to that log. Phase 2/3 live acceptance exercised
two genuine logical-session create/read/delete lifecycles per engine and eight
model-backed completions; both snapshots still contained `"_events":[]`.
The console must retain that empty source state. It must not derive lifecycle
events from polling transitions, turn the absence into a numeric zero claim,
or carry rows across an engine restart.

The v1 fleet event view therefore merges only each poller's current `_events`
array, marks the source `live`/`stale`/`disconnected`, and leaves the row list
empty against this pin. It does not turn connection transitions into events.
Persisting event rows is post-v1 because no live producer exists yet and a
store would otherwise make stale process-local rows look durable.

## Prometheus exposition (`GET /metrics`) — exact series

`completion_traffic_tok_s_10s` (gauge, **no muser_ prefix** — known wart),
`muser_queue_depth`, `muser_overload_rejections_total`,
`muser_completion_tokens_total`,
`muser_ttft_milliseconds{quantile="0.50"|"0.95"}`,
`muser_itl_milliseconds{quantile=...}`, `muser_dflash_acceptance_ratio`,
`muser_phase_seconds_total{phase=...}` / `muser_phase_samples_total{phase=...}`
(phases: queue, prefill, sampling, grammar, detokenization, enqueue_write,
dflash_draft, dflash_target_verify), `muser_request_decode_tok_s`,
`muser_decode_packed_batches_total`, `muser_decode_packed_rows_total`,
`muser_decode_batch_width_last`.

**No honesty, node, or instance labels.** Mock/target sections are simply
absent from the exposition. Fields the phase-3 history plane needs that are
NOT here (requests/s, ingress_gbps, dflash drafted/accepted cumulative,
transfer stats, session counts) exist only in the snapshot JSON — hence the
console samples `/metrics` and `/snapshot` on the same tick and labels each
stored series with its source and honesty tag.

## What a faithful reverse proxy must do

- The dashboard page is strictly same-origin: it derives every URL from
  `location.origin` with root-anchored paths (`/snapshot`, `/telemetry`,
  `/v1/dashboard/login`, `/v1/dashboard/session`, `/v1/dashboard/pairings*`,
  `/v1/fleet`, `/v1/history/*`, `/v1/nodes`, and
  `/v1/nodes/<name>/progress`). It never opens a WebSocket. Transport order:
  one-shot `/snapshot` → SSE
  `/telemetry` → 3 s `/snapshot` polling fallback. When it holds a bearer in
  memory (plain-HTTP mode) it skips SSE entirely (EventSource cannot carry
  headers) and polls, with the badge reading live telemetry unless that poll
  is itself recovering a dropped stream.
- Stale watchdog: panels dim after 4 s without a frame; hard reconnect after
  15 s. **Any SSE buffering in the proxy makes the page cycle stale→restart.**
  Stream SSE bodies chunk-by-chunk, never coalesce, no idle timeout on
  `/telemetry`, `/stream`, `/v1/nodes/{name}/progress`.
- Do not rewrite `Content-Type` on proxied POSTs (engine requires literal
  `application/json`). Pass response status/bodies through untouched — the
  page keys behavior off 401/202/409 and the error envelope.
- Browser-facing HTTPS may negotiate HTTP/2, but the console's engine clients
  deliberately use HTTP/1.1. Request versions are hop-by-hop, so the proxy
  normalizes each upstream request to HTTP/1.1; forwarding the browser's
  HTTP/2 version marker would fail before reaching the engine. Header and
  response-body parity are unchanged.
- Engine sets no CORS, no compression, and reads the direct socket peer for
  the nodes-route loopback policy (no X-Forwarded-For support). Cookie
  sessions are bound to the engine's own Host+TLS — they cannot survive a
  credential-isolating proxy, which is why the console terminates auth
  itself (bearer parity) and injects the engine key server-side.
- Console-only remote-browser routes do not extend the engine contract.
  `/v1/dashboard/session` restores the CSRF value for an existing Host-bound
  HttpOnly session after refresh. `/v1/dashboard/pairings` mints a two-minute,
  one-use LAN credential as a packed QR matrix; `/revoke` removes it; and
  `/redeem` accepts it only under exact HTTPS Origin/Host and from a direct
  local peer. Pairing tokens are held only as SHA-256 digests, never replace
  the reusable console key, never bypass certificate trust, and never travel
  in a request URL.
- A dropped downstream client must drop the upstream connection promptly
  (in-flight generation is cancelled on disconnect engine-side).

The SSE and WebSocket telemetry protocol is unchanged between the historical
`42e9af6e` recheck and the current pin: SSE remains a full snapshot every
second; WebSocket remains one-second section deltas, a ten-tick full-snapshot
resync, and five-tick ping cadence. The new snapshot fields ride those
existing envelopes; there is no new event type and the proxy must not invent
one.

## Fixture reality

The historical qualification artifacts contain
`RESULT.json → .muser.telemetry_delta`
(`muser.request-telemetry-delta.v1`). Phase 1 subsequently captured literal
live `/snapshot` and `/metrics` bodies, real SSE timing, sanitized network
classifications, DOM parity exports, and screenshots under
`fixtures/captures/`. Integration product-plane stubs replay those literal
wire bodies; historical result summaries are not reformatted into snapshots.
The full attempt ledger is in `docs/acceptance-phase1-2026-08-23.md`.
