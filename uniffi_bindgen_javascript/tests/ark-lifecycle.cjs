const assert = require('node:assert/strict');
const platform = require('./out/platform.js');
const business = require('./out/Index.js');
const fixture = require('./fixture.json');
const ids = fixture.operations;
const streams = fixture.streams;
const tick = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => { let resolve, reject; const promise = new Promise((done, fail) => { resolve = done; reject = fail; }); return {promise, resolve, reject}; };
for (const name of ['BackendSession', 'Host', 'ObjectLease', 'CallbackRegistry', 'createNamespace', 'CallController']) {
  assert.equal(name in business, false, name);
}
let next = 1, closed = 0, gate = deferred(), fail = false;
const releases = [], closingHandles = [], work = new Map();
const streamReleases = [], streamCancels = [], streamFactory = deferred();
const backend = {
  invokeSync(id) {
    if (id === fixture.callControl.create) return new platform.ArkValueResult(next++);
    if (id === fixture.callControl.cancel || id === fixture.callControl.release) return new platform.ArkValueResult(null);
    if (id === streams.events.start) return new platform.ArkValueResult(next++);
    if ([ids['Client.service'], ids['Client.conversation'], ids['Client.view']].includes(id)) return new platform.ArkValueResult(next++);
    if (id === ids['Service.name'] || id === ids['View.snapshot']) return new platform.ArkValueResult('ok');
    throw Error(`unexpected sync ${id}`);
  },
  async invokeAsync(id, args) {
    if (id === fixture.callControl.create + 3) return new platform.ArkValueResult(1);
    if (id === streams.lateEvents.start) { await streamFactory.promise; return new platform.ArkValueResult(9001); }
    if (id === streams.events.next || id === streams.lateEvents.next) return new platform.ArkValueResult(new platform.ArkItemStep(7));
    if (id === streams.events.cancel || id === streams.lateEvents.cancel) { streamCancels.push(args[0]); return new platform.ArkValueResult(null); }
    if (id === fixture.construct) return new platform.ArkValueResult(next++);
    if (id === ids['Service.work']) {
      const pending = deferred(); work.set(args[0], pending);
      await pending.promise; return new platform.ArkValueResult('finished');
    }
    if (id === ids['Client.close'] || id === ids['View.close']) {
      closingHandles.push(args[0]); await gate.promise;
      if (fail) throw new platform.UniffiError('CleanupProbe', 'cleanup failed');
      return new platform.ArkValueResult(null);
    }
    throw Error(`unexpected async ${id}`);
  },
  releaseObject(handle) { releases.push(handle); },
  releaseOutputStream(handle) { streamReleases.push(handle); },
  close() { closed++; },
};
const session = new platform.BackendSession(backend, undefined, {callControl:fixture.callControl});
(async () => {
  const api = platform.createNamespace(session).lifecycle;
  const client = await api.Client[fixture.constructor](), other = await api.Client[fixture.constructor]();
  assert.equal('dispose' in client, false);
  assert.equal('releaseReference' in client, false);
  assert.equal('objectSession' in client, false);
  assert.throws(() => platform.releaseReference(client), {errorName:'UniffiResourceCloseRequired'});
  const service = client.service();
  for (let i = 0; i < 1000; i++) assert.equal(client.service(), service);
  assert.equal(service.name(), 'ok');
  let busy = client.conversation('busy');
  const busyHandle = next - 1;
  const pending = busy.work(); busy = null;
  for (let i = 0; i < 400; i++) client.conversation(String(i));
  for (let i = 0; i < 100 && session.objects.size > 4; i++) { await tick(); global.gc(); await tick(); }
  assert.equal(session.objects.size, 4, 'long-lived owner and session must not root temporary wrappers');
  assert.equal(releases.length, 400);
  const detached = other.conversation('fixture');
  platform.releaseReference(detached); platform.releaseReference(detached);
  assert.throws(() => detached.name(), {errorName:'UniffiUseAfterDispose'});
  const view = client.view(), sibling = client.view();
  const viewHandle = next - 2, siblingHandle = next - 1;
  const stopping = view.close(); gate.resolve(); await stopping;
  assert.ok(releases.includes(viewHandle));
  assert.ok(!releases.includes(siblingHandle));
  assert.equal(sibling.snapshot(), 'ok');
  const events = client.events(), siblingEvents = other.events();
  assert.equal((await events.next()).kind, 'item'); const eventsHandle = next - 1;
  assert.equal((await siblingEvents.next()).kind, 'item'); const siblingEventsHandle = next - 1;
  const late = assert.rejects(client.lateEvents(), {errorName:'UniffiObjectClosed'});
  gate = deferred();
  const controller = new platform.CallController(); controller.abort();
  const closing = client.close({signal:controller.signal});
  assert.equal(closing, client.close());
  assert.equal(closing, sibling.close());
  assert.throws(() => service.name(), {errorName:'UniffiObjectClosed'});
  gate.resolve(); await tick();
  assert.ok(!releases.includes(busyHandle), 'in-flight service reference remains pinned');
  streamFactory.resolve(); await late;
  work.get(busyHandle).resolve(); assert.equal(await pending, 'finished'); await closing;
  assert.equal(sibling.close(), closing, 'close after owner release is still idempotent');
  assert.throws(() => sibling.snapshot(), {errorName:'UniffiObjectClosed'});
  assert.ok(streamReleases.includes(eventsHandle));
  assert.ok(streamCancels.includes(9001)); assert.ok(streamReleases.includes(9001));
  assert.ok(!streamCancels.includes(siblingEventsHandle)); await siblingEvents.cancel();
  assert.equal(other.service().name(), 'ok'); assert.equal(closed, 0);
  const failedView = other.view();
  fail = true;
  const failure = other.close();
  await assert.rejects(failure, {errorName:'CleanupProbe'});
  assert.equal(failedView.close(), failure);
  await assert.rejects(failedView.close(), {errorName:'CleanupProbe'});
  assert.equal(session.objects.size, 0);
  assert.equal(new Set(releases).size, releases.length, 'all references release exactly once');
  await session.close(); assert.equal(closed, 1);
  for (const scenario of [
    {name:'owner-pending'},
    {name:'owner-starting',stage:'starting'},
    {name:'owner-unpulled',stage:'idle'},
    {name:'owner-joins-pull',rawLast:true},
    {name:'explicit-cancel',explicit:true},
    {name:'explicit-cancel-before-owner',explicit:true,thenOwner:true},
    {name:'owner-before-explicit-cancel',thenCancel:true},
    {name:'owner-cleanup-failure',cleanupError:'CleanupProbe'},
    {name:'owner-transport-failure',pullError:'TransportProbe'},
    {name:'owner-native-interruption',pullError:'UniffiCancelled',interruption:true},
    {name:'owner-interruption-cleanup-failure',pullError:'UniffiCancelled',interruption:true,cleanupError:'CleanupProbe'},
    {name:'owner-protocol-failure',pullError:'UniffiStreamProtocol'},
    {name:'owner-error-step',stepError:'StreamProbe'},
    {name:'owner-cancelled-error-step',stepError:'UniffiCancelled'},
  ]) {
    let handle=1, siblingHandle;
    const raw=deferred(), cleanup=deferred(), native=deferred(), ack=deferred();
    const pulls=new Map(), started=[], cancelled=[], released=[], objects=[];
    const probe=new platform.BackendSession({
      invokeSync(id) {
        if(id===fixture.callControl.create) return new platform.ArkValueResult(handle++);
        if(id===fixture.callControl.cancel || id===fixture.callControl.release) return new platform.ArkValueResult(null);
        assert.equal(id,streams.events.start); const value=handle++; started.push(value); return new platform.ArkValueResult(value);
      },
      async invokeAsync(id,args) {
        if(id===fixture.construct) return new platform.ArkValueResult(handle++);
        if(id===ids['Client.close']) { await native.promise; return new platform.ArkValueResult(null); }
        if(id===streams.events.next) {
          const count=(pulls.get(args[0])??0)+1; pulls.set(args[0],count);
          return new platform.ArkValueResult(count===1 ? new platform.ArkItemStep(7) : await raw.promise);
        }
        if(id===streams.events.cancel) { cancelled.push(args[0]); if(args[0]!==siblingHandle) await cleanup.promise; return new platform.ArkValueResult(null); }
        if(id===fixture.callControl.create+3) { if(args[0]!==siblingHandle) await ack.promise; return new platform.ArkValueResult(1); }
        throw Error(`unexpected owner stream operation ${id}`);
      },
      releaseObject(value) { objects.push(value); },
      releaseOutputStream(value) { released.push(value); },
      close() {},
    },undefined,{callControl:fixture.callControl});
    const ns=platform.createNamespace(probe).lifecycle;
    const owner=await ns.Client[fixture.constructor](), other=await ns.Client[fixture.constructor]();
    const siblingStream=other.events(); await siblingStream.next(); siblingHandle=started.at(-1);
    const stream=owner.events();
    if(!scenario.stage) await stream.next();
    let reading=scenario.stage==='idle' ? null : stream.next().then(value=>({value}),error=>({error}));
    // Active Ark next enters through a microtask; let the raw pull actually
    // enter before testing its join. Starting/unpulled remain separate cases.
    if(!scenario.stage) await tick();
    let stopping=scenario.explicit ? stream.cancel() : owner.close();
    if(scenario.thenOwner) stopping=Promise.all([stopping,owner.close()]);
    let settled=false; stopping.then(()=>{settled=true;},()=>{settled=true;});
    if(!reading) reading=stream.next().then(value=>({value}),error=>({error}));
    await tick(); assert.equal(settled,false,scenario.name);
    if(scenario.thenCancel) assert.equal(stream.cancel(),stream.cancel(),'owner close keeps its first terminal request');
    if(scenario.rawLast) {
      cleanup.resolve(); native.resolve(); ack.resolve(); await tick();
      assert.equal(settled,false,'owner close joins the raw pull'); assert.equal(released.length,0);
    }
    if(scenario.pullError) raw.reject(new platform.UniffiError(scenario.pullError,'pull failed'));
    else raw.resolve(scenario.stepError ? new platform.ArkErrorStep(new platform.UniffiError(scenario.stepError,'stream failed')) : scenario.rawLast ? new platform.ArkItemStep(99) : new platform.ArkDoneStep());
    native.resolve(); await tick();
    if(!scenario.stage && !scenario.rawLast) assert.equal(settled,false,'raw completion is not cleanup ACK');
    if(scenario.cleanupError) cleanup.reject(new platform.UniffiError(scenario.cleanupError,'cleanup failed'));
    else cleanup.resolve();
    await tick();
    if(!scenario.stage && !scenario.rawLast) {
      assert.equal(settled,false,'owner close awaits the host cleanup receipt'); assert.equal(released.length,0);
    }
    ack.resolve();
    if(scenario.cleanupError) await assert.rejects(stopping,{errorName:scenario.cleanupError});
    else await stopping;
    const outcome=await reading;
    const errorName=scenario.cleanupError || (!scenario.interruption && scenario.pullError) || scenario.stepError || (scenario.explicit && 'UniffiCancelled');
    if(errorName) assert.equal((outcome.error || outcome.value?.error)?.errorName,errorName,scenario.name);
    else assert.equal(outcome.value?.kind,'done',scenario.name);
    if(scenario.cleanupError) await assert.rejects(stream.next(),{errorName:scenario.cleanupError});
    else for(let i=0;i<2;i++) assert.equal((await stream.next()).kind,'done');
    assert.equal(cancelled.includes(siblingHandle),false); assert.equal(released.includes(siblingHandle),false);
    assert.deepEqual(released,started.filter(value=>value!==siblingHandle));
    pulls.set(siblingHandle,0); assert.equal((await siblingStream.next()).kind,'item');
    await siblingStream.cancel();
    if(scenario.explicit) await owner.close();
    await other.close();
    assert.equal(probe.objects.size,0); assert.equal(probe.streams.length,0);
    assert.equal(new Set(objects).size,objects.length); assert.equal(new Set(released).size,released.length);
    await probe.close();
  }
  {
    const cleanup=deferred(),cancelled=new Set();
    let block=1,settled=false,closed=false;
    const probe=new platform.BackendSession({
      invokeSync(id,args) {
        if(id===fixture.callControl.create){const value=block;block+=4096;return new platform.ArkValueResult(value);}
        if(id===fixture.callControl.cancel)cancelled.add(args[0]);
        return new platform.ArkValueResult(null);
      },
      async invokeAsync(id,args) {
        if(id===fixture.construct)return new platform.ArkValueResult(1);
        if(id===ids['Client.close'])return new platform.ArkValueResult(null);
        assert.equal(id,streams.lateEvents.start);
        await cleanup.promise;assert.ok(cancelled.has(args.at(-1)));
        return new platform.ArkErrorResult({errorName:'UniffiCancelled',message:'factory cancelled',variant:null,data:null});
      },
      releaseObject() {},releaseOutputStream() {},close() {},
    },undefined,{callControl:fixture.callControl});
    const owner=await platform.createNamespace(probe).lifecycle.Client[fixture.constructor]();
    const controller=new platform.CallController();
    const factory=assert.rejects(owner.lateEvents({signal:controller.signal}),{errorName:'UniffiCancelled'}).then(()=>{settled=true;});
    controller.abort();
    const closing=owner.close().then(()=>{closed=true;});
    await tick();assert.equal(settled,false);assert.equal(closed,false,'owner joins factory cleanup');
    cleanup.resolve();await factory;await closing;
    assert.equal(probe.streams.length,0);assert.equal(probe.objects.size,0);
    await probe.close();
  }
  console.log('generated Ark lifecycle regressions passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
