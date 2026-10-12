#!/usr/bin/env python3
"""Build and smoke-test the public v1 release bundle.

The caller supplies the already-verified ARM64 Linux GX binary plus its
sanitized remote smoke receipt. This script packages exact bytes; it never
builds on a remote host, handles a credential, or copies repository HEAD.
"""

from __future__ import annotations

import argparse
import datetime as dt
import gzip
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import subprocess
import tarfile
import tempfile
import time
import tomllib
from typing import Any


REPO = Path(__file__).resolve().parent.parent
GX_SMOKE_SCHEMA = "muser-console.gx10-package-smoke.v1"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument(
        "--console-bin", type=Path, default=REPO / "target/release/muser-console"
    )
    parser.add_argument(
        "--mac-bin", type=Path, default=REPO / "target/release/mac-exporter"
    )
    parser.add_argument("--gx-bin", type=Path, required=True)
    parser.add_argument("--gx-smoke-receipt", type=Path, required=True)
    return parser.parse_args()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def unused_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as stream:
        stream.bind(("127.0.0.1", 0))
        return int(stream.getsockname()[1])


def wait_health(port: int, deadline: float) -> dict[str, Any]:
    last = "not attempted"
    while time.monotonic() < deadline:
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
        try:
            connection.request("GET", "/healthz")
            response = connection.getresponse()
            body = response.read()
            if response.status == 200:
                value = json.loads(body)
                if value == {"ok": True}:
                    return {"status": 200, "body": value}
                last = "unexpected JSON"
            else:
                last = f"HTTP {response.status}"
        except (OSError, ValueError, json.JSONDecodeError) as error:
            last = type(error).__name__
        finally:
            connection.close()
        time.sleep(0.1)
    raise RuntimeError(f"health probe did not pass: {last}")


