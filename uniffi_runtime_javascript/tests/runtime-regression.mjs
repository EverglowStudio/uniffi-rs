
import {
  BackendSession,
  Host,
  createFacade,
  invokeOperation,
  liftValue,
  lowerValue,
} from "./runtime.js";

// Every mock implements the current control protocol explicitly.
const callControl = { create: 64, cancel: 65, release: 66, operations: [0] };
const makeSession = (backend, host = new Host()) => {
  let nextId = 1;
  const contexts = new Map();
  const invokeSync = backend.invokeSync?.bind(backend);
  return new BackendSession({ ...backend, invokeSync(id, args) {
    if (id === callControl.create) { const call = nextId; nextId += 4096; return {kind: "value", value: call}; }
    if (id === callControl.cancel) { contexts.set(args[0], true); return {kind:"value",value:null}; }
    if (id === callControl.release) { contexts.delete(args[0]); return {kind:"value",value:null}; }
    return invokeSync?.(id, args);
  }}, host, {callControl});
};
const expectError = (fn, errorName) => {
  try {
    fn();
    throw Error(`expected ${errorName}`);
  } catch (error) {
    if (error.errorName !== errorName) throw error;
  }
};
const expectAsyncError = async (fn, errorName) => {
  try {
    await fn();
    throw Error(`expected ${errorName}`);
  } catch (error) {
    if (error.errorName !== errorName) throw error;
  }
};
const scalar = (name) => ({ kind: "scalar", name });
const defaultPolicy = { graceMs: 5000, onDeadline: "detach" };
const withPolicy = (session, policy) => { createFacade(session, { closePolicy: policy }); return session; };

let nativeRetains = 0;
let nativeReleases = 0;
const hostA = new Host();
hostA.retainCallback = () => { nativeRetains += 1; };
hostA.releaseCallback = () => { nativeReleases += 1; };
const sessionA = withPolicy(makeSession({
  invokeSync() {},
  async invokeAsync() {},
  close() {},
}, hostA), defaultPolicy);
const callbackA = sessionA.registerCallback(7, () => "A", { methods: { 0: { name: null } } });
const callbackLeaseA = sessionA.retainCallback(7, callbackA);
callbackLeaseA.release();
if (sessionA.callbacks.callbacks.size !== 0) throw Error("local callback cleanup did not run");
if (nativeReleases !== 0) throw Error("local cleanup called native release SPI");

const callbackA2 = sessionA.registerCallback(7, () => "A2", { methods: { 0: { name: null } } });
hostA.retainCallback(7, callbackA2);
if (nativeRetains !== 1 || sessionA.callbacks.callbacks.size !== 1) throw Error("host retain SPI");
if (hostA.invokeCallbackSync(7, callbackA2, 0, []) !== "A2") throw Error("host/session callback routing");
hostA.releaseCallback(7, callbackA2);
if (nativeReleases !== 1 || sessionA.callbacks.callbacks.size !== 0) throw Error("host release SPI");

expectError(() => makeSession({}, hostA), "UniffiHostSession");
expectError(() => sessionA.registerCallback(-1, () => 1, { methods: { 0: { name: null } } }), "UniffiCallbackType");
expectError(() => sessionA.registerCallback(1, () => 1), "UniffiCallbackMethod");
const noGuess = sessionA.registerCallback(1, () => 1, { methods: { 1: { name: null } } });
expectError(() => sessionA.invokeCallbackSync(1, noGuess, 0, []), "UniffiCallbackMethod");

let syncLease;
let syncStillRegistered = false;
const syncId = sessionA.registerCallback(2, () => {
  syncLease.release();
  syncStillRegistered = sessionA.callbacks.callbacks.has(`2:${syncId}`);
  return "sync";
}, { methods: { 0: { name: null } } });
syncLease = sessionA.retainCallback(2, syncId);
if (sessionA.invokeCallbackSync(2, syncId, 0, []) !== "sync" || !syncStillRegistered) throw Error("sync callback in-flight lease");
if (sessionA.callbacks.callbacks.has(`2:${syncId}`)) throw Error("sync callback cleanup after finally");

