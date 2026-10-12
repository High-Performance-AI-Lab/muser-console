# muser-console API surface

The console's own HTTP surface, as served by `crates/console-server`. Two
kinds of routes exist: **console-terminated** routes the console answers
itself, and **proxied** routes it forwards to a configured engine instance
with the browser's credentials stripped and that instance's engine key
injected server-side. The engine contract being proxied is documented in
[engine-contract.md](engine-contract.md); this file covers only what the
console adds.

## Auth model

- Loopback HTTP browser → console: `Authorization: Bearer <console access
  key>` on every authenticated route. The dashboard keeps that bearer in page
  memory only and polls snapshots because browser `EventSource` cannot attach
  it. Exact engine parity is retained: `"Bearer "` prefix, constant-time key
  comparison, and the engine's byte-exact 401 on failure
  (`WWW-Authenticate: Bearer`,
  `{"error":{"type":"authentication_required","message":"a valid bearer API key is required"}}`).
- HTTPS browser → console: the access key is sent once to
  `POST /v1/dashboard/login`, with exact HTTPS `Origin` and `Host`. The console
  returns a random one-hour `muser_console_session` cookie
  (`Secure; HttpOnly; SameSite=Strict; Path=/; Max-Age=3600`) and a random CSRF
  token. The cookie authorizes reads; every cookie-authenticated mutation,
  including ticket minting, also requires `x-csrf-token`. Sessions are bound
  to the login authority. A non-loopback listener does not accept a reusable
  console bearer on ordinary API routes.
- Console → engine: the target instance's engine key is injected; the
  browser's `Authorization`, `Cookie`, and `x-csrf-token` headers are
  stripped before forwarding. Engine keys never reach the browser; the
  console key never reaches an engine. Keys never appear in URLs, logs, or
  error bodies.
- `/stream` (WebSocket) is authenticated by a single-use console ticket
  (`?ticket=`), not a bearer — see WS tickets below.
- `POST /v1/dashboard/login` on a plain-HTTP listener always answers the
  engine's exact 400 `tls_required` error.

## Listener and upstream TLS

Loopback console HTTP remains valid. A non-loopback `listen` requires both
`tls_cert` and `tls_key`; a half-pair is refused. The private key must be a
regular, non-symlink file with exact mode 0600 and is opened with `O_NOFOLLOW`.
The console reads both PEM files at startup and terminates TLS itself.

Engine `base_url` accepts `https://` everywhere. `http://` is accepted only
when the authority uses a literal loopback IP (`127.0.0.0/8` or `::1`);
`localhost`, wildcard, LAN, and DNS authorities are refused for plaintext
engines. An HTTPS instance may set `ca_file`; without it, platform roots are
used. rustls performs normal certificate-chain and SAN validation. Every
instance owns a separate client and trust store, so a custom CA for one
engine cannot authorize another engine or an agent. Agents use a separate,
keyless client and platform roots for HTTPS.

## Error shapes

All console-generated errors use the engine's envelope
`{"error":{"type":"<type>","message":"<message>"}}`:

| status | type | when |
|---|---|---|
| 401 | `authentication_required` | missing/wrong console bearer; missing, bogus, reused, or **cross-instance** WS ticket |
| 404 | `not_found` | `/i/{name}/...` where `{name}` is not a configured instance (message: `unknown instance`) |
| 502 | `upstream_unreachable` | the target instance did not answer (connect failure or timeout); message `instance '<name>' is unreachable` |
| 400 | `invalid_request_error` | unknown query parameter on `/stream`; a malformed history query (see below) |
| 400 | `tls_required` | `POST /v1/dashboard/login` over plain HTTP |
| 400 | `invalid_request_error` | conflicting HTTP/2 authority and `Host` |
| 403 | `invalid_origin` | HTTPS login without an exact HTTPS Origin/Host match |
| 403 | `csrf_required` | cookie-authenticated mutation without the session's CSRF token |
| 503 | `history_unavailable` | a history route on a console with `history.enabled = false` |