def stop(process: subprocess.Popen[bytes] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.send_signal(signal.SIGINT)
    try:
        process.wait(timeout=8)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def smoke_console(binary: Path, ui_dir: Path) -> dict[str, Any]:
    process: subprocess.Popen[bytes] | None = None
    with tempfile.TemporaryDirectory(prefix="muser-console-package-smoke.") as raw:
        root = Path(raw)
        console_key = root / "console.key"
        engine_key = root / "engine.key"
        console_key.write_text(secrets.token_urlsafe(32) + "\n")
        engine_key.write_text(secrets.token_urlsafe(32) + "\n")
        os.chmod(console_key, 0o600)
        os.chmod(engine_key, 0o600)
        port = unused_port()
        config = root / "console.toml"
        config.write_text(
            f'listen = "127.0.0.1:{port}"\n'
            f'access_key_file = "{console_key}"\n'
            f'ui_dir = "{ui_dir}"\n\n'
            "[history]\n"
            "enabled = false\n\n"
            "[[instance]]\n"
            'name = "smoke"\n'
            'base_url = "http://127.0.0.1:9"\n'
            f'api_key_file = "{engine_key}"\n'
        )
        os.chmod(config, 0o600)
        try:
            process = subprocess.Popen(
                [str(binary), "--config", str(config)],
                cwd=root,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            health = wait_health(port, time.monotonic() + 15)
            return {"pass": True, "health": health, "transport": "loopback-http"}
        finally:
            stop(process)


def smoke_mac(binary: Path) -> dict[str, Any]:
    process: subprocess.Popen[bytes] | None = None
    port = unused_port()
    try:
        process = subprocess.Popen(
            [str(binary), "--listen", f"127.0.0.1:{port}", "--sample-ms", "200"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        health = wait_health(port, time.monotonic() + 15)
        return {
            "pass": True,
            "health": health,
            "powermetrics_required_for_health": False,
        }
    finally:
        stop(process)


def file_identity(path: Path) -> dict[str, Any]:
    completed = subprocess.run(
        ["file", "-b", str(path)],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return {
        "bytes": path.stat().st_size,
        "sha256": sha256(path),
        "file": completed.stdout.strip(),
    }


def copy_file(source: Path, destination: Path, mode: int = 0o644) -> None:
    if not source.is_file() or source.is_symlink():
        raise RuntimeError(f"release input is not a regular file: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    os.chmod(destination, mode)


def normalized_archive(source: Path, archive: Path, epoch: int) -> None:
    with archive.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as zipped:
            with tarfile.open(fileobj=zipped, mode="w") as tar:

                def normalize(info: tarfile.TarInfo) -> tarfile.TarInfo:
                    info.uid = 0
                    info.gid = 0
                    info.uname = "root"
                    info.gname = "root"
                    info.mtime = epoch
                    if info.isdir():
                        info.mode = 0o755
                    return info

                tar.add(source, arcname=source.name, recursive=True, filter=normalize)


def git_source_archive(archive: Path, prefix: str, epoch: int) -> None:
    """Archive the exact committed tree, then gzip it with a fixed timestamp."""
    process = subprocess.Popen(
        ["git", "archive", "--format=tar", f"--prefix={prefix}/", "HEAD"],
        cwd=REPO,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert process.stdout is not None
    assert process.stderr is not None
    try:
        with archive.open("xb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as zipped:
                shutil.copyfileobj(process.stdout, zipped)
        stderr = process.stderr.read()
        status = process.wait()
    except BaseException:
        process.kill()
        process.wait()
        raise
    if status != 0:
        raise RuntimeError(
            "git archive failed: " + stderr.decode("utf-8", errors="replace").strip()
        )


def main() -> int:
    args = parse_args()
    status = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=REPO,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    ).stdout
    if status:
        raise SystemExit("release packaging requires a clean source worktree")
    rustc = subprocess.run(
        ["rustc", "--version"],
        cwd=REPO,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    ).stdout.strip()
    if not rustc.startswith("rustc 1.93.1 "):
        raise SystemExit(f"release packaging requires rustc 1.93.1, got {rustc}")

    out_dir = args.out_dir.resolve()
    if out_dir.exists() or out_dir.is_symlink():
        raise SystemExit(f"refusing to replace release directory: {out_dir}")
    if Path(tempfile.gettempdir()).resolve() in out_dir.parents:
        raise SystemExit("release evidence must be outside the system temporary directory")
    if out_dir == REPO or REPO in out_dir.parents:
        raise SystemExit("release evidence must be outside the source repository")

    for label, path in [
        ("console", args.console_bin),
        ("mac agent", args.mac_bin),
        ("GX agent", args.gx_bin),
        ("GX smoke receipt", args.gx_smoke_receipt),
    ]:
        if not path.is_file() or path.is_symlink():
            raise SystemExit(f"{label} input is not a regular file: {path}")

    gx_receipt = json.loads(args.gx_smoke_receipt.read_bytes())
    gx_hash = sha256(args.gx_bin)
    if (
        gx_receipt.get("schema") != GX_SMOKE_SCHEMA
        or gx_receipt.get("pass") is not True
        or gx_receipt.get("sha256") != gx_hash
        or gx_receipt.get("health_ok") is not True
        or gx_receipt.get("architecture") != "aarch64-linux-gnu"
        or gx_receipt.get("dynamic_linkage_ok") is not True
        or gx_receipt.get("real_nvml") is not True
    ):
        raise SystemExit("GX smoke receipt does not authorize these exact bytes")

    workspace = tomllib.loads((REPO / "Cargo.toml").read_text())
    version = str(workspace["workspace"]["package"]["version"])
    bundle_name = f"muser-console-v{version}"
    out_dir.mkdir(parents=True)
    bundle = out_dir / bundle_name

    destinations = {
        "console": bundle / "bin/aarch64-apple-darwin/muser-console",
        "mac": bundle / "bin/aarch64-apple-darwin/mac-exporter",
        "gx": bundle / "bin/aarch64-unknown-linux-gnu/gx10-exporter",
    }
    copy_file(args.console_bin.resolve(), destinations["console"], 0o755)
    copy_file(args.mac_bin.resolve(), destinations["mac"], 0o755)
    copy_file(args.gx_bin.resolve(), destinations["gx"], 0o755)

    for source, relative in [
        (REPO / "ui/muser-dashboard.html", "ui/muser-dashboard.html"),
        (REPO / "examples/config.toml", "examples/config.toml"),
        (REPO / "README.md", "README.md"),
        (REPO / "CHANGELOG.md", "CHANGELOG.md"),
        (REPO / "PROVENANCE", "PROVENANCE"),
        (REPO / "Cargo.toml", "Cargo.toml"),
        (REPO / "Cargo.lock", "Cargo.lock"),
        (REPO / "rust-toolchain.toml", "rust-toolchain.toml"),
        (REPO / "LICENSE-APACHE", "LICENSE-APACHE"),
        (REPO / "LICENSE-MIT", "LICENSE-MIT"),
        (REPO / "NOTICE", "NOTICE"),
    ]:
        copy_file(source, bundle / relative)
    for source in sorted((REPO / "docs").glob("*.md")):
        copy_file(source, bundle / "docs" / source.name)
    for source in sorted((REPO / "schema").glob("*.json")):
        copy_file(source, bundle / "schema" / source.name)

    identities = {name: file_identity(path) for name, path in destinations.items()}
    if "Mach-O 64-bit executable arm64" not in identities["console"]["file"]:
        raise SystemExit("packaged console is not ARM64 Mach-O")
    if "Mach-O 64-bit executable arm64" not in identities["mac"]["file"]:
        raise SystemExit("packaged Mac agent is not ARM64 Mach-O")
    gx_file = identities["gx"]["file"]
    if "ELF 64-bit" not in gx_file or "ARM aarch64" not in gx_file:
        raise SystemExit("packaged GX agent is not ARM64 ELF")

    smoke = {
        "console": smoke_console(destinations["console"], bundle / "ui"),
        "mac_agent": smoke_mac(destinations["mac"]),
        "gx_agent": {
            "pass": True,
            "receipt_schema": gx_receipt["schema"],
            "health_ok": gx_receipt["health_ok"],
            "real_nvml": gx_receipt.get("real_nvml") is True,
        },
    }
    if not all(item.get("pass") is True for item in smoke.values()):
        raise SystemExit("one or more packaged binaries failed smoke testing")

    manifest_lines = []
    for path in sorted(bundle.rglob("*")):
        if path.is_file() and path.name != "SHA256SUMS":
            manifest_lines.append(f"{sha256(path)}  {path.relative_to(bundle)}")
    (bundle / "SHA256SUMS").write_text("\n".join(manifest_lines) + "\n")

    epoch_text = subprocess.run(
        ["git", "show", "-s", "--format=%ct", "HEAD"],
        cwd=REPO,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    ).stdout.strip()
    archive = out_dir / f"{bundle_name}-aarch64.tar.gz"
    normalized_archive(bundle, archive, int(epoch_text))
    source_archive = out_dir / f"{bundle_name}-source.tar.gz"
    git_source_archive(source_archive, f"{bundle_name}-source", int(epoch_text))

    receipt = {
        "schema": "muser-console.release-bundle.v1",
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "version": version,
        "source_commit": subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=REPO,
            check=True,
            stdout=subprocess.PIPE,
            text=True,
        ).stdout.strip(),
        "rustc": rustc,
        "bundle": {
            "path": archive.name,
            "bytes": archive.stat().st_size,
            "sha256": sha256(archive),
        },
        "source": {
            "path": source_archive.name,
            "bytes": source_archive.stat().st_size,
            "sha256": sha256(source_archive),
        },
        "binaries": identities,
        "smoke": smoke,
        "gx_receipt_sha256": sha256(args.gx_smoke_receipt),
        "excluded_secrets_and_measurement_databases": True,
        "public_release_created": False,
        "signed": False,
    }
    receipt_path = out_dir / "release-receipt.json"
    write_json(receipt_path, receipt)
    (out_dir / "artifacts.sha256").write_text(
        f"{sha256(archive)}  {archive.name}\n"
        f"{sha256(source_archive)}  {source_archive.name}\n"
        f"{sha256(receipt_path)}  {receipt_path.name}\n"
    )
    print(
        json.dumps(
            {
                "outcome": "passed",
                "archive": str(archive),
                "archive_sha256": receipt["bundle"]["sha256"],
                "receipt": str(receipt_path),
                "source_archive": str(source_archive),
                "source_archive_sha256": receipt["source"]["sha256"],
            },
            indent=2,
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
