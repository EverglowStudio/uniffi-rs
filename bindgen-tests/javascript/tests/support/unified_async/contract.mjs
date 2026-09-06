// Identical public-facade assertions in Node NAPI, Node WASM, Window and Worker.
export function assert(value, label) {
  if (!value) throw new Error(`unified async: ${label}`);
}

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
export async function deadline(promise, label, ms = 5000) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`deadline: ${label} (${ms}ms)`)), ms);
      }),
    ]);
  } finally { clearTimeout(timer); }
}

async function until(check, label) {
  const end = performance.now() + 5000;
  while (!check()) {
    if (performance.now() >= end) throw new Error(`deadline: ${label}`);
    await delay(2);
  }
}

function observe(promise, snapshot) {
  const state = { settled: false, atSettlement: null };
  state.result = Promise.resolve(promise).then(
    (value) => ({ ok: true, value }),
    (error) => ({ ok: false, error }),
  ).then((result) => {
    state.atSettlement = snapshot?.();
    state.settled = true;
    return result;
  });
  return state;
}

function cancelled(result, label) {
  assert(!result.ok && result.error?.errorName === 'UniffiCancelled',
    `${label}: expected UniffiCancelled, got ${result.ok ? result.value : result.error?.stack ?? result.error}`);
}

export async function runContract(root, realm) {
  await deadline(root.ready, `${realm} facade ready`);
  const api = root.unified_async_fixture;
  assert(api, `${realm} generated namespace`);
  const cases = [];
  let sequence = 0;
  const nextId = () => ++sequence;
  const counts = (id) => api.snapshot(id);
  const open = (id, gate) => api.openGate(id, gate);
  async function test(name, run) {
    await deadline(run(), `${realm}/${name}`, 15000);
    cases.push(name);
    console.log(JSON.stringify({ realm, case: name, status: 'passed' }));
  }

  async function finishCleanup(id, operation) {
    await until(() => counts(id).cleaning === 1, `${id} cleanup entered`);
    assert(counts(id).dropped === 1, `${id} business drop once`);
    const polls = counts(id).polls;
    await delay(25);
    assert(!operation.settled && counts(id).cleaned === 0, `${id} Promise waits for outer gate`);
    assert(counts(id).polls === polls, `${id} no business polls after drop`);
    open(id, 2);
    await until(() => counts(id).nested === 1, `${id} nested Drop registers cleanup`);
    await delay(25);
    assert(!operation.settled && counts(id).cleaned === 0, `${id} Promise waits for nested gate`);
    open(id, 4);
    const result = await deadline(operation.result, `${id} result after cleanup`);
    assert(operation.atSettlement.cleaned === 1, `${id} Rust cleaned before Promise settles`);
    assert(counts(id).dropped === 1 && counts(id).nested === 1, `${id} cleanup/drop once`);
    return result;
  }

  await test('ready-pending-send-not-sync-and-wasm-rc', async () => {
    assert(await api.ready(41) === 41, 'ready value');
    assert(await api.pendingValue(42) === 42, 'real Pending with Cell / wasm Rc');
    assert(await api.declarationProbe(43) === 43, 'impl Future + trait RPITIT Send adapters');
    const payload = { value: 46, signal: 'business-data' };
    const controller = new AbortController();
    for (const result of [await api.echoPayload(payload), await api.echoPayload(payload, { signal: controller.signal })]) {
      assert(result.value === 46 && result.signal === 'business-data', 'object business argument stays separate from trailing options');
    }
  });
  await test('sleep-timeout-spawn-and-detach', async () => {
    const start = performance.now();
    assert(await api.sleepValue(44, 20) === 44, 'sleep value');
    assert(performance.now() - start >= 15, 'sleep actually suspends');
    assert(await api.timerChecks(), 'zero / expired / maximum timer checks');
    assert(await api.spawnValue(45) === 45, 'spawn accepts Cell / wasm Rc and output');
    const id = nextId();
    await api.detachSpawn(id);
    assert(counts(id).cleaned === 0, 'detached task waits for gate');
    open(id, 1);
    await until(() => counts(id).cleaned === 1, 'handle drop detaches task');
  });
  await test('pre-aborted-never-polls-business', async () => {
    const id = nextId();
    const controller = new AbortController();
    controller.abort();
    const operation = observe(api.guarded(id, 99, { signal: controller.signal }), () => counts(id));
    cancelled(await deadline(operation.result, 'pre-aborted'), 'pre-aborted');
    assert(counts(id).started === 0 && counts(id).polls === 0 && counts(id).dropped === 0,
      'pre-aborted body and local resource construction never run');
  });
  await test('per-call-cancellation-and-nested-cleanup', async () => {
    const a = nextId(), b = nextId();
    const ca = new AbortController(), cb = new AbortController();
    const pa = observe(api.guarded(a, 101, { signal: ca.signal }), () => counts(a));
    const pb = observe(api.guarded(b, 202, { signal: cb.signal }), () => counts(b));
    await until(() => counts(a).started === 1 && counts(b).started === 1, 'concurrent business started');
    ca.abort(); ca.abort();
    cancelled(await finishCleanup(a, pa), 'cancel target');
    assert(!pb.settled && counts(b).dropped === 0, 'same operation other call remains alive');
    open(b, 1);
    const result = await finishCleanup(b, pb);
    assert(result.ok && result.value === 202, 'independent call completes with its business args');
    cb.abort(); ca.abort();
    await delay(10);
    assert(counts(a).dropped === 1 && counts(b).dropped === 1, 'late/repeated abort does not redrop');
  });
  await test('timeout-and-task-cancel-await-cleanup', async () => {
    for (const fn of [api.timeoutGuarded, api.cancelSpawn]) {
      const id = nextId();
      const operation = observe(fn(id), () => counts(id));
      const result = await finishCleanup(id, operation);
      assert(result.ok && result.value === true, 'timeout / cancel_and_wait confirmed after nested cleanup');
    }
  });
  await test('completion-wins-before-abort-during-cleanup', async () => {
    const id = nextId(), controller = new AbortController();
    const operation = observe(api.guarded(id, 304, { signal: controller.signal }), () => counts(id));
    await until(() => counts(id).started === 1, 'completion winner started');
    open(id, 1);
    await until(() => counts(id).cleaning === 1, 'business result committed before abort');
    controller.abort(); controller.abort();
    const result = await finishCleanup(id, operation);
    assert(result.ok && result.value === 304, 'committed completion survives abort during cleanup');
  });
  await test('completion-cancellation-races', async () => {
    for (let i = 0; i < 12; i++) {
      const id = nextId(), controller = new AbortController();
      const operation = observe(api.guarded(id, 303, { signal: controller.signal }), () => counts(id));
      await until(() => counts(id).started === 1, 'race started');
      // Accept either winner, but never settle before cleanup or release twice.
      if (i % 2) { open(id, 1); controller.abort(); }
      else { controller.abort(); open(id, 1); }
      open(id, 2); open(id, 4);
      const result = await deadline(operation.result, 'race result');
      if (result.ok) assert(result.value === 303, 'race completion payload');
      else cancelled(result, 'race cancellation');
      assert(operation.atSettlement.cleaned === 1, 'race settles only after cleanup');
      controller.abort();
      assert(counts(id).dropped === 1 && counts(id).cleaned === 1, 'race single terminal cleanup');
    }
  });
  await test('cancelled-return-object-drops-before-settlement', async () => {
    const plain = nextId();
    const returned = observe(api.cancelWithObject(plain, false), () => counts(plain));
    cancelled(await deadline(returned.result, 'cancelled returned object'), 'cancelled returned object');
    assert(returned.atSettlement.dropped === 1, `cancelled returned object drops before settlement, without leaking a handle: ${JSON.stringify(returned.atSettlement)}`);
    const cleanup = nextId();
    const waiting = observe(api.cancelWithObject(cleanup, true), () => counts(cleanup));
    cancelled(await finishCleanup(cleanup, waiting), 'returned object nested cleanup');
  });
  await test('stream-none-eof-and-factory-adapter', async () => {
    for (const factory of [(id) => api.events(id, 0), api.factoryEvents]) {
      const id = nextId();
      const stream = factory(id);
      assert(counts(id).started === 0, 'stream / async factory is demand driven');
      const values = [];
      for await (const item of stream) values.push(item);
      assert(JSON.stringify(values) === '[7,null,9]', 'None is an item, not EOF');
      assert(counts(id).dropped === 1, 'EOF drops stream once');
      await stream.return(); await stream.return();
      assert(counts(id).dropped === 1, 'EOF repeated return does not redrop');
    }
    const id = nextId();
    assert((await api.events(id, 3).next()).done === true, 'empty EOF');
    assert(counts(id).dropped === 1, 'empty EOF drops once');
  });
  await test('stream-first-error-terminates', async () => {
    const id = nextId(), stream = api.events(id, 1);
    assert((await stream.next()).value === 7, 'value before error');
    const result = await observe(stream.next()).result;
    assert(!result.ok && result.error instanceof api.FixtureError, 'typed first business error');
    const polls = counts(id).polls;
    assert((await stream.next()).done === true, 'first error ends stream');
    await stream.return(); await stream.return();
    assert(counts(id).polls === polls && counts(id).produced === 2 && counts(id).dropped === 1,
      'no post-error business item or poll; drop once');
  });
  await test('stream-slow-consumer-no-prefetch-return-once', async () => {
    const id = nextId(), stream = api.events(id, 0);
    assert((await stream.next()).value === 7, 'slow consumer first item');
    const polls = counts(id).polls;
    await delay(40);
    assert(counts(id).polls === polls && counts(id).produced === 1, 'bridge does not prefetch');
    await Promise.all([stream.return(), stream.return()]);
    await stream.cancel();
    assert(counts(id).dropped === 1, 'repeated return/cancel closes once');
    assert((await stream.next()).done === true, 'closed remains done');
  });
  await test('stream-return-cancels-pending-next', async () => {
    const id = nextId(), stream = api.events(id, 2);
    const pendingNext = observe(stream.next());
    await until(() => counts(id).polls > 0, 'native pending next has been polled');
    await deadline(Promise.all([stream.return(), stream.return()]), 'pending stream returns');
    assert(counts(id).dropped === 1, 'return confirms native drop before resolution');
    const result = await deadline(pendingNext.result, 'pending next cancellation');
    assert(result.ok && result.value.done === true, 'pending next ends on return');
    assert((await stream.next()).done === true, 'pending stream stays closed');
  });
  return { realm, cases, status: 'passed' };
}
