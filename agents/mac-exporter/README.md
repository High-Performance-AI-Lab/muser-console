# mac-exporter

Apple Silicon host power, exposed as Prometheus text for the muser console to
scrape. The engine tags per-node power `mock` because no data source exists
engine-side; this sidecar publishes the one the host actually has —
`powermetrics` — so the console can render it with an **agent-measured** badge,
visibly distinct from engine-reported `measured` data.

```
mac-exporter [--listen <addr>] [--sample-ms <ms>]
```

| flag | default | meaning |
|---|---|---|
| `--listen` | `127.0.0.1:9708` | bind address (`ip:port`, no name resolution) |
| `--sample-ms` | `200` | `powermetrics -i`: the width of one sample window |

The port differs from `gx10-exporter`'s `9707` so both can run on one host
during development. There is no config file and no key file: this process holds
no credential.

## What it publishes

`GET /metrics` → `text/plain; version=0.0.4; charset=utf-8`.

| series | type | source |
|---|---|---|
| `muser_agent_up{agent="mac"}` | gauge | 1 when this scrape served a real reading, else 0 |
| `muser_agent_scrape_duration_seconds{agent="mac"}` | gauge | wall clock spent on this scrape |
| `muser_host_package_power_watts{host="…"}` | gauge | combined package power |
| `muser_host_cpu_power_watts{host="…"}` | gauge | `cpu_power` sampler |
| `muser_host_gpu_power_watts{host="…"}` | gauge | `gpu_power` sampler |

Every emitted metric carries `# HELP` and `# TYPE`. A metric with no value is
not emitted at all — not as an empty family, not as a zero — so "absent" always
means "this exporter did not measure it".

Two things are stated in comments rather than series:

- the exact `powermetrics` line each number was read from, because the tool
  spells package power differently across Macs and a reader should not have to
  guess what the number is a sum of;
- the sample's own completion time and age, because publishing an age as a
  fresh timestamp is the one lie a cache can tell.

`GET /healthz` → `{"ok":true}`. That is liveness of *this process* only; whether
`powermetrics` is readable is `muser_agent_up`.

## Honesty rules this exporter follows

- **A failed scrape publishes nothing.** No zero, no remembered value, no
  estimate: `muser_agent_up 0` and no power series at all.
- **A field the output did not contain publishes nothing**, while the fields it
  did contain still publish.
- **One long-lived sampler produces 200 ms frames.** Current macOS takes about
  1.1 s to initialize a one-shot `powermetrics` process even with `-i 200`, so
  spawning on every scrape cannot truthfully deliver one-second native history.
  The supervised process is line-buffered, frame-delimited by the tool's real
  `Sampled system activity` header, and restarted if it exits or stalls.
- **A reading is reused for at most 500 ms** (`MAX_READING_AGE`). Several
  scrapers may share one completed frame, which retains its original completion
  time and is served with its own age. Past that window it is not served; the
  exporter waits for a newer frame, and if none arrives the result is a gap.
- **An HTTP scrape waits at most 900 ms** for a completed frame. The console
  allows 950 ms, leaving a bounded margin inside its one-second history tick.
- **The reason for a gap is logged once per up/down transition**, not once per
  scrape, so a machine that is simply not root writes one line, not one a
  second forever.
- **Nothing read from the environment reaches an HTTP body.** The exposition's
  failure comment is a fixed phrase the exporter wrote itself; the child's
  stderr goes to the exporter's own stderr log.

## Root, and the absence of escalation

`powermetrics` requires root:

```
$ powermetrics --samplers cpu_power -n 1 -i 200
powermetrics must be invoked as the superuser
$ echo $?
1
```

This exporter **never escalates**: no `sudo`, no setuid helper, no privileged
shim. Run it as root (a launchd job, say) and it reads the counters; run it as
anyone else and every scrape reports `muser_agent_up 0`, which is the true
statement about a process that cannot see them. Running unprivileged is a
supported, well-defined state, not an error path bolted on.

## Security

The exporter has **no authentication**, deliberately. It holds no credential
and serves nothing but host power numbers, so a key here would be a secret to
protect rather than a secret protecting something — and the console would then
have to hold and send it. **The console must never send this exporter a
credential**; there is nothing here for one to unlock.

What bounds exposure is the bind address. The default is loopback. A wider bind
is allowed — the console may scrape across the network — but only as an
explicit `--listen`, and the exporter announces it at startup. Put it on a
trusted network segment; treat the power series as it is, host telemetry
readable by anyone who can reach the port.

## Why the text format, not `--format plist`

The plist output is machine-readable, which argues for it, but it is a
NUL-separated XML property list. Reading it means either adding an XML
dependency (outside this project's dependency budget: the exporter's whole tree
is axum + tokio + libc, all already in the workspace lock) or hand-rolling an
XML parser around plist key names that would have to be *guessed*, since
`powermetrics` cannot be run on the build host to find them out. A parser built
around invented key names is exactly the kind of plausible fiction this
repository exists to avoid.

The text format's power lines are one grammar rule wide — `<label>: <number>
<unit>` — so `src/parse.rs` is a recognizer, not a schema: it reads the lines it
recognizes, ignores every other byte, and never assumes a line is present. If
this machine spells a label differently, the corresponding series is simply
absent, which is the honest report of a field the exporter could not read. The
unit (`mW` or `W`) is taken from the line itself and converted, never assumed.

## Fixture honesty

`tests/fixtures/powermetrics-cpu-gpu-power.structure.txt` is a **structure
fixture, not a capture**. It is hand-written to exercise the parser's grammar,
its numbers were chosen for their formats and are measurements of nothing, and
the file says so at the top at length. No test treats a value in it as a fact
about hardware; the assertions are all of the form "these bytes map to this
field".

It is hand-written because `powermetrics` refuses to run without root and this
project does not escalate privileges to manufacture a fixture. If an operator
captures real output under the accelerator discipline, it lands beside this file
with a PROVENANCE entry — and this file stays exactly what it is.

The failure path, by contrast, *is* tested against reality:
`tests/server.rs::the_real_powermetrics_path_reports_what_it_actually_got` runs
the real command through the real code path and asserts the exposition is honest
about whatever came back.

## Tests

```
cargo test -p mac-exporter
```

Everything runs unprivileged, and nothing in the suite needs a Mac with power
counters — or a Mac at all. The one real-command test is explicit and
feature-gated; on macOS it may be added with
`cargo test -p mac-exporter --features real-powermetrics-tests`. It asserts an
honest response shape rather than a power value and never escalates itself.
