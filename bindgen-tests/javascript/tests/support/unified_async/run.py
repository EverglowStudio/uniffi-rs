#!/usr/bin/env python3
"""Run the opt-in unified async fixture with retained logs."""
import argparse
import json
import hashlib
import os
from pathlib import Path
import re
import shlex
import shutil
import struct
import subprocess
import sys
import time


def recorded_run(command, workspace, env, logs, label):
    paths = [logs / f"fixture-{label}.{suffix}" for suffix in ["command.json", "stdout.log", "stderr.log"]]
    for path in paths:
        if path.exists():
            path.rename(path.with_name(f"{path.name}.previous-{time.time_ns()}"))
    paths[0].write_text(json.dumps({
        "cwd": str(workspace), "command": command,
        "env": {key: value for key, value in env.items() if key.startswith(("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR", "TARGET_", "OHOS_NDK_HOME"))},
    }, indent=2) + "\n")
    with paths[1].open("w") as stdout, paths[2].open("w") as stderr:
        subprocess.run(command, cwd=workspace, env=env, stdout=stdout, stderr=stderr, timeout=1800, check=True)


def build_ohos(workspace, env, logs, sdk):
    """Build and retain both SDK-targeted ELF libraries from this fresh package."""
    root = Path((logs / "fixture-latest-run.txt").read_text().strip())
    manifest = root / "generated/native/hosts/ohos/Cargo.toml"
    tool = sdk / "native/llvm/bin"
    sysroot = sdk / "native/sysroot"
    target = workspace / "target/javascript-tests/ohos"
    env = {**env, "CARGO_TARGET_DIR": str(target), "OHOS_NDK_HOME": str(sdk)}
    lock = manifest.parent / "Cargo.lock"
    if not lock.exists():
        shutil.copyfile(workspace / "Cargo.lock", lock)
        recorded_run(["cargo", "metadata", "--offline", "--format-version", "1", "--manifest-path", str(manifest)], workspace, env, logs, "ohos-lock")
    libraries = []
    for arch, clang_target, machine in [
        ("aarch64-unknown-linux-ohos", "aarch64-linux-ohos", 183),
        ("x86_64-unknown-linux-ohos", "x86_64-linux-ohos", 62),
    ]:
        env.update({
            "CARGO_ENCODED_RUSTFLAGS": "\x1f".join(["-Dwarnings", f"-Clinker={tool / 'clang'}", f"-Clink-arg=--target={clang_target}", f"-Clink-arg=--sysroot={sysroot}"]),
            "TARGET_CC": str(tool / "clang"), "TARGET_CXX": str(tool / "clang++"), "TARGET_AR": str(tool / "llvm-ar"),
            "TARGET_CFLAGS": f"--target={clang_target} --sysroot={shlex.quote(str(sysroot))} -D__MUSL__",
        })
        recorded_run(["cargo", "build", "--locked", "--manifest-path", str(manifest), "--target", arch], workspace, env, logs, f"ohos-{arch}")
        name = "libunified_async_fixture_uniffi_js_host.so"
        library = root / "ohos" / arch / name
        library.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(target / arch / "debug" / name, library)
        with library.open("rb") as source:
            header = source.read(20)
        assert header[:6] == b"\x7fELF\x02\x01" and struct.unpack_from("<HH", header, 16) == (3, machine), f"unexpected shared ELF architecture: {library}"
        recorded_run([str(tool / "llvm-readelf"), "--file-header", "--dyn-syms", str(library)], workspace, env, logs, f"ohos-{arch}-elf")
        assert "napi_register_module_v1" in (logs / f"fixture-ohos-{arch}-elf.stdout.log").read_text(), "missing N-API module registration"
        libraries.append({"target": arch, "path": str(library), "sha256": hashlib.sha256(library.read_bytes()).hexdigest(), "bytes": library.stat().st_size, "status": "passed"})
    return libraries


