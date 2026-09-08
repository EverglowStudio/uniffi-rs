"""Functional input close protocol checks using the unchanged S04 mock ABI."""
import argparse, hashlib, json, os, re, subprocess
from pathlib import Path
p = argparse.ArgumentParser(); p.add_argument('--output',type=Path,required=True)
p.add_argument('--runtime',action='store_true')
p.add_argument('--completion',action='store_true')
a = p.parse_args(); a.output.mkdir(parents=True,exist_ok=True)
root = Path(__file__).resolve().parents[5]
templates = root/'uniffi_bindgen/src/bindings/kotlin/templates'
original = (root/'bindgen-tests/kotlin/tests/support/runtime_gate.kt').read_text()
header, helpers = original.split('// PRODUCTION TEMPLATES\n')
helpers = helpers if a.runtime else helpers.split('fun main() = runBlocking {')[0]
parts = []
for name in ['HandleMap.kt','Async.kt','InputStreamRuntime.kt']:
    s = re.sub(r'{%-? (?:if .*?|endif) %}', '', (templates/name).read_text())
    s = s.replace('{{ Type::String.borrow()|lower_fn }}','lowerString').replace('{{ ci.namespace()|class_name(ci) }}','Gate')
    parts.append(s)
tests = helpers if a.runtime else helpers + Path(__file__).with_suffix('.kt').read_text()
tests = tests.replace('uniffiRustCallAsync(', 'uniffiRustCallAsyncMock(')
if a.completion:
    completion = Path(__file__).with_name('async_completion_test.kt').read_text()
    imports = '\n'.join(line for line in completion.splitlines() if line.startswith('import '))
    header = '\n'.join(line for line in imports.splitlines() if line not in header.splitlines()) + '\n' + header
    tests = '\n'.join(line for line in completion.splitlines() if not line.startswith(('package ', 'import ')))
    tests = tests.replace('UniffiNullRustCallStatusErrorHandler', 'NullErrorHandler')
adapter = Path(__file__).with_name('c_abi_terminal_mock.kt').read_text()
source = header + '\n'.join(parts) + adapter + tests
f=a.output/'StreamCloseTest.kt';f.write_text(source);jar=a.output/'gate.jar'
commands=[['kotlinc','-Werror',str(f),'-classpath',os.environ['CLASSPATH'],'-d',str(jar)],['kotlin','-J-ea','-classpath',str(jar)+os.pathsep+os.environ['CLASSPATH'],'StreamCloseTestKt']]
records=[]
for i, command in enumerate(commands):
    result=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=60)
    (a.output/f'raw-{i}.log').write_text(result.stdout)
    records.append({'command':command,'exit':result.returncode})
    (a.output/'result.json').write_text(json.dumps({'sha256':hashlib.sha256(source.encode()).hexdigest(),'commands':records},indent=2)+'\n')
    print(result.stdout,end='',flush=True)
    if result.returncode:raise SystemExit(result.returncode)
