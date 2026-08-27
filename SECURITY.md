# Security policy

## Reporting a vulnerability

Please use GitHub's **private vulnerability reporting** on this repository
(Security tab → Report a vulnerability). Do not open a public issue for
suspected vulnerabilities.

muser-console is designed to sit on private networks next to inference
engines and to be reachable from phones over HTTPS with QR pairing, so
reports about the pairing flow, TLS handling, allowlist enforcement, and
anything that could expose engine API keys are especially welcome.

## Security posture

- The console never embeds engine API keys in QR codes or URLs; pairing is
  one-time and local-network only.
- Remote access is HTTPS with explicit listen addresses; the docs call out
  why binding a specific address beats `0.0.0.0`.
- The server runs with no web framework and no template engine; responses
  are rendered from checked-in assets and validated engine data.

Security-relevant design decisions are documented in
[the README](README.md#secure-remote-access) and
[the engine contract](docs/engine-contract.md).
