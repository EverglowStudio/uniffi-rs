const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const ts = require(process.env.UNIFFI_ARK_TYPESCRIPT);
const generated = path.join(__dirname, 'generated');
const input = path.join(generated, 'Index.ets');
const moduleName = fs.readFileSync(input, 'utf8').match(/import \* as __uniffiNative from "([^"]+)"/)[1];
const nativeTypes = path.join(generated, 'ark-native.d.ts');
fs.writeFileSync(nativeTypes, `declare module ${JSON.stringify(moduleName)} { export function __uniffi_backend_factory(host: Object): Object; }`);
const outDir = path.join(generated, 'ark-out');
const options = { target: ts.ScriptTarget.ES2021, module: ts.ModuleKind.CommonJS, strict: true, useUnknownInCatchVariables: false, lib: ['lib.es2021.d.ts'], outDir, allowNonTsExtensions: true };
const host = ts.createIncrementalCompilerHost(options);
const builder = ts.createIncrementalProgram({ rootNames: [input, nativeTypes], options, host });
const program = builder.getProgram();
const diagnostics = ts.getPreEmitDiagnostics(program).concat(ts.ArkTSLinter_1_1.runArkTSLinter(builder, undefined, undefined, 'ArkTS_1_1'));
const declarations = ts.createProgram([path.join(generated, 'Index.d.ets')], options);
diagnostics.push(...ts.getPreEmitDiagnostics(declarations));
for (const diagnostic of diagnostics) {
  const pos = diagnostic.file?.getLineAndCharacterOfPosition(diagnostic.start || 0);
  console.log(`${diagnostic.file?.fileName || ''}:${pos ? pos.line + 1 : ''}: ${ts.flattenDiagnosticMessageText(diagnostic.messageText, '\n')}`);
}
assert.equal(diagnostics.filter(d => d.category === ts.DiagnosticCategory.Error).length, 0, 'strict ArkTS SDK compiler, linter and public declarations');
assert.equal(program.emit().emitSkipped, false);
// Execute the SDK-compiled Ark facade against real Rust through the native
// N-API host. OHOS .so cross-compilation is reported separately, not as a device run.
const nativeModule = path.join(outDir, 'node_modules', moduleName);
fs.mkdirSync(nativeModule, { recursive: true });
const addon = path.join(generated, 'node', moduleName.replace(/^lib/, '').replace(/\.so$/, '.node'));
fs.writeFileSync(path.join(nativeModule, 'index.js'), `module.exports = require(${JSON.stringify(addon)});`);
fs.writeFileSync(path.join(outDir, 'package.json'), '{"type":"commonjs"}');
const watchdog = setTimeout(() => { console.error('deadline: Ark native contract'); process.exit(1); }, 65000);
require('./ark-contract.cjs').run(require(path.join(outDir, 'Index.js')))
  .then(result => console.log(JSON.stringify({ ...result, sdk: ts.version, diagnostics: diagnostics.length, compilerErrors: 0, arktsLinter: 'ArkTS_1_1', nativeTransport: 'NAPI', deviceExecution: false })))
  .catch(error => { console.error(error); process.exitCode = 1; })
  .finally(() => clearTimeout(watchdog));
