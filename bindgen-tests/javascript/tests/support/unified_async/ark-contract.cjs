const assert = require('node:assert/strict');
const gate = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
async function until(check) { const end = Date.now() + 5000; while (!check()) { if (Date.now() > end) throw Error('probe gate deadline'); await new Promise(r => setTimeout(r, 1)); } }
const observe = promise => { const state = { settled: false }; state.result = Promise.resolve(promise).then(value => ({ value }), error => ({ error })).then(result => { state.settled = true; return result; }); return state; };

exports.run = async ark => {
  const api = ark.unified_async_fixture, cases = [];
  let sequence = 10000;
  const id = () => ++sequence;
  const test = async (name, fn) => { await fn(); cases.push(name); console.log(JSON.stringify({ realm: 'ark-napi', case: name, status: 'passed' })); };
  const collect = async stream => { const result = []; stream = await stream; for (;;) { const step = await stream.next(); if (step.kind === 'done') return result; if (step.kind === 'error') throw step.error; result.push(step.value); } };
  const items = value => { let read = false; return { async next() { if (read) return new ark.ArkDoneStep(); read = true; return new ark.ArkItemStep(value); }, async cancel() {} }; };
  const cleanup = async (n, operation) => {
    await until(() => api.snapshot(n).cleaning === 1);
    assert.equal(operation.settled, false); assert.equal(api.snapshot(n).dropped, 1);
    api.openGate(n, 2); await until(() => api.snapshot(n).nested === 1);
    assert.equal(operation.settled, false); assert.equal(api.snapshot(n).cleaned, 0);
    api.openGate(n, 4); const result = await operation.result; assert.equal(api.snapshot(n).cleaned, 1); return result;
  };
  await test('future-default-infallible-results', async () => {
    assert.equal(await api.directFuture(17), 17); assert.equal(await api.directBoxFuture(18), 18);
    assert.equal(api.rustDefault(), 0); assert.equal(api.rustDefault(19), 19);
    assert.equal(api.infallibleValue(20), 20); assert.equal(await api.infallibleAsyncValue(21), 21); assert.equal(await api.infallibleFutureValue(22), 22);
    api.infallibleUnit(); await api.infallibleAsyncUnit(); await api.infallibleFutureUnit();
    for (const ctor of ['new', 'newArc', 'newFuture']) { const object = await api.ResultConstructed[ctor](23); assert.equal(object.value(), 23); object.dispose(); }
  });
  await test('abort-awaits-nested-rust-cleanup', async () => {
    const n = id(), controller = new ark.CallController();
    const operation = observe(api.guarded(n, 1, { signal: controller.signal }));
    await until(() => api.snapshot(n).started === 1); controller.abort(); controller.abort();
    assert.equal((await cleanup(n, operation)).error.errorName, 'UniffiCancelled');
    const before = id(); await assert.rejects(api.guarded(before, 1, { signal: controller.signal }), { errorName: 'UniffiCancelled' }); assert.equal(api.snapshot(before).started, 0);
  });
  await test('plain-stream-none-eof-demand', async () => {
    const n = id(), stream = api.plainEvents(n);
    assert.equal((await stream.next()).value, 7); const polls = api.snapshot(n).polls;
    await api.pendingValue(0); assert.equal(api.snapshot(n).polls, polls);
    const none = await stream.next(); assert.equal(none.kind, 'item'); assert.equal(none.value, null);
    assert.equal((await stream.next()).value, 9); assert.equal((await stream.next()).kind, 'done'); assert.equal(api.snapshot(n).dropped, 1);
    await stream.cancel();
  });
  await test('sync-async-future-factory-and-item-errors', async () => {
    for (const factory of ['syncFactory', 'asyncFactory', 'futureFactory']) {
      await assert.rejects(collect(api[factory](id(), true)), error => error instanceof ark.FixtureError && error.variant === 'Factory');
      const n = id(), stream = await api[factory](n, false);
      assert.equal((await stream.next()).value, 7); const error = await stream.next();
      assert.equal(error.kind, 'error'); assert.ok(error.error instanceof ark.FixtureError); assert.equal(error.error.variant, 'Boom');
      assert.equal((await stream.next()).kind, 'done'); assert.equal(api.snapshot(n).dropped, 1);
    }
  });
  await test('object-and-rust-trait-stream-methods', async () => {
    const service = api.StreamService.new();
    for (const method of ['values', 'valuesLater', 'valuesFuture']) assert.deepEqual(await collect(service[method](id())), [7, null, 9]);
    service.dispose(); const provider = api.RustStreamProvider.new();
    for (const [method, result] of [['syncValues', [11,12]], ['asyncValues', [13,14]], ['boxedValues', [15,16]], ['staticValues', [17]]]) assert.deepEqual(await collect(provider[method]()), result);
    provider.dispose();
  });
  await test('canonical-result-item-and-factory-shapes', async () => {
    for (const [method, values] of [['canonicalInfallible', [71,72]], ['infallibleResultItems', [73,74]], ['infallibleSyncFactory', [71,72]], ['infallibleAsyncFactory', [75,76]], ['infallibleFutureFactory', [71,72]]]) assert.deepEqual(await collect(api[method]()), values);
  });
  await test('host-trait-streams-and-infallible-reverse', async () => {
    const provider = { unit() {}, async asyncUnit() {}, async futureUnit() {}, value() { return 91; }, async asyncValue() { return 92; }, async futureValue() { return 93; }, canonical() { return items(94); }, async items() { return items(95); }, factory() { return items(96); }, async asyncFactory() { return items(97); }, async futureFactory() { return items(98); } };
    for (let kind = 0; kind < 11; kind++) assert.deepEqual(await api.hostInfallible(provider, kind), [91 + kind]);
  });
  await test('host-sync-async-boxed-static-stream-and-first-error', async () => {
    let cancelled = 0, fail = false;
    const source = value => ({ async next() { return fail ? new ark.ArkErrorStep(new ark.FixtureError('producer', 'Boom', null)) : new ark.ArkDoneStep(); }, async cancel() { cancelled++; } });
    const provider = { syncValues() { return items(11); }, async asyncValues() { return items(12); }, async boxedValues() { return fail ? source(13) : items(13); }, async staticValues() { return items(14); }, async consumeInput(input) { return (await input.next()).value; }, async staticConsume(input) { return (await input.next()).value; } };
    for (let kind = 0; kind < 4; kind++) assert.deepEqual(await api.hostValues(provider, kind), [11 + kind]);
    fail = true; await assert.rejects(api.hostValues(provider, 2), error => error instanceof ark.FixtureError && error.variant === 'Boom'); assert.equal(cancelled, 1);
    // Rejections and explicit error steps share the declared E conversion;
    // unrelated errors must stay on the framework channel after both gates.
    const service = api.StreamService.new();
    for (const style of ['step', 'reject-declared', 'reject-framework', 'reject-framework-with-declared-name', 'cancel-rejected-input']) {
      const n = id(), cancelStarted = gate(), cancelGate = gate(), releaseStarted = gate(), releaseGate = gate();
      const controller = new ark.CallController();
      let pulls = 0, cancels = 0, releases = 0, cleaned = 0;
      const error = style === 'reject-framework' ? new Error('producer framework failure')
        : style === 'reject-framework-with-declared-name' ? new ark.UniffiError('FixtureError', 'not a declared instance', 'Boom', null)
        : new ark.FixtureError('producer', 'Boom', null);
      const source = {
        async next() { pulls++; if (style === 'step') return new ark.ArkErrorStep(error); throw error; },
        async cancel() { cancels++; cancelStarted.resolve(); await cancelGate.promise; },
        async release() { releases++; releaseStarted.resolve(); await releaseGate.promise; cleaned++; },
      };
      api.openGate(n, 1);
      const operation = observe(service.consume(source, n, 1, { signal: controller.signal }));
      await cancelStarted.promise;
      assert.equal(operation.settled, false); assert.equal(pulls, 1); assert.equal(cancels, 1); assert.equal(releases, 0);
      if (style === 'cancel-rejected-input') controller.abort();
      cancelGate.resolve(); await releaseStarted.promise;
      assert.equal(operation.settled, false); assert.equal(cleaned, 0); assert.equal(releases, 1);
      releaseGate.resolve(); const result = await operation.result;
      assert.equal(cleaned, 1); assert.equal(cancels, 1); assert.equal(releases, 1);
      if (style === 'step' || style === 'reject-declared') {
        assert.ok(result.error instanceof ark.FixtureError, `${style}: preserves declared E after cleanup`);
        assert.equal(result.error.errorName, 'FixtureError'); assert.equal(result.error.variant, 'Boom');
      } else {
        assert.equal(result.error instanceof ark.FixtureError, false, `${style}: cannot fabricate E`);
        assert.equal(result.error.errorName, style === 'cancel-rejected-input' ? 'UniffiCancelled' : 'UniffiOperation');
        assert.equal(result.error.variant, null);
      }
      console.log(JSON.stringify({ realm: 'ark-napi', probe: `r1-input-error-${style}`, errorName: result.error.errorName, variant: result.error.variant, pulls, cancels, releases, cleaned, status: 'passed' }));
    }
    service.dispose();
  });
  await test('host-input-proxy-survives-await-and-joined-release', async () => {
    for (const method of ['hostConsume', 'hostStaticConsume']) {
      const entered = gate(), continueCallback = gate(), returning = gate(), release = gate(); let cancelled = 0, cleaned = 0, pulled = 0, callbackDone = false;
      const consume = async input => { entered.resolve(); await continueCallback.promise; const value = (await input.next()).value; await input.release(); callbackDone = true; return value; };
      const provider = { syncValues() { return items(1); }, async asyncValues() { return items(1); }, async boxedValues() { return items(1); }, async staticValues() { return items(1); }, consumeInput: consume, staticConsume: consume };
      const source = { async next() { pulled++; return new ark.ArkItemStep(123); }, async cancel() { cancelled++; returning.resolve(); await release.promise; cleaned++; }, async release() {} };
      const operation = observe(method === 'hostConsume' ? api.hostConsume(provider, source) : api.hostStaticConsume(provider, source, false));
      await entered.promise; assert.equal(cancelled, 0); assert.equal(pulled, 0); continueCallback.resolve();
      await returning.promise; assert.equal(operation.settled || callbackDone, false); assert.equal(pulled, 1);
      release.resolve(); assert.deepEqual(await operation.result, { value: 123 }); assert.equal(callbackDone, true); assert.equal(cleaned, 1);
    }
  });
  await test('method-input-transfer-and-joined-producer-close', async () => {
    const service = api.StreamService.new();
    for (const method of ['echo', 'echoAsync']) {
      const returned = gate(), released = gate(); let pulls = 0, cancelling = 0, releasing = 0, cleaned = 0;
      const producer = { async next() { return new ark.ArkItemStep(100 + ++pulls); }, async cancel() { cancelling++; await returned.promise; }, async release() { releasing++; await released.promise; cleaned++; } };
      const stream = await service[method](producer); assert.equal(pulls, 0); assert.equal(cancelling, 0);
      assert.equal((await stream.next()).value, 101); assert.equal((await stream.next()).value, 102);
      await api.pendingValue(0); assert.equal(pulls, 2); assert.equal(cancelling, 0);
      const a = observe(stream.cancel()), b = observe(stream.cancel()); await until(() => cancelling === 1);
      assert.equal(a.settled || b.settled, false); assert.equal(releasing, 0);
      returned.resolve(); await until(() => releasing === 1); assert.equal(a.settled || b.settled, false);
      released.resolve(); assert.deepEqual(await a.result, { value: undefined }); assert.deepEqual(await b.result, { value: undefined }); assert.equal(cleaned, 1); assert.equal(cancelling, 1); assert.equal((await stream.next()).kind, 'done');
    }
    service.dispose();
  });
  await test('failed-cancelled-factory-cleans-unpulled-input', async () => {
    const service = api.StreamService.new();
    for (const method of ['echoFail', 'echoCancel']) {
      const ack = gate(); let cancelled = 0, cleaned = 0;
      const producer = { async next() { throw Error('no demand expected'); }, async cancel() { cancelled++; await ack.promise; cleaned++; }, async release() {} };
      const operation = observe(service[method](producer)); await until(() => cancelled === 1); assert.equal(operation.settled, false); ack.resolve();
      const result = await operation.result; assert.equal(cleaned, 1); assert.equal(cancelled, 1);
      if (method === 'echoFail') { assert.ok(result.error instanceof ark.FixtureError); assert.equal(result.error.variant, 'Factory'); } else assert.equal(result.error.errorName, 'UniffiCancelled');
    }
    service.dispose();
  });
  await test('pending-next-close-shares-owner-cleanup', async () => {
    const n = id(), stream = api.cleanupEvents(n), reading = observe(stream.next());
    await until(() => api.snapshot(n).polls > 0);
    const a = observe(stream.cancel()), b = observe(stream.cancel());
    const result = await cleanup(n, a); assert.deepEqual(result, { value: undefined }); assert.deepEqual(await b.result, { value: undefined });
    const terminal = await reading.result; assert.equal(terminal.value.kind, 'error'); assert.equal(terminal.value.error.errorName, 'UniffiCancelled'); assert.equal((await stream.next()).kind, 'done');
  });
  await test('local-owner-method-cleanup', async () => {
    const n = id(), component = api.LocalComponent.new(1), object = await api.OwnerCounter.new(component, n, 5);
    assert.equal(await object.value(), 5); assert.equal(await object.increment(2), 7); assert.equal(await object.addLabel(1, 'ab'), 10);
    api.openGate(n, 8); await Promise.all([object.close(), object.close()]); assert.equal(api.snapshot(n).dropped, 1); assert.equal(api.snapshot(n).cleaned, 1);
    object.dispose(); await component.close(); component.dispose();
  });
  await test('actor-send-local-fifo-and-business-error', async () => {
    for (const local of [false, true]) {
      const owner = id(), account = await api.ActorAccount.new(local, owner, 10);
      const first = id(), second = id(), third = id();
      const a = observe(account.write(first, 1, 2, false)); await until(() => api.snapshot(first).started === 1);
      const b = observe(account.write(second, 2, 0, false)); await until(() => api.snapshot(second).produced === 1);
      const c = observe(account.write(third, 3, 0, false)); await until(() => api.snapshot(third).produced === 1);
      assert.equal(api.snapshot(second).started + api.snapshot(third).started, 0);
      api.openGate(first, 1);
      assert.deepEqual(await a.result, { value: 11 }); assert.deepEqual(await b.result, { value: 13 }); assert.deepEqual(await c.result, { value: 16 });
      await assert.rejects(account.write(id(), 100, 1, false), error => error instanceof ark.ActorFixtureError && error.variant === 'Rejected');
      assert.equal(await account.write(id(), 4, 0, false), 20);
      api.openGate(owner, 8); await account.close(); assert.equal(api.snapshot(owner).cleaned, 1); account.dispose();
    }
  });
  await test('local-raw-stream-owner-scope-and-trait', async () => {
    const component = api.LocalComponent.new(1), owner = id(), object = await api.OwnerCounter.new(component, owner, 50);
    for (const method of ['rawValues', 'asyncRawValues', 'futureRawValues', 'traitRawValues']) {
      const n = id(); api.openGate(n, 2); api.openGate(n, 4);
      const stream = await (method === 'rawValues' ? object[method](n, false) : object[method](n));
      const first = await stream.next(), polls = api.snapshot(n).polls;
      assert.equal(first.kind, 'item'); assert.equal(first.value, null);
      await object.value(); assert.equal(api.snapshot(n).polls, polls); assert.equal(api.snapshot(n).produced, 1);
      assert.equal((await collect(stream)).length, 1); assert.equal(api.snapshot(n).dropped, 1); assert.equal(api.snapshot(n).cleaned, 1);
    }
    api.openGate(owner, 8); await object.close(); object.dispose(); await component.close(); component.dispose();
  });
  await test('local-pending-stream-and-cancelled-owner-close-await-cleanup', async () => {
    for (const ownerClose of [false, true]) {
      const component = api.LocalComponent.new(1), owner = id(), n = id(), object = await api.OwnerCounter.new(component, owner, 60);
      const stream = await object.rawValues(n, true), reading = observe(stream.next()); await until(() => api.snapshot(n).polls > 0);
      api.openGate(owner, 8);
      if (ownerClose) {
        const controller = new ark.CallController(), closing = observe(component.close({ signal: controller.signal }));
        await until(() => api.snapshot(n).cleaning === 1); controller.abort();
        const other = observe(component.close());
        assert.equal((await cleanup(n, closing)).error.errorName, 'UniffiCancelled'); assert.deepEqual(await other.result, { value: undefined });
      } else {
        assert.deepEqual(await cleanup(n, observe(stream.cancel())), { value: undefined });
        await object.close(); await component.close();
      }
      const step = (await reading.result).value; assert.equal(step.kind, 'error'); assert.equal(step.error.errorName, 'UniffiCancelled');
      await stream.cancel(); object.dispose(); component.dispose();
    }
  });
  await test('session-close-joins-pending-pull-and-producer-release', async () => {
    const started = gate(), cancelled = gate(), releaseStarted = gate(), release = gate(), pending = gate();
    let cancels = 0, releases = 0, cleaned = 0;
    const source = {
      async next() { started.resolve(); await pending.promise; return new ark.ArkDoneStep(); },
      async cancel() { cancels++; cancelled.resolve(); pending.resolve(); },
      async release() { releases++; releaseStarted.resolve(); await release.promise; cleaned++; },
    };
    const service = api.StreamService.new(), stream = await service.echoAsync(source), reading = observe(stream.next());
    await started.promise;
    const closing = observe(ark.close()), other = observe(ark.close());
    await cancelled.promise; await releaseStarted.promise;
    assert.equal(closing.settled || other.settled || reading.settled, false);
    assert.equal(cleaned, 0); assert.equal(cancels, 1); assert.equal(releases, 1);
    release.resolve();
    assert.deepEqual(await closing.result, { value: undefined }); assert.deepEqual(await other.result, { value: undefined });
    const step = (await reading.result).value; assert.equal(step.kind, 'error'); assert.equal(step.error.errorName, 'UniffiCancelled');
    assert.equal(cleaned, 1);
  });
  await ark.close();
  return { realm: 'ark-napi', cases, status: 'passed' };
};
