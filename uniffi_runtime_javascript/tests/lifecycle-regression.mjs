import assert from 'node:assert/strict';
import {BackendSession, Host, createFacade, invokeObjectOperation, disposeObject} from './runtime.js';

const tick = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => { resolve=a; reject=b; }); return {promise, resolve, reject}; };
const named = id => ({kind:'named', typeId:id, name:['Client','Service','Subscription'][id]});
const operations = {
  0: {id:0, name:'create', kind:'Constructor', surface:'object', ownerTypeId:0, args:[], returnType:named(0), async:true},
  1: {id:1, name:'auth', receiver:true, receiverTypeId:0, args:[], returnType:named(1), owned:true, cached:true},
  2: {id:2, name:'conversation', receiver:true, receiverTypeId:0, args:[{name:'id',type:{kind:'scalar',name:'String'}}], returnType:named(1), owned:true},
  3: {id:3, name:'close', receiver:true, receiverTypeId:0, args:[], async:true, resourceClose:true},
  4: {id:4, name:'work', receiver:true, receiverTypeId:1, args:[], async:true, returnType:{kind:'scalar',name:'String'}},
  5: {id:5, name:'subscribe', receiver:true, receiverTypeId:0, args:[], returnType:named(2)},
  6: {id:6, name:'close', receiver:true, receiverTypeId:2, args:[], async:true, resourceClose:true},
  7: {id:7, name:'check', receiver:true, receiverTypeId:1, args:[], returnType:{kind:'scalar',name:'String'}},
  8: {id:8, name:'view', receiver:true, receiverTypeId:0, args:[], returnType:named(2), owned:true},
  9: {id:9, name:'create', kind:'Constructor', surface:'object', ownerTypeId:1, args:[], returnType:named(1)},
  10: {id:10, name:'events', receiver:true, receiverTypeId:0, args:[], returnType:{kind:'outputStream', inner:{kind:'scalar', name:'I32'}},
    streamResources:[{direction:'Output', path:'return', item:{kind:'scalar',name:'I32'}, error:null, slots:{OutputStreamNext:11, OutputStreamCancel:12}}]},
  13: {id:13, name:'lateEvents', receiver:true, receiverTypeId:0, args:[], async:true, returnType:{kind:'outputStream', inner:{kind:'scalar', name:'I32'}},
    streamResources:[{direction:'Output', path:'return', item:{kind:'scalar',name:'I32'}, error:null, slots:{OutputStreamNext:11, OutputStreamCancel:12}}]},
};
class Client {
  auth(...args) { return invokeObjectOperation(this, operations[1], args); }
  conversation(...args) { return invokeObjectOperation(this, operations[2], args); }
  close(...args) { return invokeObjectOperation(this, operations[3], args); }
  subscribe() { return invokeObjectOperation(this, operations[5]); }
  view() { return invokeObjectOperation(this, operations[8]); }
  events() { return invokeObjectOperation(this, operations[10]); }
  lateEvents() { return invokeObjectOperation(this, operations[13]); }
  [Symbol.asyncDispose]() { return this.close(); }
}
class Service {
  work() { return invokeObjectOperation(this, operations[4]); }
  check() { return invokeObjectOperation(this, operations[7]); }
}
class Subscription { close(...args) { return invokeObjectOperation(this, operations[6], args); } }
const descriptors = {componentId:0, operations, types:{0:{kind:'object'},1:{kind:'object'},2:{kind:'object'}}, classes:{
  0:{typeName:'Client',typeKey:'lifecycle.Client',public:true,publicClass:Client,resource:true},
  1:{typeName:'Service',typeKey:'lifecycle.Service',public:true,publicClass:Service},
  2:{typeName:'Subscription',typeKey:'lifecycle.Subscription',public:true,publicClass:Subscription,resource:true},
}};
let next=1, backendClosed=0;
const releases=[], closes=[], works=new Map();
const streamReleases=[], streamCancels=[];
const streamFactory=deferred();
let closeGate = deferred(), closeFailure=null, reenter=null;
const session = new BackendSession({
  invokeSync(id, args) { if(id===7) return 'ok'; return next++; },
  invokeAsync(id,args) {
    if (id===0) return Promise.resolve(next++);
    if (id===3 || id===6) { closes.push(args[0]); reenter?.(); return closeGate.promise.then(() => { if(closeFailure) throw closeFailure; }); }
    if (id===4) { const pending=deferred(); works.set(args[0],pending); return pending.promise; }
    if (id===11) return Promise.resolve({kind:'item',value:7});
    if (id===12) { streamCancels.push(args[0]); return Promise.resolve(); }
    if (id===13) return streamFactory.promise;
    throw Error(`unexpected operation ${id}`);
  },
  releaseObject(handle) { releases.push(handle); },
  releaseOutputStream(handle) { streamReleases.push(handle); },
  close() { backendClosed++; },
}, new Host(), {callControl:{create:90,cancel:91,release:92,operations:[]}});
const api=createFacade(session, descriptors);
const client=await api.Client.create(), other=await api.Client.create();
assert.equal('dispose' in client,false);
assert.equal('session' in client,false);
assert.equal(Symbol.dispose in client,false);
assert.throws(()=>disposeObject(client),{errorName:"UniffiResourceCloseRequired"});
const auth=client.auth();
for(let i=0;i<1000;i++) assert.equal(client.auth(),auth);
assert.equal(session.objects.size,3,'cached accessor must allocate once');
let service=client.conversation('busy');
const busyHandle=next-1;
const busy=service.work();
assert.equal(session.objects.size,4);
// Temporary parameterized services disappear while the client stays reachable.
for(let i=0;i<400;i++) client.conversation(String(i));
assert.equal(typeof global.gc,'function','run with --expose-gc');
for(let i=0;i<100 && session.objects.size>4;i++) { await tick(); global.gc(); await tick(); }
assert.equal(session.objects.size,4,'session and owner must not root parameterized service wrappers');
const fixture=api.Service.create(), fixtureHandle=next-1;
const fixtureCall=fixture.work();
disposeObject(fixture); disposeObject(fixture);
assert.throws(()=>fixture.check(),{errorName:'UniffiUseAfterDispose'});
assert.ok(!releases.includes(fixtureHandle),'reference release waits for its pending call');
works.get(fixtureHandle).resolve('fixture'); await fixtureCall;
assert.ok(releases.includes(fixtureHandle));
const events=client.events(), siblingEvents=other.events();
await events.next(); const eventsHandle=next-1;
await siblingEvents.next(); const siblingEventsHandle=next-1;
const late=client.lateEvents();
const lateResult=assert.rejects(late,{errorName:'UniffiObjectClosed'});
const subscription1=other.subscribe(), subscription2=other.subscribe();
const sub1Handle=next-2, sub2Handle=next-1;
const cancel1=subscription1.close();
await tick();
assert.deepEqual(closes,[sub1Handle]);
closeGate.resolve(); await cancel1;
assert.ok(releases.includes(sub1Handle));
assert.ok(!releases.includes(sub2Handle),'sibling subscription remains independent');
closeGate=deferred();
const view=client.view();
const cancelled=new AbortController(); cancelled.abort();
const closing=client.close({signal:cancelled.signal});
assert.equal(view.close(),closing,'owned resource joins owner cleanup');
assert.equal(closing,client.close());
assert.equal(closing,client[Symbol.asyncDispose]());
reenter=()=>assert.equal(client.close(),closing,'publish promise before native reentry');
await tick(); reenter=null;
assert.throws(()=>auth.check(),{errorName:'UniffiObjectClosed'});
closeGate.resolve(); await tick();
assert.ok(!releases.includes(busyHandle),'owner close must retain in-flight service');
streamFactory.resolve(9001); await lateResult;
works.get(busyHandle).resolve('finished');
assert.equal(await busy,'finished');
await closing;
assert.ok(streamCancels.includes(eventsHandle));
assert.ok(streamReleases.includes(eventsHandle));
assert.ok(streamCancels.includes(9001));
assert.ok(streamReleases.includes(9001));
assert.ok(!streamCancels.includes(siblingEventsHandle));
await siblingEvents.cancel();
assert.equal(view.close(),closing,'child close stays idempotent after owner release');
assert.ok(releases.includes(busyHandle));
assert.throws(()=>auth.check(),{errorName:'UniffiObjectClosed'});
assert.equal(other.auth().check(),'ok','closing one client leaves same-session sibling live');
assert.equal(backendClosed,0,'client close must not close shared backend');
assert.equal(new Set(releases).size,releases.length,'release every lease exactly once');
// Failure remains observable and repeated close shares the same rejection.
closeGate=deferred(); closeFailure=Object.assign(new Error('cleanup failed'),{errorName:'CleanupProbe'});
const failing=other.close(); closeGate.resolve();
await assert.rejects(failing,{errorName:'CleanupProbe'});
assert.equal(other.close(),failing);
await assert.rejects(other.close(),{errorName:'CleanupProbe'});
closeFailure=null;
await subscription2.close();
assert.equal(session.objects.size,0);
await session.close(); assert.equal(backendClosed,1);

// Deferred browser-style session providers surface failures to every async caller.
let boots=0;
const initialization=deferred();
const provider=Object.assign(()=>{ boots++; return initialization.promise; },{sync(){ throw Object.assign(Error('init first'),{errorName:'UniffiNotInitialized'}); }});
const lazy=createFacade(provider,descriptors);
const construction=lazy.Client.create();
initialization.reject(Object.assign(Error('bad wasm'),{errorName:'BootProbe'}));
await assert.rejects(construction,{errorName:'BootProbe'});
assert.equal(boots,1);
console.log('lifecycle regressions passed');