let releaseAsync;
let proceedAsync;
const asyncGate = new Promise((resolve) => { proceedAsync = resolve; });
let asyncStillRegistered = false;
const asyncId = sessionA.registerCallback(3, { wait: async () => {
  await asyncGate;
  releaseAsync.release();
  asyncStillRegistered = sessionA.callbacks.callbacks.has(`3:${asyncId}`);
  return "async";
} }, { methods: { 0: { name: "wait", async: true } }, reentrancy: "allowed" });
releaseAsync = sessionA.retainCallback(3, asyncId);
const asyncResult = sessionA.invokeCallbackAsync(3, asyncId, 0, 42, []);
await Promise.resolve();
if (!sessionA.callbacks.callbacks.has(`3:${asyncId}`)) throw Error("async callback in-flight lease start");
proceedAsync();
if (await asyncResult !== "async" || !asyncStillRegistered) throw Error("async callback in-flight lease");
if (sessionA.callbacks.callbacks.has(`3:${asyncId}`)) throw Error("async callback cleanup after finally");

const recordContext = { types: { 10: {
  kind: "record",
  fields: {
    rust: { type: scalar("I32"), rustDefault: true, default: undefined },
    literal: { type: scalar("I32"), rustDefault: false, default: 4 },
  },
} } };
const recordDescriptor = { kind: "named", name: "Defaults", typeId: 10 };
const defaultRecord = lowerValue({}, recordDescriptor, recordContext);
if (Object.keys(defaultRecord).length !== 1 || defaultRecord.literal !== 4) throw Error("record absent defaults");
expectError(() => lowerValue({ literal: undefined }, recordDescriptor, recordContext), "UniffiUndefined");

const customContext = { types: { 11: {
  kind: "custom",
  builtin: scalar("String"),
  fromCustom: (value) => value.public,
  intoCustom: (value) => `public:${value}`,
} } };
const customDescriptor = { kind: "named", name: "Custom", typeId: 11 };
if (lowerValue({ public: "x" }, customDescriptor, customContext) !== "x") throw Error("custom lower order");
if (liftValue("x", customDescriptor, null, customContext) !== "public:x") throw Error("custom lift order");

const bridgedBytes = liftValue([0, 127, 255], scalar("Bytes"));
if (!(bridgedBytes instanceof Uint8Array) || bridgedBytes.join(",") !== "0,127,255") {
  throw Error("contextBridge byte array lift");
}
expectError(() => liftValue([0, -1, 256], scalar("Bytes")), "UniffiBytesType");

let naturalCancel = 0;
let naturalRelease = 0;
let naturalFinish = 0;
const naturalSession = withPolicy(makeSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, new Host()), defaultPolicy);
const natural = naturalSession.createOutputStream({
  start: () => 17,
  next: () => ({ kind: "error", error: { errorName: "NaturalError", message: "done" } }),
  cancel: () => { naturalCancel += 1; },
  release: () => { naturalRelease += 1; },
  onClose: () => { naturalFinish += 1; },
});
await expectAsyncError(() => natural.next(), "NaturalError");
if (naturalCancel !== 0 || naturalRelease !== 1 || naturalFinish !== 1) throw Error("natural output error cleanup");
await natural.cancel();
if (naturalCancel !== 0 || naturalRelease !== 1 || naturalFinish !== 1) throw Error("natural output error re-cleanup");
await naturalSession.close();


let transportCancel = 0;
let transportRelease = 0;
const transportSession = withPolicy(makeSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, new Host()), defaultPolicy);
const transport = transportSession.createOutputStream({
  start: () => 18,
  next: () => ({ kind: "not-a-step" }),
  cancel: () => { transportCancel += 1; },
  release: () => { transportRelease += 1; },
});
await expectAsyncError(() => transport.next(), "UniffiStreamProtocolError");
if (transportCancel !== 1 || transportRelease !== 1) throw Error("transport output cancellation");
await transportSession.close();

