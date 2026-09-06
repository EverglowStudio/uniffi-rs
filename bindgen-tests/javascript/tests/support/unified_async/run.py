#!/usr/bin/env python3
"""Run the opt-in unified async fixture with retained logs."""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time


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
    args = parser.parse_args()
    env = os.environ.copy()
    if args.env_file is not None:
        args.env_file = args.env_file.expanduser().resolve()
        env.update(json.loads(args.env_file.read_text()))
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
    for name in ["fixture-harness.command.txt", "fixture-harness.stdout.log", "fixture-harness.stderr.log"]:
        old = logs / name
        if old.exists():
            old.rename(logs / f"{name}.previous-{time.time_ns()}")
    (logs / "fixture-harness.command.txt").write_text(
        shlex.join(invocation) + "\n"
        + f"# Cargo cwd: {workspace}\n"
        + f"# UNIFFI_ASYNC_ARTIFACTS={logs}\n"
        + f"# RUSTFLAGS=-Dwarnings UNIFFI_ASYNC_WASM_ONLY={env.get('UNIFFI_ASYNC_WASM_ONLY', '0')}\n"
        + shlex.join(command) + "\n"
    )
    with (logs / "fixture-harness.stdout.log").open("w") as stdout, (logs / "fixture-harness.stderr.log").open("w") as stderr:
        result = subprocess.run(command, cwd=workspace, env=env, stdout=stdout, stderr=stderr, timeout=3600)
    print(f"fixture harness exit: {result.returncode}; logs: {logs}")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
