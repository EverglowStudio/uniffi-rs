#!/usr/bin/env python3
"""Consume the untouched output of `cargo test --test errors_defaults` on macOS.

Requires Swift, kotlinc, JAVA_HOME, JNA_JAR and COROUTINES_JAR. All output is
kept in this worktree's target/u1/errors-defaults directory.
"""
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess

SOURCE = Path(__file__).resolve().parent
REPO = SOURCE.parents[4]
OUT = REPO / "target/u1/errors-defaults"
ENV = dict(os.environ)
ENV["TMPDIR"] = str(OUT / "tmp")
Path(ENV["TMPDIR"]).mkdir(parents=True, exist_ok=True)
ENV["JAVA_TOOL_OPTIONS"] = ENV.get("JAVA_TOOL_OPTIONS", "") + " -Djava.io.tmpdir=" + ENV["TMPDIR"]
ENV["CLANG_MODULE_CACHE_PATH"] = str(OUT / "clang-cache")
ENV["SWIFT_MODULECACHE_PATH"] = str(OUT / "swift-cache")
commands = []


def run(label, args, expected=None):
    args = list(map(str, args))
    print("+ " + shlex.join(args), flush=True)
    result = subprocess.run(args, cwd=REPO, env=ENV, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=240)
    (OUT / f"{label}.log").write_text(result.stdout)
    commands.append({"label": label, "command": args, "exit_code": result.returncode})
    (OUT / "native-commands.json").write_text(json.dumps(commands, indent=2) + "\n")
    if expected is None:
        assert result.returncode == 0, result.stdout
    else:
        assert result.returncode != 0 and expected in result.stdout, result.stdout
    print(result.stdout, end="", flush=True)
    return result.stdout


def hashes(paths):
    return {str(p.relative_to(REPO)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(paths) if p.is_file()}


inputs = [*OUT.joinpath("swift").rglob("*"), *OUT.joinpath("kotlin").rglob("*"),
          OUT / "liberrors_defaults.dylib"]
load_path = Path(run("native-install-name", ["otool", "-D", OUT / "liberrors_defaults.dylib"]).splitlines()[-1])
assert load_path.is_absolute() and load_path.is_file(), "expected Cargo dylib install name"
assert load_path.is_relative_to(REPO / "target"), "native load path escaped this worktree target"
assert load_path.read_bytes() == (OUT / "liberrors_defaults.dylib").read_bytes(), "copied and linked Rust libraries differ"
inputs.append(load_path)
before = hashes(inputs)
swift_dir = OUT / "swift"
swift_sources = sorted(swift_dir.glob("*.swift"))
assert swift_sources, "run the errors_defaults Rust integration test first"
common = ["xcrun", "swiftc", "-swift-version", "6", "-strict-concurrency=complete",
          "-warnings-as-errors", "-module-cache-path", OUT / "swift-cache", "-I", swift_dir]
for modulemap in sorted(swift_dir.glob("*.modulemap")):
    common += ["-Xcc", f"-fmodule-map-file={modulemap}"]
run("swift-version", ["xcrun", "swiftc", "--version"])
run("swift-build", [*common, "-parse-as-library", *swift_sources, SOURCE / "Verify.swift",
                    OUT / "liberrors_defaults.dylib", "-o", OUT / "verify-swift"])
run("swift-dependencies", ["otool", "-L", OUT / "verify-swift"])
assert "SWIFT_ERRORS_DEFAULTS_OK" in run("swift-run", [OUT / "verify-swift"])
invalid_swift = OUT / "Invalid.swift"
invalid_swift.write_text('func invalid() { _ = Defaults(); _ = inspectArguments() }\n')
required_swift = "missing argument for parameter 'required'"
swift_errors = run("swift-required", [*common, "-typecheck", *swift_sources, invalid_swift], required_swift)
# Swift repeats each diagnostic in its source excerpt; count only locations.
assert sum(line.startswith(str(invalid_swift) + ":") and required_swift in line
           for line in swift_errors.splitlines()) == 2
java_home = Path(os.environ["JAVA_HOME"])
ENV["PATH"] = str(java_home / "bin") + os.pathsep + ENV["PATH"]
jars = [Path(os.environ[name]).resolve() for name in ["JNA_JAR", "COROUTINES_JAR"]]
assert all(p.is_file() for p in jars)
classpath = os.pathsep.join(map(str, jars))
kotlinc = os.environ.get("KOTLINC", "kotlinc")
kotlin_sources = sorted(OUT.joinpath("kotlin").rglob("*.kt"))
run("java-version", [java_home / "bin/java", "-version"])
run("kotlin-version", [kotlinc, "-version"])
run("kotlin-build", [kotlinc, *kotlin_sources, SOURCE / "Verify.kt", "-classpath", classpath,
                     "-jvm-target", "17", "-Werror", "-include-runtime", "-d", OUT / "verify.jar"])
assert "KOTLIN_ERRORS_DEFAULTS_OK" in run("kotlin-run", [java_home / "bin/java", "-ea",
    f"-Djna.library.path={OUT}", f"-Duniffi.component.errors_defaults.libraryOverride={OUT / 'liberrors_defaults.dylib'}",
    "-cp", str(OUT / "verify.jar") + os.pathsep + classpath, "VerifyKt"])
invalid_kotlin = OUT / "Invalid.kt"
invalid_kotlin.write_text('import uniffi.errors_defaults.*\nfun invalid() { Defaults(); inspectArguments() }\n')
required_kotlin = "no value passed for parameter 'required'"
assert run("kotlin-required", [kotlinc, *kotlin_sources, invalid_kotlin, "-classpath", classpath,
           "-d", OUT / "invalid.jar"], required_kotlin).count(required_kotlin) == 2
assert hashes(inputs) == before, "generated/native inputs changed during verification"
(OUT / "native-evidence.json").write_text(json.dumps({"passed": True, "inputs_unchanged": True,
    "sha256": before, "consumers": hashes(SOURCE.glob("*")),
    "jars": {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in jars}}, indent=2) + "\n")
print("NATIVE_ERRORS_DEFAULTS_OK")
