"""Functional native-next frame gate; uses the unchanged S04 mock ABI support."""
import argparse, hashlib, json, subprocess
from pathlib import Path
p = argparse.ArgumentParser()
p.add_argument('--output', type=Path, required=True)
p.add_argument('--runtime', action='store_true')
a = p.parse_args(); a.output.mkdir(parents=True, exist_ok=True)
root = Path(__file__).resolve().parents[5]
templates = root / 'uniffi_bindgen/src/bindings/swift/templates'
support = root / 'bindgen-tests/swift/tests/support/s04/support.swift'
body = (templates / 'Async.swift').read_text()
# Guard the short post-decision window without adding a production pause hook.
# Dynamic gates below exercise the real raw driver, ARC release and close races.
native_next = body.split('private func nextRust()', 1)[1].split('private func finishRustNext(', 1)[0]
decision = native_next.split('let cancelled = lock.withLock {', 1)[1].split('if itemFinished {', 1)[0]
assert decision.index('if stopped') < decision.index('Task.isCancelled') < decision.index('finishRustNextLocked(')
delivered = native_next.split('if itemFinished {', 1)[1].split('} else {', 1)[0]
assert 'itemContinuation?.resume()' in delivered
assert all(forbidden not in delivered for forbidden in ['Task.isCancelled', 'outcome = nil', 'finishRustNext(', 'withLock'])
slow = native_next.split('} else {', 1)[1]
assert slow.index('outcome = nil') < slow.index('finishRustNext(acknowledgesTerminal: false)') < slow.index('try await close()')
locked_finish = body.split('private func finishRustNextLocked(', 1)[1].split('private func startClose()', 1)[0]
assert all(forbidden not in locked_finish for forbidden in ['.resume()', 'withLock', 'outcome', 'Task.isCancelled'])
assert 'if acknowledgesTerminal && closeTask == nil' in locked_finish
body = body[body.index('fileprivate let uniffiContinuationHandleMap'):]
for directive in ('{%- if include_stream_runtime %}', '{%- if ci.has_async_callback_interface_definition() %}', '{%- endif %}'):
    body = body.replace(directive, '')
body = body.replace('{{ ensure_init_fn_name }}', 'uniffiEnsureGateInitialized')
tests = support.with_name('runtime.swift').read_text() if a.runtime else Path(__file__).with_suffix('.swift').read_text()
adapter = Path(__file__).with_name('c_abi_terminal_mock.swift').read_text()
source = support.read_text() + '\nenum __UniffiStreamNext<T: Sendable>: Sendable { case item(T); case done; case error(Error) }\n' + (templates / 'HandleMap.swift').read_text() + body + (templates / 'InputStreamRuntime.swift').read_text() + adapter + tests
source = source.replace('{{ Type::String.borrow()|lower_fn }}', 'lowerString').replace('{{ ci.namespace()|class_name }}', 'Gate')
f = a.output / 'gate.swift'; f.write_text(source)
commands = [['swiftc','-swift-version','6','-strict-concurrency=complete','-warnings-as-errors','-parse-as-library',str(f),'-o',str(a.output/'gate')],[str(a.output/'gate')]]
records = []
for i, command in enumerate(commands):
    try:
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=60)
    except subprocess.TimeoutExpired as error:
        stdout, stderr = (
            stream.decode('utf-8', errors='replace') if isinstance(stream, bytes) else stream or ''
            for stream in (error.stdout, error.stderr)
        )
        output = stdout + stderr + f'\nCommand timed out after {error.timeout} seconds: {command!r}\n'
        exit_code = 124
        record = {'command':command, 'exit':exit_code, 'timeout':error.timeout,
                  'returncode':None, 'stdout':stdout, 'stderr':stderr}
    else:
        output = result.stdout
        exit_code = result.returncode
        record = {'command':command,'exit':exit_code}
    (a.output/f'raw-{i}.log').write_text(output)
    records.append(record)
    (a.output/'result.json').write_text(json.dumps({'sha256':hashlib.sha256(source.encode()).hexdigest(),'commands':records},indent=2)+'\n')
    print(output, end='', flush=True)
    if exit_code: raise SystemExit(exit_code)
