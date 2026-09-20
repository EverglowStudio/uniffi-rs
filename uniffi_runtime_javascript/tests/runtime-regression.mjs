
import {
  BackendSession,
  UniffiError,
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
{
  const cleanup=deferred();let cancelled=0,released=0,pulls=0;
  const session=makeSession({invokeAsync:async(id)=>{
    if(id===5) { pulls++;return {kind:"item",value:9}; }
    if(id===6) { cancelled++;await cleanup.promise;return null; }
  },close() {}});
  session.releaseInputStream=async()=>{released++;};
  const type={kind:"inputStream",inner:scalar("I32")};
  const operation={streamArgumentPath:"argument[0]",streamResources:[{direction:"Input",path:"argument[0]",item:scalar("I32"),error:null,slots:{InputStreamPull:5,InputStreamCancel:6}}]};
  const stream=liftValue(7,type,session,null,operation);
  const consuming=(async()=>{for await(const item of stream) { assert.equal(item,9);break; }})();
  await assertPending(consuming,"for await break waits producer close");
  assert.equal(pulls,1);assert.equal(cancelled,1);assert.equal(released,0);
  const close=stream.cancel();assert.equal(close,stream.cancel());
  cleanup.resolve();await consuming;await close;
  assert.equal(released,1);await session.close();
}
{
  class FactoryError extends UniffiError {
    constructor(message,variant,data) { super({message,variant,data,errorName:"FactoryError"}); }
  }
  const session=makeSession({invokeSync:()=>({kind:"error",error:{errorName:"FactoryError",variant:"Failed",data:null,message:"factory failed"}}),close() {}});
  const descriptor={name:"syncFactory",id:0,args:[],kind:"OutputStreamStart",async:false,
    returnType:{kind:"outputStream",inner:scalar("I32")},throws:{typeId:71,name:"FactoryError"},
    context:{types:{71:{kind:"enum",variants:{Failed:{fields:{}}}}},errorClasses:{71:FactoryError}},
    streamResources:[{direction:"Output",path:"return",item:scalar("I32"),error:null,slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
  const stream=invokeOperation(session,descriptor,[]);
  await assert.rejects(stream.next(),e=>e instanceof FactoryError && e.variant==="Failed");
  await session.close();
}
// An object's stream factory can move its input into the returned subscription.
// The call frame then belongs to that subscription, even for a sync method.
for (const asyncMethod of [false, true]) {
  const cleanup = deferred();
  let inputHandle, pulls = 0, returns = 0, factoryCalls = 0;
  const factory = (args) => {
    assert.equal(args[0], 901, "stream method preserves receiver dispatch");
    inputHandle = args[1];
    factoryCalls++;
    return {kind:"value", value:{handle:71}};
  };
  const session = makeSession({
    invokeSync(id, args) { assert.equal(id, 0); return factory(args); },
    async invokeAsync(id, args) {
      if (id === 0) return factory(args);
      assert.deepEqual(args, [{handle:71}]);
      if (id === 5) return session.pullInputStream(inputHandle);
      assert.equal(id, 6);
      return null;
    },
    releaseOutputStream() {}, close() {},
  });
  const descriptor = {name:"echo", id:0, kind:"Method", receiver:true,
    async:asyncMethod, args:[{name:"input", type:{kind:"inputStream",inner:scalar("I32")}}],
    returnType:{kind:"outputStream",inner:scalar("I32")},
    streamResources:[{direction:"Output",path:"return",item:scalar("I32"),error:null,
      slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
  const producer = { [Symbol.asyncIterator]() { return {
    next() { pulls++; return {done:false,value:pulls}; },
    async return() { returns++; await cleanup.promise; return {done:true}; },
  }; } };
  let result;
  try {
    result = invokeOperation(session, descriptor, [901, producer]);
    let delivered = !asyncMethod;
    if (asyncMethod) result.then(() => { delivered = true; }, () => {});
    await turn();
    assert.equal(returns, 0, "factory completion must not cancel its moved input");
    assert.equal(delivered, true, "stream delivery must not wait for input cleanup");
    assert.equal(pulls, 0, "factory does not prefetch input");
    const stream = await result;
    assert.deepEqual(await stream.next(), {done:false,value:1});
    assert.deepEqual(await stream.next(), {done:false,value:2});
    assert.equal(factoryCalls, 1);
    const closing = stream.return();
    await assertPending(closing, "method stream close waits for producer return");
    assert.equal(returns, 1);
    cleanup.resolve();
    await closing;
    assert.equal(session.inputStreams.size, 0);
    await stream.return();
    assert.equal(returns, 1, "repeated close shares producer cleanup");
  } finally {
    cleanup.resolve();
    await Promise.resolve(result).catch(() => {});
    await session.close();
  }
}
for (const asyncMethod of [false, true]) {
  const cleanup = deferred();
  let returns = 0;
  const failure = {kind:"error",error:{errorName:"MethodFactoryError",message:"factory failed"}};
  const session = makeSession({invokeSync:() => failure, invokeAsync:async() => failure, close() {}});
  const descriptor = {name:"failingEcho",id:0,kind:"Method",receiver:true,async:asyncMethod,
    args:[{name:"input",type:{kind:"inputStream",inner:scalar("I32")}}],
    returnType:{kind:"outputStream",inner:scalar("I32")},
    streamResources:[{direction:"Output",path:"return",item:scalar("I32"),error:null,
      slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
  const producer = {next() { throw Error("failed factory must not pull input"); },
    async cancel() { returns++; await cleanup.promise; }};
  const result = invokeOperation(session, descriptor, [901, producer]);
  const failing = asyncMethod ? result : result.next();
  await assertPending(failing, "method factory failure waits for its input cleanup");
  assert.equal(returns, 1);
  cleanup.resolve();
  await assert.rejects(failing, error => error.errorName === "MethodFactoryError");
  assert.equal(session.inputStreams.size, 0);
  assert.equal(session.outputStreams.size, 0);
  await session.close();
}
{
  const cleanup = deferred();
  let returns = 0, factoryCalls = 0;
  const session = makeSession({invokeSync() { factoryCalls++; return {kind:"value",value:71}; },close() {}});
  const descriptor = {name:"unconsumedEcho",id:0,kind:"Method",receiver:true,async:false,
    args:[{name:"input",type:{kind:"inputStream",inner:scalar("I32")}}],
    returnType:{kind:"outputStream",inner:scalar("I32")},
    streamResources:[{direction:"Output",path:"return",item:scalar("I32"),error:null,
      slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
  const producer = {next() { throw Error("unconsumed method must not pull input"); },
    async cancel() { returns++; await cleanup.promise; }};
  const stream = invokeOperation(session, descriptor, [901, producer]);
  const closing = stream.return();
  await assertPending(closing, "unstarted method stream still owns input cleanup");
  assert.equal(factoryCalls, 0);
  assert.equal(returns, 1);
  cleanup.resolve();
  await closing;
  assert.equal(session.inputStreams.size, 0);
  await session.close();
}
for (const carrier of [71, {handle:71}]) {
  const seen=[];
  const session=makeSession({
    invokeAsync:async(id,args)=>{seen.push([id,args[0]]);return {kind:"item",value:4};},
    cancelOutputStream:()=>{},releaseOutputStream:()=>{},close() {},
  });
  const descriptor={kind:"outputStream",inner:scalar("I32")};
  const operation={returnType:descriptor,streamResources:[{direction:"Output",path:"return",item:scalar("I32"),error:null,slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
  const stream=liftValue(carrier,descriptor,session,null,operation);
  assert.deepEqual(await stream.next(),{done:false,value:4});
  await stream.return();
  assert.equal(seen[0][1],carrier,"next retains the backend lease carrier");
  assert.equal(seen[1][1],carrier,"cancel retains the backend lease carrier");
  await session.close();
}
function controlled(invoke, managed = [0, 1], registration = null) {
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
      if (registration) await registration;
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
  const registration = deferred();
  const {session,events,calls} = controlled(async (_id,args,entry) => {
    assert.deepEqual(args.slice(0,-1),["business"]);
    await clean.promise;
    return entry.cancelled ? cancelledEnvelope() : {kind:"value",value:42};
  }, [0, 1], registration.promise);
  const controller = new AbortController();
  const pending = session.invokeAsync(0,["business"],{signal:controller.signal});
  controller.abort();
  assert.deepEqual(events.slice(0,2),[["create",1],["cancel",1]]);
  registration.resolve();
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
  const pending=stream.next();pending.catch(()=>{});await turn();assert.equal(pulls,1);
  await assert.rejects(stream.next(),e=>e.errorName==="UniffiStreamConcurrentNext");
  const close=stream.cancel();assert.equal(close,stream.cancel());
  await assertPending(close,"stream close must await native cancel cleanup");
  await assertPending(pending,"interrupted next must await native cancel cleanup");
  assert.equal(cancels,1);assert.equal(events.some(e=>e[0]==="stream-release"),false);
  cleanup.resolve();await close;await assert.rejects(pending,e=>e.errorName==="UniffiCancelled");
  assert.deepEqual(await stream.next(),{done:true,value:undefined});
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
// Backend entry may immediately invoke a host callback which closes the session.
// Its call promise must already be visible to teardown before that reentry.
{
  const cleanup = deferred();
  let close;
  const {session, events} = controlled((_id, _args, entry) => {
    close = session.close();
    return cleanup.promise.then(() => entry.cancelled ? cancelledEnvelope() : 1);
  });
  const pending = session.invokeAsync(0, []);
  assert.ok(close, "backend entry is immediate and reentrant");
  assert.equal(session.calls.size, 1);
  await assertPending(close, "reentrant close retains the active call through cleanup");
  assert.equal(events.some(event => event[0] === "engine-close"), false);
  cleanup.resolve();
  await assert.rejects(pending, error => error.errorName === "UniffiCancelled");
  await close;
  assert.equal(session.calls.size, 0);
  assert.equal(session._waiters.size, 0);
  assert.equal(events.filter(event => event[0] === "release").length, 1);
}
{
  const {session, backend} = controlled(() => 0);
  backend.invokeAsync = () => { throw Error("synchronous backend failure"); };
  for (const operation of [0, 5]) {
    await assert.rejects(session.invokeAsync(operation, []), error => error.message === "synchronous backend failure");
    assert.equal(session.calls.size, 0);
    assert.equal(session._waiters.size, 0);
  }
  await session.close();
}
for (const destroyAt of ["reserve", "invoke"]) {
  let session;
  session = new BackendSession({
    invokeSync() {
      if (destroyAt === "reserve") session.environmentDestroyed();
      return {kind: "value", value: 1};
    },
    invokeAsync() {
      assert.equal(destroyAt, "invoke", "do not enter the backend after destruction");
      session.environmentDestroyed();
      return new Promise(() => {});
    },
  }, new Host(), {callControl: {create: 8, cancel: 9, release: 10, operations: [0]}});
  await assert.rejects(session.invokeAsync(0, []), error => error.errorName === "UniffiEnvironmentDestroyed");
  assert.equal(session.calls.size, 0);
  assert.equal(session._waiters.size, 0);
  assert.equal(session._activeGuards, 0);
}
const inputLifecycleChecks = await Promise.allSettled([
  (async () => {
    const cleanup = deferred();
    let entered = 0, cleaned = 0, released = 0;
    const {session} = controlled(() => 0);
    const source = (async function* () {
      try { yield 1; yield 2; }
      finally { entered++; await cleanup.promise; cleaned++; }
    })();
    const marker = session.createInputStream(source, {onClose: () => { released++; }});
    try {
      assert.deepEqual(await marker.next(), {kind: "item", value: 1});
      const closing = marker.cancel();
      await turn();
      assert.equal(entered, 1, "standard async generator return must enter finally");
      await assertPending(closing, "generator close must await finally cleanup");
      assert.equal(cleaned, 0);
      assert.equal(released, 0);
      cleanup.resolve();
      await closing;
      await marker.cancel();
      await marker.release();
      assert.deepEqual([entered, cleaned, released], [1, 1, 1]);
    } finally {
      cleanup.resolve();
      await source.return();
      await session.close();
    }
  })(),
  (async () => {
    let cancelled = 0, released = 0;
    const host = new Host();
    host.releaseInputStream = () => { released++; };
    const session = makeSession({close() {}}, host);
    for (let index = 0; index < 128; index++) {
      const marker = session.createInputStream({
        next: () => ({done: false, value: 1}),
        cancel: () => { cancelled++; },
      });
      await marker.next();
      await marker.cancel();
      await marker.release();
      await marker.cancel();
      await marker.release();
      await host.cancelInputStream(marker.handle);
      await host.releaseInputStream(marker.handle);
    }
    assert.deepEqual([cancelled, released], [128, 128], "retired input handles never release the producer twice");
    assert.deepEqual([
      session.inputStreams.size, session.inputHost.slots.size,
      session.inputHost.closedHandles.size, session.inputCancelled.size,
      session.inputReleased.size,
    ], [0, 0, 0, 0, 0], "completed input subscriptions retain no historical promises");
    await session.close();
  })(),
]);
for (const result of inputLifecycleChecks) if (result.status === "rejected") console.error(result.reason);
assert.equal(inputLifecycleChecks.filter(result => result.status === "rejected").length, 0, "input lifecycle checks");
{
  const raw = deferred(), cleanup = deferred();
  let closing, returned = 0, released = 0;
  const {session} = controlled(() => 0);
  const marker = session.createInputStream({
    next() { closing = marker.cancel(); return raw.promise; },
    async cancel() { returned++; await cleanup.promise; },
  }, {onClose: () => { released++; }});
  const reading = marker.next();
  assert.ok(closing, "input producer enters directly and may reenter cancel");
  await assertPending(closing, "reentrant input close joins its published raw next");
  cleanup.resolve();
  await assertPending(closing, "producer return cannot release an active raw next");
  assert.equal(released, 0);
  raw.resolve({done:false,value:1});
  assert.deepEqual(await reading, {kind:"done"});
  await closing;
  assert.deepEqual([returned,released], [1,1]);
  await session.close();
}
{
  const entered = deferred(), raw = deferred(), cleanup = deferred(), frame = deferred();
  let closing, cancelled = 0, released = 0, frameEntered = 0;
  const {session} = controlled(() => 0);
  const stream = session.createOutputStream({
    handle: 3,
    next() { closing = stream.cancel(); entered.resolve(); return raw.promise; },
    async cancel() { cancelled++; await cleanup.promise; },
    release() { released++; },
    async onClose() { frameEntered++; await frame.promise; },
  });
  const reading = assert.rejects(stream.next(), error => error.errorName === "UniffiCancelled");
  await entered.promise;
  await assertPending(closing, "output backend reentry joins the raw next before release");
  cleanup.resolve();
  await assertPending(closing, "output cleanup cannot release an active raw next");
  assert.deepEqual([released,frameEntered], [0,0]);
  raw.resolve({kind:"item",value:1});
  await assertPending(closing, "output close still joins its input call frame");
  assert.deepEqual([released,frameEntered], [1,1]);
  frame.resolve();
  await Promise.all([reading,closing]);
  assert.equal(cancelled,1);
  assert.equal(session.outputStreams.size,0);
  assert.equal(session._waiters.size,0);
  await session.close();
}
const outputEntryChecks = await Promise.allSettled([
  (async () => {
    for (const factory of [true, false]) {
      const raw = deferred(), entered = deferred(), cleanup = deferred();
      let stream, closing, guardsAtEntry, cancels = 0, releases = 0;
      const session = makeSession({
        invokeSync() { return {kind:"value",value:9}; },
        invokeAsync(id) {
          if (id === 5) {
            guardsAtEntry = session._activeGuards;
            closing = stream.cancel();
            entered.resolve();
            return raw.promise;
          }
          if (id === 6) { cancels++; return cleanup.promise; }
          throw Error("unexpected stream operation");
        },
        releaseOutputStream() { releases++; }, close() {},
      });
      const type = {kind:"outputStream",inner:scalar("I32")};
      const descriptor = {name:"oneGuard",id:2,args:[],kind:"OutputStreamStart",async:false,
        returnType:type,streamResources:[{direction:"Output",path:"return",item:scalar("I32"),error:null,
          slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
      stream = factory ? invokeOperation(session,descriptor,[]) : liftValue(9,type,session,null,descriptor);
      const reading = assert.rejects(stream.next(), error=>error.errorName === "UniffiCancelled");
      await entered.promise;
      await assertPending(closing,"generated stream close joins its prepublished raw guard");
      cleanup.resolve({kind:"value"});
      await assertPending(closing,"cleanup cannot retire the raw next frame");
      assert.equal(releases,0);
      raw.resolve({kind:"value",value:{kind:"item",value:1}});
      await Promise.all([reading,closing]);
      assert.equal(cancels,1);
      assert.equal(releases,1);
      assert.equal(session._activeGuards,0);
      assert.equal(session._waiters.size,0);
      await session.close();
      assert.equal(guardsAtEntry,1,"a generated raw pull uses one prepublished guard");
    }
  })(),
  (async () => {
    const raw = deferred(), cleanup = deferred(), frame = deferred();
    let reentrant, cancels = 0, releases = 0, frameEntered = 0;
    const session = makeSession({close() {}});
    const stream = session.createOutputStream({
      handle:10,next:()=>raw.promise,
      cancel() { cancels++; reentrant = stream.cancel(); return cleanup.promise; },
      release() { releases++; },
      async onClose() { frameEntered++; await frame.promise; },
    });
    const reading = assert.rejects(stream.next(),error=>error.errorName === "UniffiCancelled");
    // Let the existing raw pull enter; the assertion concerns close entry,
    // independently of lazy start scheduling.
    while (session._activeGuards === 0) await Promise.resolve();
    const closing = stream.cancel();
    const enteredDirectly = cancels;
    await assertPending(closing,"direct cancellation still waits for Rust cleanup");
    cleanup.resolve();
    await assertPending(closing,"direct cancellation still joins raw next");
    assert.equal(releases,0);
    raw.resolve({kind:"item",value:1});
    await assertPending(closing,"direct cancellation still joins onClose");
    assert.deepEqual([releases,frameEntered],[1,1]);
    frame.resolve();
    await Promise.all([reading,closing]);
    assert.equal(reentrant,closing,"close is published before synchronous backend reentry");
    assert.equal(cancels,1);
    await session.close();
    assert.equal(enteredDirectly,1,"active stream cancellation enters without resolved-start or scheduling hops");
  })(),
  (async () => {
    for (const destroyed of [false,true]) {
      const cleanup = deferred();
      let guardsAtEntry, cancels = 0, releases = 0;
      const failure = Error("raw next failed synchronously");
      const session = makeSession({
        invokeAsync(id) {
          if (id === 5) {
            guardsAtEntry = session._activeGuards;
            if (destroyed) { session.environmentDestroyed(); return new Promise(()=>{}); }
            throw failure;
          }
          assert.equal(id,6); cancels++; return cleanup.promise;
        },
        releaseOutputStream() { releases++; },close() {},
      });
      const type = {kind:"outputStream",inner:scalar("I32")};
      const descriptor = {returnType:type,streamResources:[{direction:"Output",path:"return",item:scalar("I32"),error:null,
        slots:{OutputStreamNext:5,OutputStreamCancel:6}}]};
      const stream = liftValue(21,type,session,null,descriptor);
      const reading = stream.next();
      if (!destroyed) {
        await assertPending(reading,"synchronous next failure still waits for cleanup");
        assert.equal(cancels,1); assert.equal(releases,0);
        cleanup.resolve({kind:"value"});
      }
      await assert.rejects(reading,error=>destroyed ? error.errorName === "UniffiEnvironmentDestroyed" : error.message === failure.message);
      assert.equal(guardsAtEntry,1);
      assert.equal(session._activeGuards,0); assert.equal(session._waiters.size,0);
      if (!destroyed) { assert.equal(releases,1); await session.close(); }
    }
  })(),
  (async () => {
    for (const synchronous of [true,false]) {
      const failure = new UniffiError({errorName:"UniffiCleanupFailed",message:"close failed"});
      let calls = 0, released = 0, finished = 0;
      const session = makeSession({close() {}});
      const stream = session.createOutputStream({
        handle:31,next:()=>({kind:"item",value:1}),
        cancel() {calls++; if(synchronous) throw failure; return Promise.reject(failure);},
        release() {released++;},onClose() {finished++;},
      });
      assert.deepEqual(await stream.next(),{done:false,value:1});
      const closing = stream.cancel();
      assert.equal(stream.cancel(),closing);
      await assert.rejects(closing,error=>error === failure);
      await assert.rejects(stream.cancel(),error=>error === failure);
      assert.deepEqual([calls,released,finished],[1,1,1]);
      await session.close();
    }
  })(),
]);
for (const result of outputEntryChecks) if (result.status === "rejected") console.error(result.reason);
assert.equal(outputEntryChecks.filter(result=>result.status === "rejected").length,0,"output entry checks");
const inputFrameChecks = await Promise.allSettled([
  (async () => {
    for (const rawFirst of [true, false]) for (const rejectRaw of [true, false]) for (const rejectReturn of [true, false]) {
      const raw = deferred(), returned = deferred(), onClose = deferred(), onCloseEntered = deferred();
      const hostRelease = deferred(), hostReleaseEntered = deferred();
      const failure = new Error("producer return failed");
      let closing, reentrant, returns = 0, closes = 0, releases = 0;
      const host = new Host();
      host.releaseInputStream = () => { releases++; hostReleaseEntered.resolve(); return hostRelease.promise; };
      const session = makeSession({close() {}}, host);
      const marker = session.createInputStream({
        next() { closing = marker.cancel(); return raw.promise; },
        cancel() {
          returns++;
          reentrant = marker.cancel();
          return returned.promise.then(() => { if (rejectReturn) throw failure; });
        },
      }, {onClose() { closes++; onCloseEntered.resolve(); return onClose.promise; }});
      const reading = marker.next();
      reading.catch(() => {});
      try {
        assert.equal(returns, 1, "published input close enters producer return synchronously");
        assert.equal(reentrant, closing, "producer reentry sees the already published session close");
        await assertPending(closing, "close joins producer return and raw next");
        const finishRaw = () => rejectRaw ? raw.reject(new Error("raw cancelled")) : raw.resolve({done:false,value:1});
        if (rawFirst) finishRaw(); else returned.resolve();
        await assertPending(closing, "neither raw next nor return alone acknowledges close");
        assert.equal(closes, 0);
        if (rawFirst) returned.resolve(); else finishRaw();
        await onCloseEntered.promise;
        await assertPending(closing, "onClose still owns a cleanup gate");
        assert.equal(releases, 0);
        onClose.resolve();
        await hostReleaseEntered.promise;
        await assertPending(closing, "original Host release remains awaited");
        hostRelease.resolve();
        if (rejectReturn) {
          await assert.rejects(closing, error => error === failure);
          await assert.rejects(reading, error => error === failure);
        } else {
          await closing;
          assert.deepEqual(await reading, {kind:"done"});
          await marker.release();
          assert.equal(session.inputHost.closedHandles.size, 0);
        }
        assert.deepEqual([returns, closes, releases], [1, 1, 1]);
      } finally {
        raw.resolve({done:true}); returned.resolve(); onClose.resolve(); hostRelease.resolve();
        await Promise.allSettled([reading, closing, reentrant]);
        await session.close();
      }
    }
  })(),
  (async () => {
    let reentrant, closeReentrant, returns = 0, closes = 0;
    const session = makeSession({close() {}});
    const marker = session.createInputStream({
      next: () => ({done:false,value:1}),
      cancel() { returns++; reentrant = marker.cancel(); },
    }, {onClose() { closes++; closeReentrant = marker.cancel(); }});
    const closing = marker.cancel();
    try {
      assert.equal(returns, 1, "idle input close has no empty dispatch before return");
      assert.equal(closes, 1, "synchronous producer cleanup can enter onClose directly");
      assert.equal(reentrant, closing);
      assert.equal(closeReentrant, closing);
      await closing;
      await marker.release();
      assert.deepEqual([returns,closes], [1,1]);
      assert.deepEqual([session.inputHost.slots.size,session.inputHost.closedHandles.size,
        session.inputCancelled.size,session.inputReleased.size], [0,0,0,0]);
    } finally { await closing; await session.close(); }
  })(),
  (async () => {
    for (const mode of ["next-throw", "next-reject", "lower-throw", "error-step"]) {
      const failure = new Error("bad item"), returned = deferred(), onClose = deferred();
      const returnEntered = deferred(), closeEntered = deferred();
      let returns = 0, closes = 0;
      const session = makeSession({close() {}});
      const marker = session.createInputStream({
        next() {
          if (mode === "next-throw") throw failure;
          if (mode === "next-reject") return Promise.reject(failure);
          if (mode === "error-step") return {kind:"error",error:failure};
          return {done:false,value:1};
        },
        cancel() { returns++; returnEntered.resolve(); return returned.promise; },
      }, {
        lowerItem() { throw failure; },
        lowerError(error) { assert.equal(error,failure); return "declared-item-error"; },
        onClose() { closes++; closeEntered.resolve(); return onClose.promise; },
      });
      const reading = marker.next();
      try {
        await returnEntered.promise;
        await assertPending(reading,"item error cannot precede producer cleanup");
        returned.resolve();
        await closeEntered.promise;
        await assertPending(reading,"item error cannot precede onClose");
        onClose.resolve();
        assert.deepEqual(await reading,{kind:"error",error:"declared-item-error"});
        await marker.release();
        assert.deepEqual([returns,closes],[1,1]);
      } finally { returned.resolve(); onClose.resolve(); await reading; await session.close(); }
    }
  })(),
  (async () => {
    const raw = deferred(), cleanup = deferred(), closeEntered = deferred();
    const failure = new Error("synchronous return failed");
    let closes = 0;
    const session = makeSession({close() {}});
    const marker = session.createInputStream({next: () => raw.promise, cancel() { throw failure; }}, {
      onClose() { closes++; closeEntered.resolve(); return cleanup.promise; },
    });
    const reading = marker.next(), closing = marker.cancel();
    reading.catch(() => {});
    try {
      await assertPending(closing,"synchronous return failure still joins the raw producer");
      assert.equal(closes,0);
      raw.resolve({done:false,value:1});
      await closeEntered.promise;
      await assertPending(closing,"synchronous return failure still joins onClose");
      cleanup.resolve();
      await assert.rejects(closing,error=>error===failure);
      await assert.rejects(reading,error=>error===failure);
      assert.equal(closes,1);
    } finally {
      raw.resolve({done:true}); cleanup.resolve();
      await Promise.allSettled([reading,closing]); await session.close();
    }
  })(),
  (async () => {
    const raw = deferred();
    let pulls = 0, reentrant;
    const session = makeSession({close() {}});
    const marker = session.createInputStream({
      next() { pulls++; return pulls === 1 ? raw.promise : {done:true}; },
    }, {lowerItem(value) { reentrant = marker.next(); reentrant.catch(() => {}); return value; }});
    const reading = marker.next();
    try {
      await assert.rejects(marker.next(),error=>error.errorName==="UniffiInputStreamConcurrentNext");
      raw.resolve({done:false,value:7});
      assert.deepEqual(await reading,{kind:"item",value:7});
      await assert.rejects(reentrant,error=>error.errorName==="UniffiInputStreamConcurrentNext");
      assert.equal(pulls,1,"reservation includes conversion after raw completion");
      assert.deepEqual(await marker.next(),{kind:"done"});
      await marker.release();
      assert.equal(pulls,2);
    } finally { raw.resolve({done:true}); await reading; await session.close(); }
  })(),
]);
for (const result of inputFrameChecks) if (result.status === "rejected") console.error(result.reason);
assert.equal(inputFrameChecks.filter(result=>result.status === "rejected").length,0,"input frame checks");
// S08: an ordinary scalar facade may finish on the managed call's published
// guard. The final value/frame and the raw terminal error keep their own roles.
const scalarOperation = {
  name:"scalarCompletion",id:0,args:[{name:"value",type:scalar("I32")}],
  returnType:scalar("U64"),async:true,
};
for (const [returnType,raw,wanted] of [
  [scalar("U64"),"18446744073709551615",18446744073709551615n],
  [scalar("Bool"),true,true],
  [null,null,undefined],
]) {
  let published,ended=0,received;
  const session=makeSession({invokeAsync(_id,args) {
    published=[...session.calls.values()][0].promise;
    received=args;
    assert.equal(session._activeGuards,1,"guard is published before backend entry");
    return {kind:"value",value:raw};
  },close() {}});
  const originalEnd=session.endCallFrame.bind(session);
  session.endCallFrame=(frame)=>{ended++;return originalEnd(frame);};
  const pending=invokeOperation(session,{...scalarOperation,returnType},[7]);
  assert.equal(pending,published,"scalar facade returns the same final published guard");
  assert.equal(session.frameCleanups.has(pending),true,"final guard remains tracked");
  assert.equal(await pending,wanted);
  assert.deepEqual(received,[7,1]);
  assert.equal(ended,1);
  assert.equal(session.frameCleanups.size,0);
  assert.equal(session.calls.size,0);
  // Direct BackendSession users still receive the unconverted backend value.
  assert.equal(await session.invokeAsync(0,[7]),raw);
  await session.close();
}
{
  const reply=deferred(),frameCleanup=deferred();let ended=0,closed=0;
  const session=makeSession({invokeAsync:()=>reply.promise,close(){closed++;}});
  const originalEnd=session.endCallFrame.bind(session);
  session.endCallFrame=(frame)=>{ended++;originalEnd(frame);return frameCleanup.promise;};
  const pending=invokeOperation(session,scalarOperation,[7]);
  const closing=session.close();
  // Model a Rust result committed before close's cancellation request.
  reply.resolve({kind:"value",value:"42"});
  await assertPending(pending,"conversion joins the actual frame cleanup");
  await assertPending(closing,"session close joins the final scalar guard");
  assert.equal(ended,1);assert.equal(closed,0);
  frameCleanup.resolve();assert.equal(await pending,42n);await closing;
  assert.equal(closed,1);assert.equal(session.frameCleanups.size,0);
}
for (const mode of ["lift-error","backend-error","release-error"]) {
  const reply=deferred();let ended=0;
  const {session,backend}=controlled(()=>reply.promise);
  const originalControl=backend.invokeSync;
  backend.invokeSync=(id,args)=>{
    if(mode==="release-error"&&id===10) throw new UniffiError({errorName:"ReleaseProbe",message:"release failed"});
    return originalControl(id,args);
  };
  const originalEnd=session.endCallFrame.bind(session);
  session.endCallFrame=(frame)=>{ended++;return originalEnd(frame);};
  const pending=invokeOperation(session,{...scalarOperation,returnType:scalar("U8")},[7]);
  const closing=session.close();
  reply.resolve(mode==="backend-error"
    ? {kind:"error",error:{domain:"internal",errorName:"BackendProbe",message:"backend failed"}}
    : {kind:"value",value:mode==="lift-error"?256:1});
  const errorName={"lift-error":"UniffiNumericError","backend-error":"BackendProbe","release-error":"ReleaseProbe"}[mode];
  await assert.rejects(pending,e=>e.errorName===errorName);
  if(mode==="lift-error") await closing;
  else await assert.rejects(closing,e=>e.errorName===errorName);
  assert.equal(ended,1,"even a control release failure ends the call frame");
  assert.equal(session.calls.size,0);assert.equal(session.frameCleanups.size,0);
}
for (const mode of ["pre-abort","argument","admission"]) {
  let entries=0,ended=0;
  const session=makeSession({invokeAsync(){entries++;return {kind:"value",value:1};},close(){}});
  const originalEnd=session.endCallFrame.bind(session);
  session.endCallFrame=(frame)=>{ended++;return originalEnd(frame);};
  const controller=new AbortController();
  if(mode==="pre-abort") controller.abort();
  if(mode==="admission") session.backend.invokeSync=()=>({kind:"value",value:-1});
  const pending=invokeOperation(session,scalarOperation,[mode==="argument"?null:7,{signal:controller.signal}]);
  const errorName={"pre-abort":"UniffiCancelled","argument":"UniffiNumericError","admission":"UniffiCallControl"}[mode];
  await assert.rejects(pending,e=>e.errorName===errorName);
  assert.equal(entries,0);assert.equal(ended,1);
  assert.equal(session.frameCleanups.size,0);assert.equal(session.calls.size,0);
  await session.close();
}
{
  const registration=deferred(),cleanup=deferred();let ended=0;
  const {session,events,calls}=controlled(async(_id,_args,entry)=>{
    await cleanup.promise;assert.equal(entry.cancelled,true);return cancelledEnvelope();
  },[0],registration.promise);
  const originalEnd=session.endCallFrame.bind(session);
  session.endCallFrame=(frame)=>{ended++;return originalEnd(frame);};
  const controller=new AbortController();
  const pending=invokeOperation(session,scalarOperation,[7,{signal:controller.signal}]);
  controller.abort();const closing=session.close();registration.resolve();
  await assertPending(pending,"pre-registration cancellation joins Rust cleanup");
  await assertPending(closing,"close cannot bypass the cancelled scalar cleanup");
  assert.equal(ended,0);cleanup.resolve();
  await assert.rejects(pending,e=>e.errorName==="UniffiCancelled");await closing;
  assert.equal(ended,1);assert.equal(calls.size,0);
  assert.equal(events.filter(e=>e[0]==="cancel").length,1);
  assert.equal(events.filter(e=>e[0]==="release").length,1);
}
for (const mode of ["value","throw","destroy"]) {
  let published,closing,ended=0;
  const session=makeSession({invokeAsync() {
    published=[...session.calls.values()][0].promise;
    closing=session.close();
    if(mode==="throw") throw new UniffiError({errorName:"SyncProbe",message:"sync failure"});
    if(mode==="destroy") session.environmentDestroyed();
    return {kind:"value",value:"42"};
  },close(){}});
  const originalEnd=session.endCallFrame.bind(session);
  session.endCallFrame=(frame)=>{ended++;return originalEnd(frame);};
  const pending=invokeOperation(session,scalarOperation,[7]);
  assert.equal(pending,published,"synchronous reentry shares the published final guard");
  if(mode==="value") {assert.equal(await pending,42n);await closing;}
  else {
    const errorName=mode==="throw"?"SyncProbe":"UniffiEnvironmentDestroyed";
    await assert.rejects(pending,e=>e.errorName===errorName);
    await assert.rejects(closing,e=>e.errorName===errorName);
  }
  assert.equal(ended,1);assert.equal(session.calls.size,0);assert.equal(session.frameCleanups.size,0);
}
{
  let published,received;
  const session=makeSession({invokeAsync(_id,args){published=[...session.calls.values()][0].promise;received=args;return {kind:"value",value:"42"};},close(){}});
  const descriptor={...scalarOperation,receiver:true,ownerSurface:"object"};
  const carrier={handle:51};
  const pending=invokeOperation(session,descriptor,[carrier,7]);
  assert.equal(pending,published);assert.equal(await pending,42n);
  assert.deepEqual(received,[carrier,7,1],"the engine retains its original receiver carrier");
  let customLifts=0;
  const custom={kind:"named",name:"Tagged",typeId:701};
  const customDescriptor={...scalarOperation,returnType:custom,context:{types:{701:{kind:"custom",builtin:scalar("U64"),intoCustom:value=>{customLifts++;return {value};}}}}};
  const customResult=invokeOperation(session,customDescriptor,[7]);
  assert.notEqual(customResult,published,"custom conversion keeps its general facade path");
  assert.deepEqual(await customResult,{value:42n});assert.equal(customLifts,1);
  await session.close();
}

// A hard destruction settles the published call once. A later raw value or
// rejection must not revive a detached session or run its cleanup hooks again.
for (const managed of [true, false]) {
  for (const lateFailure of [false, true]) {
    const reply = deferred();
    const {session, events, calls} = controlled(() => reply.promise, managed ? [0] : []);
    const pending = session.invokeAsync(0, []);
    const closing = session.close();
    session.environmentDestroyed();
    await assert.rejects(pending, error => error.errorName === "UniffiEnvironmentDestroyed");
    await assert.rejects(closing, error => error.errorName === "UniffiEnvironmentDestroyed");
    const controls = events.filter(event => ["cancel", "release", "engine-close"].includes(event[0]));
    if (lateFailure) reply.reject(new UniffiError({errorName: "LateFailure", message: "late raw error"}));
    else reply.resolve({kind: "value", value: 42});
    await turn();
    assert.deepEqual(events.filter(event => ["cancel", "release", "engine-close"].includes(event[0])), controls);
    assert.equal(session.calls.size, 0);
    assert.equal(session._waiters.size, 0);
    assert.equal(session._activeGuards, 0);
    assert.equal(calls.size, 0);
  }
}
// Release can synchronously initiate close while settlement has already
// removed the detach waiter. Teardown must retain the call and its raw error
// until the same scalar promise finishes its actual frame cleanup.
for (const releaseFailure of [false, true]) {
  const reply = deferred(), frameCleanup = deferred();
  const controller = new AbortController();
  let closing, releases = 0, ended = 0;
  const {session, backend, events} = controlled(() => reply.promise);
  const originalControl = backend.invokeSync;
  backend.invokeSync = (id, args) => {
    if (id === 10) {
      releases++;
      closing = session.close();
      if (releaseFailure) throw new UniffiError({errorName: "ReleaseReentry", message: "release failed after close entry"});
    }
    return originalControl(id, args);
  };
  const originalEnd = session.endCallFrame.bind(session);
  session.endCallFrame = frame => {
    ended++;
    originalEnd(frame);
    return frameCleanup.promise;
  };
  const pending = invokeOperation(session, scalarOperation, [7, {signal: controller.signal}]);
  controller.abort();
  reply.resolve({kind: "value", value: "42"});
  await turn();
  assert.ok(closing, "release entered teardown synchronously");
  assert.equal(session.calls.size, 0, "the call was removed before its frame acknowledgement");
  assert.equal(ended, 1);
  assert.equal(events.some(event => event[0] === "engine-close"), false);
  await assertPending(pending, "release settlement keeps frame cleanup awaited");
  await assertPending(closing, "teardown retains the settled call's pending frame and error");
  frameCleanup.resolve();
  if (releaseFailure) {
    await assert.rejects(pending, error => error.errorName === "ReleaseReentry");
    await assert.rejects(closing, error => error.errorName === "ReleaseReentry");
  } else {
    assert.equal(await pending, 42n);
    await closing;
  }
  assert.equal(releases, 1);
  assert.equal(ended, 1);
  assert.equal(events.filter(event => event[0] === "engine-close").length, 1);
  assert.equal(session.frameCleanups.size, 0);
  assert.equal(session._waiters.size, 0);
  assert.equal(session._activeGuards, 0);
}

// Empty frames still leave the active stack, and a failed registration must
// not leave a placeholder to be cancelled alongside the next valid input.
{
  const {session} = controlled(() => 0);
  const outer = session.beginCallFrame();
  const empty = session.beginCallFrame();
  assert.notEqual(outer, empty);
  assert.equal(session.endCallFrame(empty), undefined);
  assert.equal(session.endCallFrame(empty), undefined);
  const id = session.registerCallback(90, () => 7, {retention: "scoped", methods: {0: {name: null}}});
  assert.equal(session.invokeCallbackSync(90, id, 0, []), 7);
  const registrationError = new Error("input registration failed");
  assert.throws(() => session.createInputStream({get next() {throw registrationError;}}), error => error === registrationError);
  let returned = 0, closed = 0;
  const marker = session.createInputStream({next: () => ({done: false, value: 1}), cancel: () => {returned++;}}, {onClose: () => {closed++;}});
  const cancelled = [];
  const originalCancel = session.cancelInputStream.bind(session);
  session.cancelInputStream = handle => {assert.equal(handle, marker.handle); cancelled.push(handle); return originalCancel(handle);};
  await session.endCallFrame(outer);
  assert.equal(returned, 1);
  assert.equal(closed, 1);
  assert.ok(cancelled.length > 0);
  assert.equal(session.callbacks.callbacks.size, 0);
  assert.equal(session.inputStreams.size, 0);
  await session.close();
}
// The nonempty input snapshot must survive synchronous onClose splices. A
// producer can also close its sibling and reenter end before cleanup is stored.
for (const crossCancel of [false, true]) {
  const {session} = controlled(() => 0);
  const frame = session.beginCallFrame(), cleanup = deferred();
  const returned = [0, 0], closed = [0, 0], released = [];
  let second, reentrant;
  session.createInputStream({next: () => ({done: false, value: 1}), cancel: () => {returned[0]++;}}, {onClose: () => {
    closed[0]++;
    if (crossCancel) second.cancel();
    reentrant = session.endCallFrame(frame);
  }});
  second = session.createInputStream({next: () => ({done: false, value: 2}), cancel: () => {returned[1]++;}}, {onClose: () => {
    closed[1]++;
    return cleanup.promise;
  }});
  const originalRelease = session.host.releaseInputStream.bind(session.host);
  session.host.releaseInputStream = handle => {released.push(handle); return originalRelease(handle);};
  const closing = session.endCallFrame(frame);
  assert.deepEqual(returned, [1, 1]);
  assert.deepEqual(closed, [1, 1]);
  assert.equal(reentrant, undefined, "collecting end retains its existing reentry timing");
  assert.equal(session.endCallFrame(frame), closing);
  await assertPending(closing, "both synchronous splices still retain the sibling cleanup gate");
  cleanup.resolve();
  await closing;
  assert.equal(session.endCallFrame(frame), closing);
  assert.equal(released.length, 2);
  assert.equal(new Set(released).size, 2);
  assert.deepEqual(returned, [1, 1]);
  assert.deepEqual(closed, [1, 1]);
  await session.close();
}
{
  const {session} = controlled(() => 0);
  const frame = session.beginCallFrame(), siblingCleanup = deferred();
  const failure = new UniffiError({errorName: "FrameCleanup", message: "producer cleanup failed"});
  const returned = [0, 0], closed = [0, 0], releaseCalls = [];
  session.createInputStream({next: () => ({done: false, value: 1}), cancel: () => {returned[0]++; throw failure;}}, {onClose: () => {closed[0]++;}});
  session.createInputStream({next: () => ({done: false, value: 2}), cancel: () => {returned[1]++;}}, {onClose: () => {closed[1]++; return siblingCleanup.promise;}});
  const originalRelease = session.releaseInputStream.bind(session);
  session.releaseInputStream = handle => {releaseCalls.push(handle); return originalRelease(handle);};
  const closing = session.endCallFrame(frame);
  await assertPending(closing, "failed cleanup cannot skip its sibling");
  siblingCleanup.resolve();
  await assert.rejects(closing, error => error === failure);
  assert.equal(session.endCallFrame(frame), closing);
  await assert.rejects(session.endCallFrame(frame), error => error === failure);
  assert.deepEqual(returned, [1, 1]);
  assert.deepEqual(closed, [1, 1]);
  assert.equal(releaseCalls.length, 2);
  assert.equal(new Set(releaseCalls).size, 2);
  await assert.rejects(session.close(), error => error === failure);
}

// M05: declared errors used as ordinary DTO values match their generated classes.
{
  class ValueError extends UniffiError {
    constructor(message = "", variant = null, data = null) { super({errorName: "ValueError", message, variant, data}); }
  }
  class PayloadError extends UniffiError {
    constructor(message = "", variant = null, data = null) { super({errorName: "PayloadError", message, variant, data}); }
  }
  const named = (name, typeId) => ({kind: "named", name, typeId});
  const error = named("ValueError", 1), payload = named("PayloadError", 2);
  const optional = {kind: "optional", inner: error};
  const context = {types: {
    1: {kind: "enum", error: true, unit: true, variants: {Io: {fields: {}}, Worker: {fields: {}}}},
    2: {kind: "enum", error: true, unit: false, variants: {
      Empty: {fields: {}}, Failed: {fields: {code: scalar("U64"), cause: optional}},
    }},
    3: {kind: "record", fields: {error: optional, history: {kind: "sequence", inner: error}}},
    4: {kind: "enum", unit: false, variants: {Failed: {fields: {error}}, Closed: {fields: {error: optional}}}},
    5: {kind: "enum", unit: true, variants: {Io: {fields: {}}}},
  }, errorClasses: {1: ValueError, 2: PayloadError}};
  const failure = new ValueError("host-only message", "Io");
  const report = named("Report", 4), state = named("State", 3);
  assert.equal(lowerValue(failure, error, context), "Io");
  assert.deepEqual(lowerValue({tag: "Failed", error: failure}, report, context), {tag: "Failed", error: "Io"});
  assert.deepEqual(lowerValue({tag: "Closed", error: null}, report, context), {tag: "Closed", error: null});
  // The generated N-API and WASM hosts use different lift carriers for unit
  // errors. Both still lower to the native variant string.
  for (const raw of [{error: "Io", history: ["Worker"]}, {error: {tag: "Io"}, history: [{tag: "Worker"}]}]) {
    const lifted = liftValue(raw, state, null, context);
    assert.ok(lifted.error instanceof ValueError);
    assert.equal(lifted.error.variant, "Io");
    assert.equal(lifted.error.data, "Io");
    assert.equal(lifted.error.message, "Io", "ordinary carriers do not transport exception messages");
    assert.equal(lifted.error.descriptor, context.types[1]);
    assert.ok(lifted.history[0] instanceof ValueError);
    assert.deepEqual(lowerValue(lifted, state, context), {error: "Io", history: ["Worker"]});
  }
  assert.equal(liftValue(null, optional, null, context), null);
  const carrier = {tag: "Failed", code: 7n, cause: "Io"};
  assert.deepEqual(lowerValue(new PayloadError("ignored", "Failed", {code: "7", cause: failure}), payload, context), carrier);
  const payloadValue = liftValue(carrier, payload, null, context);
  assert.ok(payloadValue instanceof PayloadError);
  assert.equal(payloadValue.variant, "Failed");
  assert.equal(payloadValue.data.code, 7n);
  assert.ok(payloadValue.data.cause instanceof ValueError);
  assert.deepEqual(lowerValue(payloadValue, payload, context), carrier);
  const wasmPayloadValue = liftValue({...carrier, cause: {tag: "Io"}}, payload, null, context);
  assert.ok(wasmPayloadValue.data.cause instanceof ValueError);
  assert.deepEqual(lowerValue(wasmPayloadValue, payload, context), carrier);
  assert.deepEqual(lowerValue(new PayloadError("", "Empty"), payload, context), {tag: "Empty"});
  assert.equal(liftValue({tag: "Empty"}, payload, null, context).data, "Empty");
  for (const invalid of ["Io", {tag: "Io"}, new Error("Io"), new PayloadError("", "Io")]) {
    expectError(() => lowerValue(invalid, error, context), "UniffiErrorType");
  }
  expectError(() => lowerValue(new ValueError("", "Unknown"), error, context), "UniffiEnumVariant");
  expectError(() => lowerValue(new PayloadError("", "Failed", {code: 7n}), payload, context), "UniffiErrorType");
  expectError(() => lowerValue(new PayloadError("", "Failed", {code: -1n, cause: null}), payload, context), "UniffiNumericError");
  expectError(() => liftValue("Unknown", error, null, context), "UniffiEnumVariant");
  for (const invalid of [{tag: "Unknown"}, {tag: "Io", extra: 1}, {tag: 1}, ["Io"], Object.create({tag: "Io"})]) {
    expectError(() => liftValue(invalid, error, null, context), "UniffiEnumVariant");
  }
  expectError(() => liftValue({tag: "Failed", code: 7n}, payload, null, context), "UniffiEnumVariant");
  expectError(() => liftValue("Io", error, null, {types: context.types}), "UniffiErrorProtocol");
  const ordinary = named("Ordinary", 5);
  assert.equal(lowerValue("Io", ordinary, context), "Io");
  assert.equal(liftValue("Io", ordinary, null, context), "Io");
  expectError(() => lowerValue(failure, ordinary, context), "UniffiEnumType");
  expectError(() => liftValue({tag: "Io"}, ordinary, null, context), "UniffiEnumVariant");

  // Preserve the exception envelope and callback rejection paths.
  const session = makeSession({invokeSync() {
    return {kind: "error", error: {variant: "Io", data: null, message: "operation failed"}};
  }, close() {}});
  const throws = {name: "ValueError", typeId: 1};
  assert.throws(() => invokeOperation(session, {name: "fail", id: 0, args: [], returnType: null, async: false, throws, context}),
    value => value instanceof ValueError && value.variant === "Io" && value.message === "operation failed");
  const callback = session.registerCallback(6, () => { throw new PayloadError("callback failed", "Failed", {code: "7", cause: failure}); }, {
    methods: {0: {name: null, throws: {name: "PayloadError", typeId: 2}}}, context,
  });
  assert.throws(() => session.invokeCallbackSync(6, callback, 0, []), value => value instanceof PayloadError
    && value.message === "callback failed" && value.data.code === 7n && value.data.cause === "Io");
  await session.close();

  // Stream rejection adapters already own a native carrier; do not treat it
  // as a public field value or lower its payload twice.
  for (const [type, rejection, expected] of [[error, failure, "Io"], [payload, payloadValue, carrier]]) {
    let lowerError;
    lowerValue({}, {kind: "inputStream", inner: scalar("I32")}, context, {
      createInputStream(_source, conversions) { lowerError = conversions.lowerError; return {handle: 9}; },
    }, {streamResources: [{direction: "Input", path: "argument[0]", error: type}]}, "argument[0]");
    assert.deepEqual(lowerError(rejection), expected);

    const outputSession = makeSession({
      invokeSync: () => ({kind: "value", value: 9}),
      invokeAsync: async id => ({kind: "value", value: id === 8 ? {kind: "error", error: expected} : null}),
      close() {},
    });
    const output = invokeOperation(outputSession, {name: "errors", id: 0, args: [], async: false, context,
      returnType: {kind: "outputStream", inner: scalar("I32")},
      streamResources: [{direction: "Output", path: "return", error: type,
        slots: {OutputStreamNext: 8, OutputStreamCancel: 9}}],
    });
    await assert.rejects(output.next(), value => value instanceof context.errorClasses[type.typeId]
      && value.variant === rejection.variant);
    await output.cancel();
    await outputSession.close();
  }
}

console.log("S01 runtime protocol: 22 scenarios passed; legacy callback/conversion/stream assertions passed; S08 immediate-entry, input lifecycle, raw-next reentry, single-guard, lazy input-frame and scalar completion checks passed; shared-state late delivery and release reentry 6 checks passed; call-frame collection lifecycle 4 checks passed; M05 ordinary error field conversions and exception boundaries passed");