Errors from an engine (including engine 401s) pass through untouched —
status, body, and headers, minus hop-by-hop headers and `Set-Cookie`.
Unregistered paths get axum's default empty-body 404 (engine parity; there
is no static-file fallback).

## Console-terminated routes

| route | auth | behavior |
|---|---|---|
| `GET /`, `GET /dashboard` | none | serves `ui/muser-dashboard.html` byte-exact, `Cache-Control: no-store` |
| `GET /healthz` | none | `{"ok":true}` |
| `GET /v1/fleet` | loopback bearer or HTTPS session | fleet listing, below |
| `GET /v1/history/{name}/series` | loopback bearer or HTTPS session | the static series catalog, below |
| `GET /v1/history/{name}` | loopback bearer or HTTPS session | stored history for that instance, below |
| `POST /v1/dashboard/login` | exact HTTPS Origin/Host + console bearer | mints the HTTPS session; 400 `tls_required` on HTTP |
| `POST /v1/ws-tickets`, `POST /i/{name}/v1/ws-tickets` | loopback bearer or HTTPS session + CSRF | mints a console WS ticket, below |

### `GET /v1/fleet`

```json
{"instances":[
  {"name":"gx","authority":"127.0.0.1:4949","default":true,
   "agents":[{"name":"gx-gpu","kind":"gx10","state":"live"}]},
  {"name":"mac","authority":"127.0.0.1:4950","default":false,"agents":[]}]}
```

Instances appear in config order. Exactly one is `default: true` — the
first, the one behind the root-anchored routes. Authorities only; no keys.
A UI that gets a non-200 here falls back to the phase-1 single-instance
root-anchored behavior.

`agents` lists the sidecar exporters configured for that instance, in config
order, and is always present (empty when none). `state` is derived only from
what the sampler's last scrape did:

| state | meaning |
|---|---|
| `live` | the exporter answered the last scrape |
| `unreachable` | it did not (connect failure, timeout, non-200, unreadable body) |
| `unknown` | no scrape has completed yet — including on a console whose history plane is disabled, where the sampler never runs |

Nothing here is inferred from configuration: a configured agent is not a
running one. `live` describes the exporter, not its data source — an
exporter that answers with `muser_agent_up 0` is reachable and stores
nothing, and the gap in its series is what says its source was down.

### Live fleet event view

The dashboard merges only the `_events` arrays in the fleet pollers' current
snapshots. Every row is labeled with instance and session id. Parseable
timestamps sort newest-first; equal timestamps and unparseable timestamps
retain source order. Identical rows are deduplicated only within one
instance, because the same event on two engines can be real twice.

Each source is visibly `live`, `stale`, or `disconnected`. Those labels are
poll state, not invented event rows. The UI never derives connectivity or
session events, never fills absent event detail with zero/default values, and
does not persist rows. A restarted instance replaces its snapshot and
therefore cannot inherit the prior process's event array. Persisted event
history is explicitly post-v1.

## Proxied routes

### Per-instance namespace

Every configured instance is served under `/i/{name}/...`. The upstream
path is the original path with the `/i/{name}` prefix stripped (e.g.
`GET /i/gx/snapshot` → `GET {base_url}/snapshot`). Instance names are
validated `[A-Za-z0-9_-]{1,64}` at config load and matched exact-string
(no percent-decoding). Unknown name → 404 `unknown instance`. On
bearer-authenticated routes the bearer check runs before instance
resolution, so unauthenticated requests get 401 regardless of the name.
`/i/{name}/stream` is the exception: its credential is the
instance-scoped ticket itself, checked against the raw name segment, so
an unknown name fails exactly like a bogus ticket (401, never 404) —
there is no unauthenticated name-enumeration oracle on any route.

