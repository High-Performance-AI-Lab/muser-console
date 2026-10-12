# OpenTelemetry collector compatibility

Where the console's data sources map onto collector receivers, and — more
usefully — where they do not. This is a compatibility note, not a plan to
adopt OTel: the engine is never instrumented from here, and native
OTLP/handoff tracing is a future engine feature that is out of scope for
this repository. The position stated in the README stands: OTel is
consumed at collector level only, later.

## The four sources

The console reads four things. Only two of them a stock collector can read.

| source | shape | collector receiver |
|---|---|---|
| engine `GET /metrics` | Prometheus text 0.0.4 | `prometheus` — works today |
| agent `GET /metrics` (gx10, mac exporters) | Prometheus text 0.0.4 | `prometheus` — works today |
| engine `GET /snapshot` | `MetricsSnapshot` JSON | **none** — see below |
| engine `GET /telemetry` (SSE) / `GET /stream` (WS) | snapshot envelopes + section deltas | **none** — see below |

### What works as-is

Both Prometheus endpoints are ordinary exposition and need no adapter. A
scrape config per engine and per agent is enough:

```yaml
receivers:
  prometheus:
    config:
      scrape_configs:
        - job_name: muser-engine
          scrape_interval: 1s
          authorization:
            type: Bearer
            credentials_file: /etc/muser/api-key   # mode 0600
          static_configs:
            - targets: ["127.0.0.1:4949"]
              labels: { instance_name: gx }
        - job_name: muser-agent
          scrape_interval: 1s
          static_configs:                          # exporters need no auth
            - targets: ["127.0.0.1:9707"]
              labels: { instance_name: gx, agent: gx-gpu, provenance: agent-measured }
```

Two engine-side details a collector operator must know, both documented in
[engine-contract.md](engine-contract.md):

- `completion_traffic_tok_s_10s` carries **no `muser_` prefix** while every
  sibling does. Any dashboard or rule that assumes the prefix will miss it.
- The engine's exposition carries **no honesty, node, or instance labels**
  at all. `instance_name` above is a label the *scraper* attaches, not
  something the engine asserts.

### What does not work, and why it matters

**The snapshot has no receiver.** `MetricsSnapshot` JSON is where the
honesty sidecar lives, and it is the only place several phase-3 series
exist at all — `wire.requests_per_s`, `wire.ingress_gbps`, the cumulative
DFlash counters, transfer receipts, and the disaggregation counters are
absent from the Prometheus exposition. There is no stock receiver that
reads it. The options, in order of how much they preserve:

1. A custom receiver (or a `prometheus`-shaped shim) that fetches
   `/snapshot`, reads `_honesty`, and drops or labels each field
   accordingly. This is the only option that keeps provenance.
2. `filelog` over a snapshot dump. Works, loses nothing structurally, but
   needs something to write the dumps and re-introduces a staleness
   question the console does not otherwise have.
3. Scrape only `/metrics` and accept a smaller, measured-only dataset.
   Honest, just narrower.

**The live streams have no receiver.** SSE and WebSocket are not
receiver-shaped. Nothing about the state plane belongs in a collector
anyway: it is current-value data with a 4-second staleness contract in the
browser, and buffering it through a pipeline would break exactly the
property the dashboard depends on. If OTel ever ingests this, it should be
because the engine emits OTLP natively — an engine change, not a console
one.

## The honesty problem, stated plainly

A collector pipeline flattens everything into samples with labels. That is
fine for measured data and wrong for everything else, because the engine's
`mock` and `target` tags live in `_honesty` in the snapshot, not in the
exposition. A pipeline built only on the `prometheus` receiver therefore
*cannot* mislabel mock data — the engine never exports it — but it also
cannot tell you that `nodes`, `wire.egress_gbps`, and the derived joules
and GFLOPs figures exist and are unmeasured. The absence is silent.

If this repository's data ever flows into a collector, provenance must
ride along as a label from the first hop, not be reconstructed later:

- `provenance="measured"` — engine-reported live counter.
- `provenance="agent-measured"` — sidecar exporter (NVML, powermetrics).
- `provenance="target"` — documented release threshold, never observed.
- Never emit a series for a `mock`-tagged or absent field. Absence is the
  honest record; a zero is not.

That is the same rule the console's own store applies (`source` and
`honesty` columns on every row), so a collector-based pipeline that adopts
it stays consistent with the console rather than contradicting it.

## Why the console does not simply become a collector

The console already does the two things a collector would do for it —
scrape on a fixed tick, and retain with downsampling — in about a thousand
lines with one embedded dependency, and it does one thing a collector
cannot: it reads the honesty sidecar and refuses to store what the engine
did not measure. Adding a collector in front would mean either losing that
distinction or reimplementing it inside a custom receiver. The trade only
becomes worthwhile when there is a second consumer of this telemetry, and
at that point the right seam is the console exporting OTLP, not the
console consuming it.
