from pathlib import Path
import os,sys,json,subprocess,tempfile,hashlib,shlex,datetime,argparse
root=Path(__file__).resolve().parents[5]
source=root/'uniffi_bindgen/src/bindings/swift/templates'
support=Path(__file__).parent
parser=argparse.ArgumentParser()
parser.add_argument('--output',type=Path,required=True)
parser.add_argument('--prefix',default='swift-runtime')
args=parser.parse_args()
artifacts=args.output
artifacts.mkdir(parents=True,exist_ok=True)
prefix=args.prefix
env=os.environ.copy()
a=(source/'Async.swift').read_text();a=a[a.index('fileprivate let uniffiContinuationHandleMap'):];a=a.replace('{%- if ci.has_async_callback_interface_definition() %}','').replace('{%- if include_stream_runtime %}','').replace('{%- endif %}','').replace('{{ ensure_init_fn_name }}','uniffiEnsureGateInitialized')
step='enum __UniffiStreamNext<T: Sendable>: Sendable { case item(T); case done; case error(Error) }\n'
s=(support/'support.swift').read_text()+step+(source/'HandleMap.swift').read_text()+a+(source/'InputStreamRuntime.swift').read_text()+(support/'runtime.swift').read_text()
s=s.replace('{{ Type::String.borrow()|lower_fn }}','lowerString').replace('{{ ci.namespace()|class_name }}','Gate')
with tempfile.TemporaryDirectory(prefix='uniffi-s04-swift-') as tmp:
    path=Path(tmp)/'gate.swift';path.write_text(s)
    results=[]
    for label,command in [('compile',['swiftc','-swift-version','6','-strict-concurrency=complete','-warnings-as-errors','-parse-as-library',str(path),'-o',tmp+'/gate']),('run',[tmp+'/gate'])]:
        with open(artifacts/(prefix+'-'+label+'.log'),'w') as log:
            log.write(datetime.datetime.now().astimezone().isoformat()+'\n'+shlex.join(command)+'\n');log.flush()
            result=subprocess.run(command,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=45)
            log.write('\nEXIT: '+str(result.returncode)+'\n')
        results.append({'command':command,'exit':result.returncode})
        print(label,result.returncode,flush=True)
        if result.returncode:
            print((artifacts/(prefix+'-'+label+'.log')).read_text(),flush=True)
            break
        if label == 'run': print((artifacts/(prefix+'-'+label+'.log')).read_text(),flush=True)
    (artifacts/(prefix+'.json')).write_text(json.dumps({'sha256':hashlib.sha256(s.encode()).hexdigest(),'commands':results},indent=2)+'\n')
    sys.exit(result.returncode)