| route | methods | mode |
|---|---|---|
| `/i/{name}/snapshot` | GET | buffered |
| `/i/{name}/metrics` | GET | buffered |
| `/i/{name}/telemetry` | GET | streaming (SSE, chunk-by-chunk, no idle timeout) |
| `/i/{name}/v1/chat/completions` | POST | streaming (OpenAI-compatible SSE) |
| `/i/{name}/v1/nodes` | GET, POST | buffered |
| `/i/{name}/v1/nodes/{node}/progress` | GET | streaming (SSE) |
| `/i/{name}/v1/ws-tickets` | POST | console-terminated mint |
| `/i/{name}/stream` | WS upgrade | bridge to that instance's `/stream` |

Buffered routes have a 30 s overall timeout; streaming routes bound only
the upstream response head (10 s) and never buffer or time out the body.
`Content-Type` on proxied POSTs passes through byte-exact (the engine
requires literal `application/json`).

### Root-anchored compatibility routes

`/snapshot`, `/metrics`, `/telemetry`, `/v1/chat/completions`, `/v1/nodes`,
`/v1/nodes/{node}/progress`, `/v1/ws-tickets`, and `/stream` behave exactly
as in phase 1, mapped to the **default instance** (the first in config).
The imported dashboard boots against these before the fleet listing loads,
and an old or fleet-unaware page keeps working unchanged.

### Instance isolation

Requests to different instances use independent hyper clients and rustls
trust stores. A dead or hung instance yields a 502
`upstream_unreachable` for its own routes without delaying or failing
requests — including in-flight SSE streams — to other instances. This is
the phase-2 acceptance property and is covered by integration tests
(`tests/fleet.rs`).

## WS tickets and the `/stream` bridge

1. `POST /i/{name}/v1/ws-tickets` (or root `/v1/ws-tickets` = default
   instance) with loopback bearer auth or an HTTPS session plus CSRF returns
   `{"ticket":"<base64url>","expires_in":30,"single_use":true}`. The ticket
   is minted and stored by the console itself, scoped to that instance; no
   engine is contacted at mint time.
2. The browser opens `ws(s)://console/i/{name}/stream?ticket=<ticket>`
   (or root `/stream` for the default instance). Tickets are single-use,
   expire after 30 s, and are consumable **only** by the instance they were
   minted for; cross-instance presentation is rejected as
   `authentication_required` (indistinguishable from a bogus ticket) and
   does not consume the ticket. Any query parameter other than `ticket`
   is rejected 400, mirroring the engine's strict `StreamQuery`.
3. On upgrade the console exchanges its own credentials server-side: it
   POSTs the instance's `/v1/ws-tickets` with the engine key, then connects
   to the engine's `/stream?ticket=...` and forwards frames verbatim in
   both directions. If the exchange or connect fails, the downstream socket
   is closed with code 1008 (`upstream stream unavailable`); the console
   never fabricates telemetry frames.

## History API (phase 3)

The history plane is a second, separate read path. Live tiles keep reading
the state plane directly; charts read only these routes. The two planes
join on the pinned field names in the series catalog, which is what makes
"the chart and the tile are the same number" a checkable property.

Both routes take the console bearer and are instance-scoped: a history
query needs a name to join on, so there is no root-anchored variant.
Resolution order matches every other console route — bearer, then instance
(404 `not_found` / `unknown instance`), then the history plane itself. With
`history.enabled = false` both answer 503:

```json
{"error":{"type":"history_unavailable",
          "message":"the history plane is disabled on this console"}}
```

### `GET /v1/history/{instance}/series`

The static catalog — what the console can store, not what it has stored.

```json
{"series":[
  {"name":"decode_tok_s","kind":"gauge","source":"metrics",
   "honesty_path":null,"unit":"tok/s"},
  {"name":"requests_per_s","kind":"gauge","source":"snapshot",
   "honesty_path":"wire.requests_per_s","unit":"req/s"}
]}
```

`kind` is `gauge` or `counter` and decides aggregation (mean vs. last
value). `source` is `metrics` | `snapshot` | `agent`. `honesty_path` is the
dotted path under the snapshot's `_honesty` sidecar, and is `null` for
`/metrics` series: the exposition carries no honesty labels because it only
ever contains measured series. It is `null` for `agent` series too, for a
different reason — no engine sidecar stands behind them at all; their
honesty is `agent-measured`, written by the sampler.

