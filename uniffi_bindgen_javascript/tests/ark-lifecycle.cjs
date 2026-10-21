const assert = require('node:assert/strict');
const platform = require('./out/platform.js');
const business = require('./out/Index.js');
const fixture = require('./fixture.json');
const ids = fixture.operations;
const streams = fixture.streams;
const tick = () => new Promise(resolve => setImmediate(resolve));
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return {promise, resolve}; };
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
  console.log('generated Ark lifecycle regressions passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
