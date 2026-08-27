# Agent deployment, upgrade, and rollback

Agents are keyless measurement sidecars. They never receive a console or
engine credential, never invoke `sudo`, and never read a password. Installation
privilege belongs to the operator or service manager, outside the process.

## macOS power agent

Build on ARM64 macOS with the pinned toolchain and locked graph:

```sh
cargo build -p mac-exporter --release --locked
shasum -a 256 target/release/mac-exporter
```

For an acceptance run, the operator starts the already-built binary exactly
as root and stops it after capture:

```sh
sudo -- /opt/muser-console/bin/mac-exporter \
  --listen 127.0.0.1:9708 --sample-ms 200
```

For a durable launchd installation, stage the candidate as the login user,
verify its checksum, then use one narrow privileged install step. The installed
file should be owned by `root:wheel`, mode 0755, in a root-owned directory such
as `/usr/local/libexec/muser-console/`. The launchd plist should run that exact
absolute path as root, bind loopback unless the network policy explicitly
permits wider exposure, and capture stderr. The exporter itself must never be
made setuid.

Before an upgrade, copy the installed binary to a root-owned versioned rollback
path and record both hashes. Install the staged candidate without following a
symlink, restart the launchd job, then require:

- `GET /healthz` returns `{"ok":true}`;
- `GET /metrics` reports `muser_agent_up 1` and real package/CPU/GPU fields on
  the target Mac;
- a console native one-second history query records fresh
  `source=agent`, `honesty=agent-measured` points.

If any check fails, stop the candidate, reinstall the recorded previous bytes,
restart the job, and verify the previous hash and health. A non-root process
reporting `muser_agent_up 0` is an honest diagnostic, not a reason for the
binary to ask for privilege.

## GX10 NVML agent

The GX artifact is ARM64 Linux GNU and is built inside pinned Rust 1.93.1
Bookworm, not on the Mac host. Package only the `gx10-exporter` crate, the
minimal workspace manifest, and the locked dependency graph into a task
directory. Never copy repository HEAD wholesale to the node.

Before touching the node, capture a receipt containing container id, image,
start time, restart count, producer supervisor/socket state, GPU-lock state,
filesystem target state, and NVML visibility. Stop for operator direction if
the exporter target, port, or service exists but does not match the recorded
muser-console installation. Never overwrite an unknown service.

For first install, assert the target is absent. For an update, extract the
current `/usr/local/bin/gx10-exporter` from the resident container first and
record its SHA-256 as the rollback artifact. Build the candidate in the pinned
ARM64 Bookworm image and verify all of these before installation:

- ELF machine is AArch64;
- the interpreter and dynamic dependencies resolve in the resident container;
- candidate, transferred, and in-container SHA-256 values agree;
- a temporary bind serves `/healthz` and honest NVML exposition;
- device indices and supported fields match direct NVML visibility.

Only then copy the candidate to
`muser-redhat-native-f1-593b96a:/usr/local/bin/gx10-exporter`. The dedicated
host systemd unit executes it inside that container on `0.0.0.0:9707` and
restarts after container availability. The accepted unit is retained as
[`phase4-20260823-gx-deploy-attempt4.service`](../fixtures/captures/phase4-20260823-gx-deploy-attempt4.service).
Do not edit, restart, or replace the resident producer or its supervisor as
part of exporter deployment.

After installation, repeat the complete node-state receipt and require the
resident container image/start/restart identity and producer health to be
unchanged. Leave the dedicated exporter service healthy. To roll back, stop
only that unit, restore the previously extracted binary after rechecking its
hash and linkage, start the unit, and repeat exporter plus resident-producer
health checks. Never roll back by replacing the container image or producer.