#### Agent series

The store's key is `(instance, series, ts)`, so a per-device reading carries
its device in the *series name* rather than in a label. The catalog names a
fixed eight devices, `gpu0`…`gpu7`:

| series | exporter metric | unit |
|---|---|---|
| `gpu<i>_utilization_ratio` | `muser_gpu_utilization_ratio{gpu="<i>"}` | ratio |
| `gpu<i>_power_watts` | `muser_gpu_power_watts{gpu="<i>"}` | W |
| `gpu<i>_temperature_celsius` | `muser_gpu_temperature_celsius{gpu="<i>"}` | °C |
| `gpu<i>_memory_used_bytes` | `muser_gpu_memory_used_bytes{gpu="<i>"}` | bytes |
| `gpu<i>_memory_total_bytes` | `muser_gpu_memory_total_bytes{gpu="<i>"}` | bytes |
| `host_package_power_watts` | `muser_host_package_power_watts` | W |
| `host_cpu_power_watts` | `muser_host_cpu_power_watts` | W |
| `host_gpu_power_watts` | `muser_host_gpu_power_watts` | W |

An agent publishing more than eight GPUs gets the first eight stored and one
log line naming how many were dropped; the extra devices are not addressable
and are never folded into another device's series. `gpu8_utilization_ratio`
is an unknown series (400), like any other name the catalog does not hold.

### `GET /v1/history/{instance}`

| parameter | default | meaning |
|---|---|---|
| `series` | the whole catalog | comma-separated series names |
| `from_ms` | `to_ms - 15 min` | inclusive lower bound, unix ms |
| `to_ms` | now | inclusive upper bound, unix ms |
| `step_s` | 1 | bucket width in seconds |

```json
{"series":{"decode_tok_s":{
   "kind":"gauge","source":"metrics",
   "points":[[1755300000000,107.91612745927448]],
   "honesty":"measured","honesty_tags":["measured"]}}}
```

`points` are `[unix_ms, value]` pairs, oldest first. `honesty_tags` lists
every distinct honesty tag the range actually holds, oldest first; it is
empty when the range holds no rows — a series with no data makes no honesty
claim. `honesty` is that tag when there is exactly one, and `null` otherwise
(no rows, or a range whose provenance changed part-way through). A caller
that badges a whole window with one chip must use `honesty`, so a mixed
window gets no chip rather than a claim that was true of only part of it.

Points are **only** what the store holds. Missing ticks are missing points:
nothing is interpolated, carried forward, or zero-filled, and an empty
bucket yields no point at all. A requested series the store has nothing for
comes back with an empty `points` array (and its catalog `kind`/`source`,
so a client can say "no history yet" rather than dropping the panel).

`step_s` above the native 1 s resolution buckets server-side on
epoch-anchored boundaries: gauges average, counters keep the bucket's last
value. `step_s` of 1 returns the stored rows untouched.

Rejections are 400 `invalid_request_error`, naming what was wrong:

| condition | message |
|---|---|
| unrecognized series | `unknown series: 'gpu_util', 'made_up'` |
| `to_ms <= from_ms` | `to_ms must be greater than from_ms` |
| `step_s < 1` | `step_s must be at least 1` |
| non-integer number | `query parameter 'from_ms' must be an integer` |
| unknown parameter | `unknown query parameter 'surprise'` |
| over 20000 points/series | `range would return N points per series at step_s=S; raise step_s (limit 20000)` |

The point cap is refused rather than silently truncated: half a range drawn
as if it were the whole one is a lie the caller cannot see. Query values are
matched byte-exact with no percent-decoding, matching the console's existing
`/stream` query discipline.

## The sampler and the store

