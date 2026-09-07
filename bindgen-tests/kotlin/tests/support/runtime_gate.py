#!/usr/bin/env python3
"""Compile production JNA coroutine templates against a deterministic mock ABI.

This checks Kotlin lifecycle races without Cargo; it is not native FFI validation.
Pass separate directories for generated source/logs and derived binaries.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument('output', type=Path)
parser.add_argument('--binary-output', type=Path, required=True)
args = parser.parse_args()
root = Path(__file__).resolve().parents[4]
templates = root / 'uniffi_bindgen/src/bindings/kotlin/templates'
environment = os.environ.copy()
if not environment.get('CLASSPATH'):
    parser.error('CLASSPATH must include Kotlin test, JNA and kotlinx-coroutines jars')
out = args.output.resolve()
out.mkdir(parents=True, exist_ok=True)
binary_out = args.binary_output.resolve()
binary_out.mkdir(parents=True, exist_ok=True)
jar = binary_out / 'jna-runtime-gate.jar'
parts = []
for name in ['HandleMap.kt', 'Async.kt', 'InputStreamRuntime.kt']:
    source = (templates / name).read_text()
    source = re.sub(r'{%-? (?:if .*?|endif) %}', '', source)
    source = source.replace('{{ Type::String.borrow()|lower_fn }}', 'lowerString')
    source = source.replace('{{ ci.namespace()|class_name(ci) }}', 'Gate')
    if '{{' in source or '{%' in source:
        raise RuntimeError(f'Unrendered template expression in {name}')
    parts.append(source)
source = Path(__file__).with_name('runtime_gate.kt').read_text()
header, tests = source.split('// PRODUCTION TEMPLATES\n')
(out / 'jna-runtime-gate.kt').write_text(header + '\n'.join(parts) + tests)
env = environment
commands = [
    ['kotlinc', '-Werror', str(out / 'jna-runtime-gate.kt'), '-classpath', environment['CLASSPATH'], '-d', str(jar)],
    ['kotlin', '-J-ea', '-classpath', str(jar) + ':' + environment['CLASSPATH'], 'Jna_runtime_gateKt'],
]
for index, command in enumerate(commands):
    (out / f'command-{index}.json').write_text(json.dumps(command, indent=2))
    log = out / f'raw-{index}.log'
    with log.open('w') as output:
        process = subprocess.Popen(command, env=env, text=True, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait()
            code = 124
    print(log.read_text(), end='', flush=True)
    if code:
        raise SystemExit(code)
    if index == 0:
        (out / 'binary.json').write_text(json.dumps({
            'path': str(jar),
            'sha256': hashlib.sha256(jar.read_bytes()).hexdigest(),
            'bytes': jar.stat().st_size,
        }, indent=2) + '\n')