def main():
    workspace = Path(__file__).resolve().parents[5]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--env-file", type=Path,
        help="optional JSON environment mapping to overlay on the current environment",
    )
    parser.add_argument(
        "--artifacts", type=Path,
        default=workspace / "target/javascript-tests/unified-async-artifacts",
        help="log/artifact directory (default: workspace/target/javascript-tests/unified-async-artifacts)",
    )
    parser.add_argument("--ohos-sdk", type=Path, help="OpenHarmony SDK root containing native/llvm and native/sysroot; defaults to OHOS_NDK_HOME")
    args = parser.parse_args()
    env = os.environ.copy()
    if args.env_file is not None:
        args.env_file = args.env_file.expanduser().resolve()
        env.update(json.loads(args.env_file.read_text()))
    wasm_only = env.get("UNIFFI_ASYNC_WASM_ONLY") == "1"
    sdk = args.ohos_sdk or (Path(env["OHOS_NDK_HOME"]) if env.get("OHOS_NDK_HOME") else None)
    if not wasm_only:
        if sdk is None:
            parser.error("full matrix requires --ohos-sdk or OHOS_NDK_HOME")
        sdk = sdk.expanduser().resolve()
        if not (sdk / "native/llvm/bin/clang").is_file():
            parser.error(f"OHOS SDK clang not found: {sdk}")
        for key in ["UNIFFI_TEST_ELECTRON", "UNIFFI_ARK_TYPESCRIPT"]:
            if not env.get(key) or not Path(env[key]).is_file():
                parser.error(f"full matrix requires {key} pointing to an installed tool")
    logs = args.artifacts.expanduser().resolve()
    env["UNIFFI_ASYNC_ARTIFACTS"] = str(logs)
    env["RUSTFLAGS"] = "-Dwarnings"
    env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    logs.mkdir(parents=True, exist_ok=True)
    command = [
        "cargo", "test", "--locked", "-p", "uniffi-bindgen-tests-javascript",
        "--test", "unified_async", "--", "--ignored", "--nocapture",
    ]
    invocation = [sys.executable, str(Path(__file__).resolve())]
    if args.env_file is not None:
        invocation.extend(["--env-file", str(args.env_file)])
    invocation.extend(["--artifacts", str(logs)])
    if sdk is not None:
        invocation.extend(["--ohos-sdk", str(sdk)])
    tool_env = {key: env[key] for key in ["UNIFFI_TEST_ELECTRON", "UNIFFI_ARK_TYPESCRIPT", "UNIFFI_TEST_TYPESCRIPT_COMPILER", "UNIFFI_TEST_PLAYWRIGHT_MODULE", "UNIFFI_TEST_CHROME"] if key in env}
    for name in ["fixture-harness.command.txt", "fixture-harness.stdout.log", "fixture-harness.stderr.log", "fixture-summary.json"]:
        old = logs / name
        if old.exists():
            old.rename(logs / f"{name}.previous-{time.time_ns()}")
    (logs / "fixture-harness.command.txt").write_text(
        shlex.join(invocation) + "\n"
        + f"# Cargo cwd: {workspace}\n"
        + f"# UNIFFI_ASYNC_ARTIFACTS={logs}\n"
        + f"# RUSTFLAGS=-Dwarnings UNIFFI_ASYNC_WASM_ONLY={env.get('UNIFFI_ASYNC_WASM_ONLY', '0')}\n"
        + f"# Platform tools: {json.dumps(tool_env, ensure_ascii=False)}\n"
        + shlex.join(command) + "\n"
    )
    with (logs / "fixture-harness.stdout.log").open("w") as stdout, (logs / "fixture-harness.stderr.log").open("w") as stderr:
        result = subprocess.run(command, cwd=workspace, env=env, stdout=stdout, stderr=stderr, timeout=3600)
    if result.returncode == 0:
        realms = {}
        for name in ["fixture-node-napi.stdout.log", "fixture-node-wasm.stdout.log", "fixture-chrome.stdout.log", "fixture-electron.stdout.log"]:
            path = logs / name
            if not path.exists():
                continue
            for line in path.read_text().splitlines():
                try:
                    row = json.loads(line[line.index("{"):])
                except (ValueError, json.JSONDecodeError):
                    continue
                if "realm" in row and "cases" in row:
                    realms[row["realm"]] = row["cases"]
        expected = {"node-wasm", "chrome-window", "chrome-worker"}
        if not wasm_only:
            expected.update(["node-napi", "electron-renderer"])
        if set(realms) != expected or any(len(cases) != 44 or sum(c.startswith("actor-") for c in cases) != 6 or sum(c.startswith("s03-") for c in cases) != 13 or sum(c.startswith("r1-") for c in cases) != 4 for cases in realms.values()):
            raise RuntimeError(f"incomplete unified async platform matrix: {realms}")
        baseline = realms["node-wasm"]
        if len(set(baseline)) != 44 or any(cases != baseline for cases in realms.values()):
            raise RuntimeError("all JavaScript realms must execute exactly the same ordered 44 cases")
        ark = None
        ohos = []
        if not wasm_only:
            for line in (logs / "fixture-ark.stdout.log").read_text().splitlines():
                try:
                    row = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if row.get("realm") == "ark-napi" and "cases" in row:
                    ark = row
            expected_ark = re.findall(r"await test\('([^']+)'", (Path(__file__).parent / "ark-contract.cjs").read_text())
            if ark is None or ark.get("status") != "passed" or len(expected_ark) != 17 or ark["cases"] != expected_ark:
                raise RuntimeError(f"incomplete strict SDK/Ark native matrix: {ark}")
            ohos = build_ohos(workspace, env, logs, sdk)
        (logs / "fixture-summary.json").write_text(json.dumps({
            "status": "passed", "realms": realms,
            "legacy_cases_per_realm": 21, "actor_cases_per_realm": 6,
            "declaration_cases_per_realm": 13,
            "review_p1_cases_per_realm": 4,
            "javascript_realm_cases": sum(map(len, realms.values())),
            "ark_native": ark, "ohos_backend_builds": ohos,
            "total_realm_cases": sum(map(len, realms.values())) + (len(ark["cases"]) if ark else 0),
            "harmony_device_execution": False,
            "tools": tool_env,
            "env_file": str(args.env_file) if args.env_file else None,
            "env_sha256": hashlib.sha256(args.env_file.read_bytes()).hexdigest() if args.env_file else None,
        }, indent=2) + "\n")
    print(f"fixture harness exit: {result.returncode}; logs: {logs}")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
