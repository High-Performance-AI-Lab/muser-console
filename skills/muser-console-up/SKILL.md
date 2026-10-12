---
name: muser-console-up
description: Build, configure, start, and verify the standalone muser-console fleet and history service. Use when one or more running Muser engines need the separate console, loopback setup, LAN or private-VPN TLS access, instance credentials, or startup diagnosis. Do not use when the embedded dashboard started by `muser up` is sufficient.
---

# Stand up the standalone Muser console

Work from the repository root. Read the README quick start first. Read
`docs/console-api.md` for authentication or routing work and
`docs/engine-contract.md` only when an engine/console contract mismatch must be
diagnosed.

## Decide whether this service is needed

The main Muser engine already serves its embedded dashboard at
`http://127.0.0.1:4949`. Use this standalone repository only for fleet views,
persisted history, or access from another trusted machine. Do not make a
one-Mac + one-GX10 user install a second service for ordinary onboarding and
inference.

## Prepare one engine

Start Muser with telemetry and management authentication enabled. Pass a
dedicated regular, non-symlink engine key file with mode `0600` to the
engine's `--api-key-file` option. The console receives the path to that file,
not a copied key value.

If the engine is not already running, create its key outside both repositories
and start the release bundle from the Muser directory:

```sh
mkdir -p "$HOME/.muser"
chmod 700 "$HOME/.muser"
openssl rand -hex 32 > "$HOME/.muser/engine.key"
chmod 600 "$HOME/.muser/engine.key"
./bin/muser up --api-key-file "$HOME/.muser/engine.key"
```

Use `./target/release/muser` instead of `./bin/muser` for a source build.

Keep a same-machine engine on literal loopback HTTP, normally
`http://127.0.0.1:4949`. A remote engine must use HTTPS, a valid certificate
SAN, and a configured CA file. Never weaken these refusals or expose an engine
directly to the public Internet.

## Build

```sh
cargo build --release --locked --bin muser-console
```

The resulting binary is `target/release/muser-console`.

## Create private configuration

Create configuration outside the repository. Never commit a key, local
configuration, database, certificate private key, or copied engine state.
Generate a console access key distinct from every engine key:

```sh
mkdir -p "$HOME/.config/muser-console" \
  "$HOME/.local/share/muser-console"
chmod 700 "$HOME/.config/muser-console"
openssl rand -hex 32 > "$HOME/.config/muser-console/console.key"
chmod 600 "$HOME/.config/muser-console/console.key"
```

Save this shape as `~/.config/muser-console/config.toml`, replacing the
absolute placeholders:

```toml
listen = "127.0.0.1:5959"
access_key_file = "console.key"
ui_dir = "/absolute/path/to/muser-console/ui"

[history]
enabled = true
db_path = "/absolute/path/to/muser-console-data/history.sqlite"
retention_days = 7

[[instance]]
name = "local"
base_url = "http://127.0.0.1:4949"
api_key_file = "/absolute/path/to/muser/engine.key"
```

Relative paths are resolved from the configuration directory. TOML does not
expand `~` or shell variables. `ui_dir` names the directory containing
`muser-dashboard.html`, not the HTML file. Every key path must identify a
regular non-symlink file with mode `0600` or stricter.

## Start and verify

```sh
./target/release/muser-console \
  --config "$HOME/.config/muser-console/config.toml"
```

Require the unauthenticated liveness route to pass:

```sh
curl -fsS http://127.0.0.1:5959/healthz
```

It must return `{"ok":true}`. Then open `http://127.0.0.1:5959`, sign in with
the console access key, and require the configured instance to appear
connected. Send a real prompt through the console and verify that the engine
responds and the session/history view updates. A healthy console process with
a disconnected engine is not a successful stand-up.

## Add trusted network access only when requested

Loopback HTTP is the default. Any non-loopback `listen` address requires both
`tls_cert` and a mode-`0600` `tls_key`; the certificate SAN must cover the
exact hostname or IP used by the browser. Use only a trusted LAN or private
VPN. Keep upstream engines on loopback where possible. Follow the README's LAN
certificate and pairing procedure rather than inventing a bypass.

## Diagnose by boundary

- A missing page usually means `ui_dir` does not contain
  `muser-dashboard.html`.
- A disconnected instance usually means the engine `/healthz`, `base_url`,
  or engine `api_key_file` is wrong.
- A permissions refusal means the key is a symlink or its mode is too broad;
  correct the file instead of relaxing validation.
- A remote URL refusal means HTTPS, the SAN, or `ca_file` is missing.
- A non-loopback listener refusal means console TLS is incomplete.

Retain exact startup errors without printing key contents. After source
changes, run:

```sh
cargo fmt --all --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --release --locked
```
