# gx10-exporter

NVIDIA GPU telemetry the muser engine cannot see, published as Prometheus
text for the console to scrape.

The engine tags its `nodes[]` block `mock` and always serves it empty
(`docs/engine-contract.md`): per-node GPU utilization, power, temperature
and memory have no engine-side data source at all. This exporter is that
data source. It runs as a sidecar process inside the node's existing pinned
container, reads NVML, and the console stores what it scrapes with
`source = agent` and `honesty = agent-measured` — deliberately a different
badge from the engine's own `measured`, because it is a different
measurement made by a different program.

## Running it

```
gx10-exporter [--listen <addr>]     # default 127.0.0.1:9707
```

Two routes, no others:

| route | answer |
|---|---|
| `GET /metrics` | the exposition below, `Content-Type: text/plain; version=0.0.4; charset=utf-8` |
| `GET /healthz` | `{"ok":true}` |

`/healthz` reports **this process**, not the hardware. It stays `ok` while
NVML is unavailable on purpose: whether the GPU source answered is
`muser_agent_up`, and a health check that failed on absent hardware would
make a supervisor restart an exporter that is honestly reporting a gap.

### Bind and auth

The default bind is loopback. The console normally scrapes across the
network, so a wider bind is allowed — but it has to be spelled out with
`--listen`, and the exporter logs what that means when it starts.

**There is no authentication, and that is deliberate.** This process holds
no credential: not the engine's API key, not the console's access key, not
an SSH key. It publishes GPU counters and nothing else, so a bearer check
would protect nothing while giving the console a reason to *send* it a key.
The console's `[[agent]]` config has no `api_key` field for exactly that
reason, and the console never sends a credential to an agent. Access
control for a non-loopback bind belongs to the network layer.

## Exposition

Every family carries `# HELP` and `# TYPE`.

```
muser_agent_up{agent="gx10"}                     1 | 0
muser_agent_scrape_duration_seconds{agent="gx10"}
muser_gpu_utilization_ratio{gpu="0",uuid="…",name="…"}    0..1
muser_gpu_power_watts{gpu="0",uuid="…",name="…"}
muser_gpu_temperature_celsius{gpu="0",uuid="…",name="…"}
muser_gpu_memory_used_bytes{gpu="0",uuid="…",name="…"}
muser_gpu_memory_total_bytes{gpu="0",uuid="…",name="…"}
```

Units are converted where NVML's differ from the series name, and the HELP
text says so: `nvmlDeviceGetUtilizationRates` reports whole percent and is
divided by 100 (no clamping — a value outside 0..1 would mean the driver
said something outside 0..100, and substituting a plausible number is
exactly what this exporter exists not to do); `nvmlDeviceGetPowerUsage`
reports milliwatts and is divided by 1000.

Label values are Prometheus-escaped (`\`, `"`, newline) because device names
and UUIDs come from the driver, not from us.

The exporter publishes every device NVML enumerates. The console caps the
per-GPU series it stores at 8 and logs if more appear; that cap is the
console's, not this exporter's.

## What it will not do

Absence is the honest report, so every one of these is a gap and never a
number:

- **NVML unavailable** (no driver, no library, `nvmlInit_v2` failed): the
  exporter still starts and serves `muser_agent_up 0` — and *only* that.
  No device series, and no scrape duration either: the duration of a failed
  probe describes nothing about the node.
- **One field's probe failed**: that field is omitted for that device while
  the device's other fields still publish, and the other devices are
  untouched.
- **No device reported a field at all**: the whole family is omitted, `HELP`
  and `TYPE` included — a header with no samples under it announces a series
  the exporter does not have.
- **A driver string is missing or is not UTF-8**: the `uuid` / `name` label
  is absent rather than empty or guessed at. The `gpu` index label is always
  present because it is the enumeration position, not a probe.
- **A non-finite reading**: omitted like a failure. `NaN` is a
  number-shaped hole.

Nothing is ever zero-filled, carried forward from a previous scrape, or
estimated. `muser_agent_up 1` means the source answered *this* scrape.

## NVML is loaded at run time, never linked

The build host has no CUDA toolchain, and the exporter has to build and
start on machines with no NVIDIA driver at all. So `libnvidia-ml` is never
a link-time dependency: `src/nvml.rs` opens it with `dlopen`
(`libnvidia-ml.so.1`, then `libnvidia-ml.so`) and resolves ten entry points
with `dlsym` — `nvmlInit_v2`, `nvmlShutdown`, `nvmlDeviceGetCount_v2`,
`nvmlDeviceGetHandleByIndex_v2`, `nvmlDeviceGetUUID`, `nvmlDeviceGetName`,
`nvmlDeviceGetUtilizationRates`, `nvmlDeviceGetPowerUsage`,
`nvmlDeviceGetTemperature`, `nvmlDeviceGetMemoryInfo`.

Resolution is all-or-nothing: a missing symbol fails the load rather than
becoming an absent field at scrape time, so "this exporter is talking to the
wrong library" can never be mistaken for "the driver declined to answer".
The load is retried on every scrape, because a driver can appear after the
exporter does (container restart, module load).

Dependencies are the workspace's existing ones — axum, tokio, serde_json —
plus `libc` for `dlopen`/`dlsym`. No CUDA crate, no Prometheus client
crate.

## Tests

`cargo test -p gx10-exporter`. Reading NVML needs an NVIDIA GPU, which no
machine in this project has, so the source sits behind a `GpuSource` trait
and the tests drive a `RecordedSource` instead. **The values those tests use
are structural stimulus, not measurements**: no NVML capture exists here,
none is invented, and no file in this repository claims to be one. What the
tests assert about values is the honesty rules above — that a reading the
source did not produce never reaches the wire.

`RecordedSource` is test-only by construction: the binary can build the NVML
source and nothing else, and no flag, config key, or environment variable
selects anything different.