One tokio task per instance ticks once a second (`[history]` has no knob for
this — the cadence is part of the contract) and fetches `GET /metrics` and
`GET /snapshot` concurrently over the same hyper client the proxy uses, with
a 2 s per-request budget, the instance's engine key injected server-side,
and identity encoding. The same tick also fetches `GET /metrics` from every
agent attached to that instance, concurrently with the engine's two. Every
source of a tick shares one timestamp so they join exactly.

Everything the sampler does about bad data is *not writing*:

- a fetch, status, or parse failure records nothing for that source this
  tick, and logs once per up/down transition rather than once per tick;
- a field the engine did not report records nothing;
- a field the engine tagged `mock` records nothing — the honesty tag is read
  before the value is, so no path can store one by accident;
- a non-finite number records nothing.

Honesty tags are read from the snapshot's `_honesty` sidecar at the catalog's
documented path, falling back to successively shorter prefixes (a section may
be tagged as a whole). `mock`, or no tag at all, means skip; the one exception
is `_`-prefixed process extensions, which carry no sidecar entry and are
measured per the engine docs. Where a field exists in both planes (queue
depth, overload rejections, decode traffic, dflash accept rate) `/metrics`
wins and the snapshot copy is not stored — one field, one series.

The store is a single sqlite file owned by one writer thread (WAL,
`busy_timeout`, mode 0600); the async side sends batches down a channel and
never touches the connection. A maintenance pass runs at startup and hourly:
rows older than 24 h collapse to 60 s buckets (gauge mean, counter last, on
whole buckets only, idempotently) and rows past `retention_days` are deleted.
Downsampling replaces real samples with their aggregate and retention
deletes; neither pass ever fills a gap.

### Agent scrapes

An agent's scrape follows the same not-writing rules, plus two of its own:

- a device the exporter did not publish, and a single reading whose probe
  failed (the exporter omits that one line and serves the rest), each record
  nothing — the rest of the device still publishes;
- `muser_agent_up 0` is the exporter disclaiming its own data source, so that
  tick stores nothing at all, even if device lines are present.

Stored agent rows carry `source = agent` and `honesty = agent-measured`, and
are written under the **instance's** name so the fleet/instance join keeps
working. `agent-measured` is a different claim from the engine's `measured`
and never collapses into it: the engine never saw these numbers.

### `[[agent]]` config

```toml
[[agent]]
name = "gx-gpu"                    # unique, [A-Za-z0-9_-]{1,64}
base_url = "http://10.0.0.7:9707" # HTTP or HTTPS; HTTPS uses platform roots
instance = "gx"                    # must name a configured [[instance]]
kind = "gx10"                      # "gx10" | "mac"
```

There is deliberately **no `api_key_file`**, and the table rejects unknown
keys, so there is nowhere to put a credential: the exporters serve no
secrets, and the console sends none to them — not an engine key, not its own
access key, no cookie or CSRF token. `kind` is descriptive (it rides
`/v1/fleet` so the UI can name the sidecar) and never changes parsing: the
console stores the series the exporter actually published.

An agent naming an instance the console does not serve is a config error
rather than an orphaned scraper — its samples could never be joined against
anything. Agents are not proxied: no `/i/{name}` route reaches an exporter,
and the browser never talks to one.

An HTTPS agent has no `ca_file`: it uses the platform trust store in the
separate keyless agent client. A custom engine CA is never copied into that
client.

### `[history]` config

```toml
[history]
db_path = "console-history.sqlite"   # anchored against the config directory
retention_days = 7
enabled = true
```

A relative `db_path` resolves against the config directory (never the
process's working directory) and its parents are created at startup. The
store holds the fleet's telemetry and is created mode 0600, like the key
files.

### Numeric fidelity

The console parses engine JSON with serde_json's `float_roundtrip` feature
on. Without it, serde_json's fast float parser is off by one ULP on some
17-significant-digit values, so the console would store a neighbour of the
number the engine reported and the browser — which parses correctly — would
draw a chart that disagrees with its own tile in the last digit. Values
cross the console unchanged, bit for bit, in both planes.