const voidSession = withPolicy(makeSession({
  invokeSync: () => ({ kind: "value", value: null }),
  async invokeAsync() { return { kind: "value", value: "ignored" }; },
  close() {},
}, new Host()), defaultPolicy);
const voidDescriptor = { name: "void", id: 0, args: [], returnType: null, async: false };
if (invokeOperation(voidSession, voidDescriptor, []) !== undefined) throw Error("void sync return");
voidDescriptor.async = true;
if (await invokeOperation(voidSession, voidDescriptor, []) !== undefined) throw Error("void async return");
await voidSession.close();

const hostB = new Host();
const sessionB = withPolicy(makeSession({ invokeSync() {}, async invokeAsync() {}, close() {} }, hostB), defaultPolicy);
const callbackB = sessionB.registerCallback(7, () => "B", { methods: { 0: { name: null } } });
try {
  hostA.invokeCallbackSync(7, callbackB, 0, []);
  throw Error("host registries crossed");
} catch (error) {
  if (error.errorName !== "UniffiCallbackMissing") throw error;
}
if (hostB.invokeCallbackSync(7, callbackB, 0, []) !== "B") throw Error("host B callback routing");
await sessionB.close();
await sessionA.close();

"ok";
// S01: mock the Rust terminal decision and cleanup gate independently.
import assert from "node:assert/strict";
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const turn = () => new Promise((resolve) => setImmediate(resolve));
async function assertPending(promise, label) {
  let settled = false;
  promise.then(() => { settled = true; }, () => { settled = true; });
  await turn();
  assert.equal(settled, false, label);
}
const cancelledEnvelope = () => ({kind:"error", error:{domain:"internal",errorName:"UniffiCancelled",message:"call cancelled"}});
function controlled(invoke, managed = [0, 1]) {
  const events = [];
  const calls = new Map();
  let next = 1;
  const control = {create:8,cancel:9,release:10,operations:managed};
  const backend = {
    invokeSync(id, args) {
      if (id === control.create) { const callId = next; next += 4096; events.push(["create",callId]); return {kind:"value",value:callId}; }
      if (id === control.cancel) {
        const callId = args[0];
        const entry = calls.get(callId);
        if (entry) entry.cancelled = true;
        else calls.set(callId, null);
        events.push(["cancel",callId]); return {kind:"value",value:null};
      }
      if (id === control.release) { if (calls.get(args[0]) === null) calls.delete(args[0]); events.push(["release",args[0]]); return {kind:"value",value:null}; }
      events.push(["sync",id,args]); return {kind:"value",value:args[0]};
    },
    async invokeAsync(id,args) {
      events.push(["invoke",id,args]);
      if (!managed.includes(id)) return invoke(id,args,null);
      const callId = args.at(-1);
      const entry = {callId, cancelled:calls.has(callId)};
      calls.set(callId,entry);
      try { return await invoke(id,args,entry); }
      finally { calls.delete(callId); events.push(["rust-drop",callId]); }
    },
    close() { events.push(["engine-close"]); },
    releaseOutputStream(handle) { events.push(["stream-release",handle]); },
  };
  const session = new BackendSession(backend,new Host(),{callControl:control});
  return {session,events,calls,backend};
}

assert.throws(() => new BackendSession({},new Host()), (e) => e.errorName === "UniffiCallControl");
assert.throws(() => new BackendSession({},new Host(),{callControl:{create:8,cancel:9,release:10}}), (e) => e.errorName === "UniffiCallControl");

