# Public export manifest — 2026-08-29

## Scope

This manifest covers the 2026-08-29 public update to `muser-console`. The
repository is an optional fleet, history, and private-network access
companion. The one-Mac + one-GX10 onboarding path is shipped by the main
`muser` bundle and does not require this repository.

Approval status: **operator-approved for an ordinary push on 2026-08-29**.
The operator explicitly authorized pushing the Lab repositories to GitHub.
This approval does not authorize rewriting public history, force-pushing,
tagging, changing repository visibility, or publishing a GitHub release.

## Exhaustive path inventory

The intended export contains 149 paths. The SHA-256 of their
newline-terminated, byte-sorted relative path list is:

```text
75644c0845077ed05097baebfc508b2f5d95174d3b0cb08b30f976225cd733b7
```

Reproduce the inventory before staging:

```sh
git ls-files --cached --others --exclude-standard |
  LC_ALL=C sort |
  tee /tmp/muser-console-export-paths |
  shasum -a 256
```

Every candidate path is classified below. Counts are part of the contract;
an unmatched or additional path stops the export.

| Classification | Path set | Count |
| --- | --- | ---: |
| Ship as-is | `.github/workflows/ci.yml` | 1 |
| Ship as-is | Root metadata and build files: `.gitignore`, `CHANGELOG.md`, `CONTRIBUTING.md`, `Cargo.lock`, `Cargo.toml`, `LICENSE-APACHE`, `LICENSE-MIT`, `NOTICE`, `PROVENANCE`, `README.md`, `SECURITY.md`, `rust-toolchain.toml` | 12 |
| Ship as-is | `agents/**` | 25 |
| Ship as-is | `assets/**` | 4 |
| Ship as-is | `crates/**` | 27 |
| Ship as-is | `docs/**`, including this manifest | 5 |
| Ship as-is | `examples/**` | 1 |
| Ship as-is | `fixtures/**` | 69 |
| Ship as-is | `schema/**` | 2 |
| Ship as-is | `scripts/**` | 2 |
| Ship as-is | `ui/**` | 1 |
| Needs trim | None | 0 |
| Exclude | None of the 149 candidate paths | 0 |

Repository metadata, build output, local configuration, databases, and OS
metadata are outside the candidate set and must not be copied: `.git/**`,
`target/**`, `console.toml`, `data/**`, `*.sqlite*`, and `.DS_Store`.

The checked-in acceptance fixtures are release evidence. Their example
filesystem locations use neutral `/opt/...` paths; documentation uses
reserved example addresses and generic `/absolute/path/to/...` values.

## Incremental publication contract

1. Preserve the existing sanitized Lab history and append an ordinary commit;
   never rewrite or force-push it.
2. Require exactly the 149 paths above in the publication snapshot.
3. Use `videlalvaro <30834+videlalvaro@users.noreply.github.com>` for every
   new author and committer identity.
4. Do not add private Git history, credentials, local databases, build
   outputs, or release evidence outside this manifest.
5. Run the Lab export verifier against a fresh one-commit snapshot of the
   exact publication tree and require zero failures and skips. Also require
   the locked Rust test suite, strict Clippy, rustfmt, RustSec,
   fixture/schema tests, and a release build to pass.
6. Activate the publication account only for the authorized push, then
   restore the default account immediately afterward.
