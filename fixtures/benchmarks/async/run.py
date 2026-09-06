#!/usr/bin/env python3
"""Build once, sample without compilers. Output JSONL retains every call latency."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import time
import zipfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
CASES = ["future_ready", "future_pending_wake", "callback_ready", "callback_yield", "future_cancel_lifecycle",
         "stream_output_ready", "stream_output_pending", "stream_input", "stream_roundtrip"]


def run(command, env, cwd=ROOT, **kwargs):
    command = list(map(str, command))
    print("+ " + shlex.join(command), file=sys.stderr, flush=True)
    return subprocess.run(command, env=env, cwd=cwd, check=True, **kwargs)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["prepare", "sample"])
    parser.add_argument("backend", choices=["swift", "jna", "jni"])
    parser.add_argument("--environment", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True, help="Persistent build artifacts, unique per product revision/backend")
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target")
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--iterations", type=int, default=100)
    parser.add_argument("--warmup", type=int, default=1000)
    parser.add_argument("--stream-items", type=int, default=64)
    parser.add_argument("--case", choices=["all"] + CASES, default="all")
    parser.add_argument("--label", default="calibration")
    args = parser.parse_args()
    if min(args.samples, args.iterations, args.warmup, args.stream_items) < 1:
        parser.error("sample/workload counts must be positive")
    if args.backend == "jni" and args.case.startswith("stream_"):
        parser.error("JNI has no Stream baseline")
    if args.backend == "jni" and args.case == "future_cancel_lifecycle":
        parser.error("JNI suspendCoroutine does not propagate Job cancellation in the baseline")
    env = os.environ.copy()
    env.update(json.loads(args.environment.read_text()))
    target = args.target_dir.resolve()
    out = args.out.resolve()
    env["CARGO_TARGET_DIR"] = str(target)
    features = "async-bench,uniffi-bindgen-kotlin-jni" if args.backend == "jni" else "async-bench-streams"
    if args.action == "prepare":
        out.mkdir(parents=True, exist_ok=True)
        metadata_path = out / "build.json"
        metadata_path.unlink(missing_ok=True)  # An interrupted rebuild must not be sampled.
        run(["cargo", "build", "--locked", "--release", "-p", "uniffi-fixture-benchmarks", "--features", features], env)
        shutil.copy2(target / "release/libuniffi_benchmarks.dylib", out)
        # Cargo's absolute install name otherwise reloads the shared target's
        # most recently built backend instead of this revision's staged library.
        run(["install_name_tool", "-id", "@rpath/libuniffi_benchmarks.dylib", out / "libuniffi_benchmarks.dylib"], env)
        run(["codesign", "--force", "--sign", "-", out / "libuniffi_benchmarks.dylib"], env)
        if args.backend == "jni":
            run(["cargo", "build", "--locked", "--release", "-p", "uniffi-bindgen-kotlin-jni"], env)
            run([target / "release/uniffi-bindgen-kotlin-jni", "bindings", "src:uniffi-fixture-benchmarks",
                 "--features", "async-bench", "--features", "uniffi-bindgen-kotlin-jni", out], env)
        else:
            run(["cargo", "build", "--locked", "--release", "-p", "uniffi", "--features", "cli"], env)
            run([target / "release/uniffi-bindgen", "generate", out / "libuniffi_benchmarks.dylib",
                 "--language", "swift" if args.backend == "swift" else "kotlin", "--out-dir", out, "--no-format"], env)
        if args.backend == "swift":
            modulemap = out / "benchmarksFFI.modulemap"
            common = ["-O", "-swift-version", "5", "-I", out, "-L", out, "-Xcc", f"-fmodule-map-file={modulemap}",
                      "-luniffi_benchmarks", "-Xlinker", "-rpath", "-Xlinker", out]
            run(["swiftc", *common, "-emit-library", "-emit-module", "-module-name", "benchmarks",
                 out / "benchmarks.swift", "-o", out / "libbenchmarks.dylib"], env, cwd=out)
            run(["swiftc", *common, "-parse-as-library", "-lbenchmarks", HERE / "ForeignBench.swift", "-o", out / "foreign-bench"], env)
            command = [str(out / "foreign-bench")]
        else:
            sources = sorted(out.rglob("*.kt"))
            run(["kotlinc", "-Werror", "-jvm-target", "17", "-classpath", env["CLASSPATH"], "-include-runtime", "-d", out / "foreign-bench.jar",
                 *sources, HERE / "ForeignBench.kt", HERE / ("StreamsJni.kt" if args.backend == "jni" else "StreamsJna.kt")], env)
            command = ["java", "-ea", "-Xms512m", "-Xmx512m", f"-Djava.library.path={out}", f"-Djna.library.path={out}",
                       "-cp", str(out / "foreign-bench.jar") + os.pathsep + env["CLASSPATH"], "ForeignBenchKt"]
        harness_files = sorted((ROOT / "fixtures/benchmarks").rglob("*"))
        metadata = {
            "backend": args.backend, "source_root": str(ROOT), "built_at_unix": time.time(),
            "product_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
            "harness_commit": None if subprocess.check_output(["git", "status", "--porcelain", "--", "fixtures/benchmarks"], cwd=ROOT) else subprocess.check_output(["git", "log", "-1", "--format=%H", "--", "fixtures/benchmarks"], cwd=ROOT, text=True).strip(),
            "harness_files_sha256": {str(p.relative_to(ROOT)): sha(p) for p in harness_files if p.is_file() and "__pycache__" not in str(p)},
            "cargo_lock_sha256": sha(ROOT / "Cargo.lock"), "features": features, "profile": "release", "concurrency": 1,
            "command": command, "environment": json.loads(args.environment.read_text()),
            "classpath_sha256": {p: sha(Path(p)) for p in env["CLASSPATH"].split(os.pathsep)},
        }
        for tool in [["rustc", "-Vv"], ["swiftc", "--version"], ["java", "-version"], ["kotlinc", "-version"]]:
            result = run(tool, env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            metadata[tool[0]] = result.stdout.strip()
        shutil.copy2(ROOT / "Cargo.lock", out / "Cargo.lock")
        templates = ["uniffi_bindgen/src/bindings/kotlin/templates/" + name
                     for name in ["NamespaceLibraryTemplate.kt", "macros.kt"]]
        metadata["calibration_templates_sha256"] = {name: sha(ROOT / name) for name in templates}
        (out / "source.patch").write_bytes(subprocess.check_output(
            ["git", "diff", "--binary", "--", "Cargo.lock", "fixtures/benchmarks", *templates], cwd=ROOT))
        with zipfile.ZipFile(out / "harness-source.zip", "w", zipfile.ZIP_DEFLATED) as archive:
            for name in metadata["harness_files_sha256"]:
                archive.write(ROOT / name, name)
            for name in templates:
                archive.write(ROOT / name, name)
        metadata["artifact_sha256"] = {str(p.relative_to(out)): sha(p) for p in out.rglob("*") if p.is_file()}
        metadata_path.write_text(json.dumps(metadata, indent=2) + "\n")
        print(json.dumps({"kind": "prepared", "build": str(metadata_path)}))
    else:
        metadata = json.loads((out / "build.json").read_text())
        if metadata["backend"] != args.backend:
            parser.error("build backend does not match requested backend")
        if metadata["environment"] != json.loads(args.environment.read_text()):
            parser.error("environment changed since prepare; rebuild before sampling")
        for name, expected in metadata["artifact_sha256"].items():
            if sha(out / name) != expected:
                parser.error(f"prepared artifact changed: {name}")
        for name, expected in metadata["classpath_sha256"].items():
            if sha(Path(name)) != expected:
                parser.error(f"classpath dependency changed: {name}")
        print(json.dumps({"kind": "run", "label": args.label, "samples": args.samples, "iterations": args.iterations,
                          "warmup": args.warmup, "stream_items": args.stream_items, "case": args.case, "build": metadata}), flush=True)
        command = metadata["command"] + list(map(str, [args.samples, args.iterations, args.warmup, args.stream_items, args.case]))
        if args.backend != "swift":
            command.append(args.backend)
        run(command, env, timeout=1800)
        print(json.dumps({"kind": "complete", "backend": args.backend, "label": args.label}), flush=True)


if __name__ == "__main__":
    main()