{
  const clean = deferred();
  const {session,events,calls} = controlled(async (_id,args,entry) => {
    assert.deepEqual(args.slice(0,-1),["business"]);
    await clean.promise;
    return entry.cancelled ? cancelledEnvelope() : {kind:"value",value:42};
  });
  const controller = new AbortController();
  const pending = session.invokeAsync(0,["business"],{signal:controller.signal});
  controller.abort();
  assert.deepEqual(events.slice(0,2),[["create",1],["cancel",1]]);
  await assertPending(pending,"cancel waits Rust cleanup");
  clean.resolve();
  await assert.rejects(pending,(e) => e.errorName === "UniffiCancelled" && e.domain === "internal");
  assert.equal(calls.size,0);
  assert.equal(events.filter((e)=>e[0]==="release").length,1);
  await session.close();
}
{
  const {session,events} = controlled(() => { throw Error("preaborted business ran"); });
  const controller=new AbortController();controller.abort();
  await assert.rejects(session.invokeAsync(0,[],{signal:controller.signal}),(e)=>e.errorName==="UniffiCancelled");
  assert.deepEqual(events,[]);
  await session.close();
}
{
  const cleanup=deferred();
  const {session,events}=controlled(async ()=>{ await cleanup.promise; return {kind:"value",value:"committed"}; });
  const controller=new AbortController();
  const pending=session.invokeAsync(0,[],{signal:controller.signal});
  await turn();controller.abort();cleanup.resolve();
  assert.equal(await pending,"committed","late abort must preserve Rust completion");
  controller.abort();
  assert.equal(events.filter((e)=>e[0]==="cancel").length,1);
  await session.close();
}
{
  const gate=deferred();
  const {session,events}=controlled(async (_id,_args,entry)=>{ await gate.promise; return entry.cancelled ? cancelledEnvelope() : 2; });
  const one=new AbortController();
  const first=session.invokeAsync(0,[],{signal:one.signal});
  const second=session.invokeAsync(0,[]);
  one.abort(); gate.resolve();
  await assert.rejects(first,(e)=>e.errorName==="UniffiCancelled");
  assert.equal(await second,2);
  assert.deepEqual(events.filter(e=>e[0]==="cancel"),[["cancel",1]]);
  await session.close();
}
{
  const cleanup=deferred();
  const {session,events}=controlled(async (_id,_args,entry)=>{ await cleanup.promise; return entry.cancelled ? cancelledEnvelope() : 1; });
  const pending=session.invokeAsync(0,[]);
  const close=session.close();
  assert.equal(close,session.close());
  await new Promise(resolve=>setTimeout(resolve,15));
  await assertPending(close,"close cannot use old grace deadline");
  assert.equal(events.some(e=>e[0]==="engine-close"),false);
  cleanup.resolve();
  await assert.rejects(pending,(e)=>e.errorName==="UniffiCancelled");
  await close;
  assert.equal(events.at(-1)[0],"engine-close");
  assert.ok(events.findIndex(e=>e[0]==="release")<events.findIndex(e=>e[0]==="engine-close"));
}
{
  const {session}=controlled(()=>new Promise(()=>{}));
  const pending=session.invokeAsync(0,[]);
  const close=session.close();
  session.environmentDestroyed();
  await assert.rejects(close,(e)=>e.errorName==="UniffiEnvironmentDestroyed");
  await assert.rejects(pending,(e)=>e.errorName==="UniffiEnvironmentDestroyed");
}
{
  let received;
  const {session}=controlled((_id,args)=>{ received=args;return {kind:"value",value:args[0]}; });
  const descriptor={name:"defaulted",id:0,args:[{name:"count",type:scalar("I32"),default:9}],returnType:scalar("I32"),async:true};
  assert.equal(await invokeOperation(session,descriptor,[undefined,{signal:new AbortController().signal}]),9);
  assert.deepEqual(received,[9,1]);
  const record={kind:"named",typeId:12,name:"Business"};
  const recordDescriptor={...descriptor,args:[{name:"business",type:record}],returnType:record,context:{types:{12:{kind:"record",fields:{signal:{type:scalar("String")}}}}}};
  assert.deepEqual(await invokeOperation(session,recordDescriptor,[{signal:"business-data"}]),{signal:"business-data"});
  assert.throws(()=>invokeOperation(session,{...descriptor,async:false},[1,{}]),e=>e.errorName==="UniffiArgumentCount");
  await session.close();
}
{
  const received = [];
  const {session} = controlled((_id, args, call) => {
    assert.ok(call, "last argument must identify the allocated context");
    received.push(args);
    return {kind:"value",value:null};
  });
  const descriptor = {name:"rustDefault",id:0,args:[
    {name:"first",type:scalar("I32"),rustDefault:true},
    {name:"second",type:scalar("I32"),rustDefault:true},
  ],returnType:null,async:true};
  await invokeOperation(session,descriptor,[]);
  await invokeOperation(session,descriptor,[7,undefined,{signal:new AbortController().signal}]);
  await invokeOperation(session,{...descriptor,receiver:true},[42]);
  assert.deepEqual(received,[[undefined,undefined,1],[7,undefined,2],[42,undefined,undefined,3]],
    "omitted defaults reserve fixed business slots before callId, including the receiver");
  await session.close();
}
{
  const {session}=controlled(()=>cancelledEnvelope());
  const descriptor={name:"fallible",id:0,args:[],returnType:null,async:true,throws:{typeId:99,name:"BusinessError"},context:{types:{99:{kind:"enum",variants:{Oops:{fields:{}}}}}}};
  await assert.rejects(invokeOperation(session,descriptor,[]),e=>e.errorName==="UniffiCancelled");
  await session.close();
}
{
  const cleanup=deferred();let returnCount=0;let released=0;
  const {session}=controlled(()=>({kind:"value",value:7}));
  const frame=session.beginCallFrame();
  session.createInputStream({next:()=>({done:false,value:1}),cancel:async()=>{returnCount++;await cleanup.promise;}},{onClose:()=>{released++;}});
  session.leaveCallFrame(frame);
  const closing=session.endCallFrame(frame);
  assert.equal(closing,session.endCallFrame(frame));
  await assertPending(closing,"input frame waits producer return");
  assert.equal(returnCount,1);assert.equal(released,0);
  cleanup.resolve();await closing;
  assert.equal(released,1);await session.close();
}
{
  const callbackGate=deferred();
  const {session}=controlled(()=>0);
  const frame=session.beginCallFrame();
  const id=session.registerCallback(40,async()=>{await callbackGate.promise;return 8;},{retention:"scoped",methods:{0:{name:null,async:true}}});
  session.leaveCallFrame(frame);
  const callback=session.invokeCallbackAsync(40,id,0,1,[]);
  const closing=session.endCallFrame(frame);
  await assertPending(closing,"callback frame waits active invocation");
  callbackGate.resolve();assert.equal(await callback,8);await closing;
  assert.equal(session.callbacks.callbacks.size,0);await session.close();
}
{
  const pull=deferred(),cleanup=deferred();let returns=0;
  const {session}=controlled(()=>0);
  const marker=session.createInputStream({next:()=>pull.promise,cancel:async()=>{returns++;pull.resolve({done:true});await cleanup.promise;}});
  const pending=marker.next();await turn();
  const close=marker.cancel();assert.equal(close,marker.cancel());
  await assertPending(close,"producer cleanup gate");
  await assertPending(pending,"cancelled input next must wait cleanup");
  assert.equal(returns,1);cleanup.resolve();await close;await pending;await session.close();
}
{
  const next=deferred(),cleanup=deferred();let pulls=0,cancels=0;
  const {session,events}=controlled(async(id,args,entry)=>{
    assert.equal(entry,null,"stream slot must not create call context");
    assert.deepEqual(args,[71]);
    if(id===5){pulls++;return next.promise;}
    if(id===6){cancels++;next.resolve({kind:"done"});await cleanup.promise;return null;}
    throw Error("unexpected operation");
  },[]);
  const stream=session.createOutputStream({handle:71,next:(handle)=>session.invokeAsync(5,[handle]),cancel:(handle)=>session._invokeCleanupAsync(6,[handle])});
  await turn();assert.equal(pulls,0,"no prefetch");
  const pending=stream.next();await turn();assert.equal(pulls,1);
  await assert.rejects(stream.next(),e=>e.errorName==="UniffiStreamConcurrentNext");
  const close=stream.cancel();assert.equal(close,stream.cancel());
  await assertPending(close,"stream close must await native cancel cleanup");
  assert.equal(cancels,1);assert.equal(events.some(e=>e[0]==="stream-release"),false);
  cleanup.resolve();await close;assert.deepEqual(await pending,{done:true,value:undefined});
  assert.equal(events.filter(e=>e[0]==="stream-release").length,1);
  assert.equal(events.some(e=>e[0]==="create"),false);
  await session.close();
}
{
  const factory=deferred();let pulls=0,cancels=0;
  const {session,events}=controlled(async(id,args)=>{
    if(id===0){await factory.promise;return 88;}
    assert.deepEqual(args,[88]);
    if(id===5){pulls++;return {kind:"done"};}
    if(id===6){cancels++;return null;}
  },[0]);
  const descriptor={name:"asyncFactory",id:0,args:[],kind:"OutputStreamStart",async:true,returnType:{kind:"outputStream",inner:scalar("I32")},streamResources:[{direction:"Output",path:"return",item:scalar("I32"),slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
  const result=invokeOperation(session,descriptor,[]);
  assert.ok(result instanceof Promise);
  await assertPending(result,"async factory must await native creation");
  factory.resolve();const stream=await result;
  assert.equal(pulls,0);await stream.return();
  assert.equal(cancels,1,"created but unconsumed stream owns a native handle");
  assert.equal(events.filter(e=>e[0]==="stream-release").length,1);
  await session.close();
}
{
  const factory=deferred(),cleanup=deferred();
  const {session,events}=controlled(async(id)=>{
    if(id===0){await factory.promise;return 89;}
    if(id===6){await cleanup.promise;return null;}
    throw Error("unexpected pull");
  },[0]);
  const descriptor={name:"closingFactory",id:0,args:[],kind:"OutputStreamStart",async:true,returnType:{kind:"outputStream",inner:scalar("I32")},streamResources:[{direction:"Output",path:"return",item:scalar("I32"),slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
  const result=invokeOperation(session,descriptor,[]);result.catch(()=>{});
  const close=session.close();factory.resolve();
  await assertPending(close,"close waits newly committed factory handle release");
  assert.equal(events.some(e=>e[0]==="engine-close"),false);
  cleanup.resolve();await assert.rejects(result,e=>e.errorName==="UniffiSessionClosed");await close;
  assert.ok(events.findIndex(e=>e[0]==="stream-release")<events.findIndex(e=>e[0]==="engine-close"));
}
{
  const gate=deferred();let cleaned=0;
  const {session}=controlled(()=>0);
  const frame=session.beginCallFrame();
  session.createInputStream({next:()=>({done:false,value:1}),cancel:()=>Promise.reject(Error("first cleanup failed"))});
  session.createInputStream({next:()=>({done:false,value:2}),cancel:async()=>{await gate.promise;cleaned++;}});
  session.leaveCallFrame(frame);
  const closing=session.endCallFrame(frame);
  await assertPending(closing,"one cleanup failure must not skip a pending sibling");
  gate.resolve();await assert.rejects(closing,/first cleanup failed/);assert.equal(cleaned,1);
  await assert.rejects(session.close(),/first cleanup failed/);
}
{
  const gates=[deferred(),deferred()],callbackGate=deferred();
  const {session}=controlled(async(id)=>{await gates[id].promise;return null;});
  const callback=session.registerCallback(60,async()=>{await callbackGate.promise;return ()=>3;},{
    methods:{0:{name:null,async:true,returnType:{kind:"named",name:"Nested",typeId:61},callbackContracts:{return:{callbackTypeId:61,retention:"retained"}}}},
    context:{types:{61:{kind:"callback",methods:{0:{name:null,async:false}}}}}
  });
  const descriptor={name:"pending",id:0,args:[],returnType:null,async:true};
  const first=invokeOperation(session,descriptor,[]);
  const second=invokeOperation(session,{...descriptor,id:1},[]);
  const returned=session.invokeCallbackAsync(60,callback,0,1,[]);
  callbackGate.resolve();const nestedId=await returned;
  assert.equal(session.callbacks.callbacks.get(`61:${nestedId}`).frameRefs,0,"callback return must not join another suspended call frame");
  gates[1].resolve();await second;
  assert.ok(session.callbacks.callbacks.has(`61:${nestedId}`),"unrelated call cleanup must not release callback return");
  gates[0].resolve();await first;await session.close();
}
{
  let polls = 0;
  const {session,events,calls} = controlled((_id,_args,entry)=>{ assert.ok(entry); polls++; return 1; });
  for (let i = 0; i < 4097; i++) assert.equal(await session.invokeAsync(0,[]),1);
  assert.equal(polls,4097,"every call enters its own Rust context");
  assert.deepEqual(events.filter(e=>e[0]==="create"),[["create",1],["create",4097]]);
  assert.equal(events.filter(e=>e[0]==="cancel" || e[0]==="release").length,0);
  assert.equal(calls.size,0);
  await session.close();
}
{
  for (const first of [0,-1,1.5,Infinity,NaN,Number.MAX_SAFE_INTEGER - 4094]) {
    const {session,backend} = controlled(()=>{throw Error("invalid block entered business");});
    backend.invokeSync=()=>({kind:"value",value:first});
    await assert.rejects(session.invokeAsync(0,[]),e=>e.errorName==="UniffiCallControl");
    await session.close();
  }
  const seen=[];
  const {session,backend} = controlled((_id,args)=>{seen.push(args.at(-1));return 1;});
  backend.invokeSync=()=>({kind:"value",value:Number.MAX_SAFE_INTEGER-4095});
  for (let i=0;i<4096;i++) await session.invokeAsync(0,[]);
  assert.equal(seen[0],Number.MAX_SAFE_INTEGER-4095);
  assert.equal(seen.at(-1),Number.MAX_SAFE_INTEGER);
  await assert.rejects(session.invokeAsync(0,[]),e=>e.errorName==="UniffiCallControl");
  await session.close();
}
{
  const {session,events,calls} = controlled(()=>{throw Error("business failure");});
  await assert.rejects(session.invokeAsync(0,[]),/business failure/);
  assert.equal(calls.size,0);
  assert.equal(events.some(e=>e[0]==="release"),false);
  await session.close();
}
{
  const controller=new AbortController();
  const {session,events,calls,backend}=controlled(()=>1);
  const invoke=backend.invokeAsync.bind(backend);
  backend.invokeAsync=(...args)=>invoke(...args).then(value=>{
    assert.equal(calls.size,0,"Rust already committed and dropped its call lease");
    controller.abort();
    assert.equal(calls.get(args[1].at(-1)),null,"late cancellation inserts only a marker");
    return value;
  });
  assert.equal(await session.invokeAsync(0,[],{signal:controller.signal}),1);
  assert.equal(calls.size,0,"JS settlement removes late cancellation marker");
  assert.equal(events.filter(e=>e[0]==="release").length,1);
  await session.close();
}
console.log("S01 runtime protocol: 22 scenarios passed; legacy callback/conversion/stream assertions passed");
