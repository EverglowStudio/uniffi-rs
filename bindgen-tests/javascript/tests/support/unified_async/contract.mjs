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
  await test('local-generated-constructor-sync-async-borrow-and-business-error', async () => {
    const component = api.LocalComponent.new(1), id = nextId();
    const counter = await api.OwnerCounter.new(component, id, 10);
    assert(counter instanceof api.OwnerCounter && counts(id).started === 1, 'renamed local object constructor');
    const value = counter.value();
    assert(value instanceof Promise && await value === 10, 'Rust sync method is async across owner');
    assert(await counter.addLabel(3, 'ab') === 15, 'borrowed argument created on owner');
    assert(await counter.increment(4) === 19, 'Rc and reference survive Rust await');
    assert(await counter.fallible(false) === 19, 'business result success');
    const failure = await observe(counter.fallible(true)).result;
    assert(!failure.ok && failure.error instanceof api.FixtureError, 'local business error remains typed');
    open(id, 8);
    await Promise.all([counter.close(), counter.close()]);
    assert(counts(id).dropped === 1 && counts(id).cleaned === 1, 'close waits for object destruction and cleanup');
    const closed = await observe(counter.value()).result;
    assert(!closed.ok && !(closed.error instanceof api.FixtureError), 'closed is a framework error');
    counter.dispose(); counter.dispose();
    await component.close(); component.dispose();
  });
  await test('local-methods-interleave-and-host-callback-reenters', async () => {
    const component = api.LocalComponent.new(1), id = nextId(), blocked = nextId();
    const counter = await api.OwnerCounter.new(component, id, 0);
    const waiting = observe(counter.gatedIncrement(blocked, 10));
    await until(() => counts(blocked).started === 1, 'local gated method entered');
    assert(await counter.increment(1) === 1 && !waiting.settled, 'await does not serialize the object');
    const reentered = await counter.visit({ async visit() { return await counter.increment(1); } });
    assert(reentered === 4 && !waiting.settled, 'host callback can await same-owner method');
    open(blocked, 1);
    const result = await waiting.result;
    assert(result.ok && result.value === 12, 'suspended method resumes with current state');
    open(id, 8); await counter.close(); counter.dispose();
    await component.close(); component.dispose();
  });
  await test('local-call-cancellation-awaits-owner-nested-cleanup', async () => {
    const component = api.LocalComponent.new(1), id = nextId(), call = nextId(), pre = nextId();
    const counter = await api.OwnerCounter.new(component, id, 5);
    const aborted = new AbortController(); aborted.abort();
    cancelled(await observe(counter.guarded(pre, { signal: aborted.signal })).result, 'pre-aborted local');
    assert(counts(pre).started === 0 && counts(pre).dropped === 0, 'pre-aborted local body never starts');
    const controller = new AbortController();
    const operation = observe(counter.guarded(call, { signal: controller.signal }), () => counts(call));
    await until(() => counts(call).started === 1, 'local method entered');
    controller.abort(); controller.abort();
    cancelled(await finishCleanup(call, operation), 'local nested cleanup');
    assert(await counter.value() === 6 && counts(id).dropped === 0, 'cancel releases request and retains object');
    open(id, 8); await counter.close(); counter.dispose();
    await component.close(); component.dispose();
  });
  await test('local-close-cancels-calls-and-waits-for-body-drop', async () => {
    const component = api.LocalComponent.new(1), id = nextId(), call = nextId();
    const counter = await api.OwnerCounter.new(component, id, 0);
    const operation = observe(counter.guarded(call), () => counts(call));
    await until(() => counts(call).started === 1, 'method before close');
    const a = observe(counter.close(), () => counts(id));
    const b = observe(counter.close(), () => counts(id));
    cancelled(await finishCleanup(call, operation), 'object close cancels in-flight call');
    await until(() => counts(id).cleaning === 1, 'object destructor starts local cleanup');
    await delay(20);
    assert(!a.settled && !b.settled, 'all close waiters share destructor completion');
    open(id, 8);
    assert((await a.result).ok && (await b.result).ok, 'close succeeds after cleanup');
    assert(a.atSettlement.cleaned === 1 && b.atSettlement.cleaned === 1, 'object cleanup precedes close resolution');
    counter.dispose(); await component.close(); component.dispose();
  });
  await test('local-last-proxy-dispose-requests-owner-release', async () => {
    const component = api.LocalComponent.new(1), id = nextId();
    const counter = await api.OwnerCounter.new(component, id, 0);
    counter.dispose(); counter.dispose();
    await until(() => counts(id).cleaning === 1, 'last proxy triggers owner destruction');
    const closing = observe(component.close());
    await delay(20);
    assert(!closing.settled && counts(id).dropped === 1, 'owner close waits for detached proxy cleanup');
    open(id, 8); assert((await closing.result).ok, 'owner release completes');
    assert(counts(id).cleaned === 1, 'last proxy cleanup once'); component.dispose();
  });
  await test('local-owner-close-reclaims-all-objects', async () => {
    const component = api.LocalComponent.new(1), a = nextId(), b = nextId();
    const first = await api.OwnerCounter.new(component, a, 0);
    const second = await api.OwnerCounter.new(component, b, 0);
    open(a, 8);
    const closing = observe(component.close());
    await until(() => counts(b).cleaning === 1, 'owner closes every owned body');
    assert(!closing.settled, 'owner awaits all cleanup');
    open(b, 8); assert((await closing.result).ok, 'owner close complete');
    assert(counts(a).dropped === 1 && counts(b).dropped === 1 && counts(a).cleaned === 1 && counts(b).cleaned === 1,
      'all owned resources cleaned once');
    await Promise.all([first.close(), second.close(), component.close()]);
    first.dispose(); second.dispose(); component.dispose();
  });
  await test('local-cancelled-return-proxy-releases-body-before-settlement', async () => {
    const component = api.LocalComponent.new(1), id = nextId();
    const operation = observe(component.cancelReturnedCounter(id), () => counts(id));
    await until(() => counts(id).cleaning === 1, 'undelivered proxy releases its owner body');
    await delay(20);
    assert(!operation.settled && counts(id).dropped === 1, 'cancelled return awaits owner destructor cleanup');
    open(id, 8);
    cancelled(await operation.result, 'local returned proxy');
    assert(operation.atSettlement.cleaned === 1, 'no unpublished handle and cleanup before settlement');
    await component.close(); component.dispose();
  });
  for (const kind of ['object', 'owner']) {
    await test(`local-cancel-${kind}-close-awaits-original-cleanup`, async () => {
      const component = api.LocalComponent.new(1), id = nextId();
      const counter = await api.OwnerCounter.new(component, id, 0);
      const target = kind === 'object' ? counter : component;
      const controller = new AbortController();
      const operation = observe(target.close({ signal: controller.signal }), () => counts(id));
      await until(() => counts(id).cleaning === 1, `${kind} close started destructor cleanup`);
      controller.abort(); controller.abort();
      const other = observe(target.close(), () => counts(id));
      await delay(25);
      assert(!operation.settled && !other.settled && counts(id).cleaned === 0,
        `cancelled ${kind} close and second waiter both retain cleanup confirmation`);
      open(id, 8);
      cancelled(await operation.result, `cancelled ${kind} close`);
      assert((await other.result).ok, `second ${kind} close succeeds`);
      assert(operation.atSettlement.cleaned === 1 && other.atSettlement.cleaned === 1,
        `both ${kind} close waiters settle after destruction cleanup`);
      await component.close(); counter.dispose(); component.dispose();
    });
  }
  // S05: ordinary business methods backed by public Rust actor APIs. Each case
  // runs with Send(!Sync) state and native owner-local Rc state in all four realms.
  async function actorAccount(local, value = 0) {
    const id = nextId();
    const account = await api.ActorAccount.new(local, id, value);
    return { id, account };
  }
  async function closeAccount({ id, account }) {
    open(id, 8);
    await account.close();
    assert(counts(id).dropped === 1 && counts(id).cleaned === 1, 'actor body released before close');
    account.dispose();
  }
  function actorError(result, label, variant = null, kind = null) {
    assert(!result.ok && result.error instanceof api.ActorFixtureError, `${label}: typed business/lifecycle failure`);
    if (variant !== null) assert(result.error.variant === variant, `${label}: preserves error variant ${variant}`);
    if (kind !== null) assert(result.error.data?.kind === kind, `${label}: preserves runtime failure ${kind}`);
  }
  await test('actor-send-local-fifo-and-business-error', async () => {
    for (const local of [false, true]) {
      const ledger = await actorAccount(local, 10), { account } = ledger;
      assert(await account.value() === 10, 'actor normal ask');
      const first = nextId(), second = nextId(), third = nextId();
      const a = observe(account.write(first, 1, 2, false));
      await until(() => counts(first).started === 1, 'first actor handler started');
      const b = observe(account.write(second, 2, 0, false));
      await until(() => counts(second).produced === 1, 'second ask admission attempted with available capacity');
      const c = observe(account.write(third, 3, 0, false));
      await until(() => counts(third).produced === 1, 'third ask admitted behind second');
      assert(counts(second).started === 0 && counts(third).started === 0, 'awaiting actor handler never reenters');
      open(first, 1);
      assert((await a.result).value === 11 && (await b.result).value === 13 && (await c.result).value === 16, 'accepted writes execute FIFO');
      actorError(await observe(account.write(nextId(), 100, 1, false)).result, 'business rejection', 'Rejected');
      assert(await account.write(nextId(), 4, 0, false) === 20, 'business error does not stop actor');
      await closeAccount(ledger);
    }
  });
  await test('actor-abort-started-ask-waits-cooperative-recovery-and-cleanup', async () => {
    for (const local of [false, true]) {
      const ledger = await actorAccount(local, 5), { account } = ledger;
      const id = nextId(), controller = new AbortController();
      const call = observe(account.write(id, 3, 3, true, { signal: controller.signal }), () => counts(id));
      await until(() => counts(id).started === 1, 'actor handler started before abort');
      controller.abort(); controller.abort();
      await until(() => counts(id).polls === 1, 'handler observes cooperative request cancellation');
      assert(!call.settled && counts(id).dropped === 0, 'framework has not dropped mutable handler');
      open(id, 1);
      await until(() => counts(id).cleaning === 1, 'recovery ended and async cleanup entered');
      assert(counts(id).nested === 1 && !call.settled, 'recovery precedes cleanup and settlement');
      await delay(20); assert(!call.settled, 'Promise waits for Rust cleanup gate');
      open(id, 2); cancelled(await call.result, 'actor ask aborted');
      assert(call.atSettlement.cleaned === 1 && counts(id).dropped === 1 && counts(id).ownerDropped === 1, 'actor request release before Promise rejection');
      assert(await account.value() === 8, 'state recovery completed and actor remains usable');
      await closeAccount(ledger);
    }
  });
  await test('actor-queued-abort-releases-owner-without-executing', async () => {
    for (const local of [false, true]) {
      const ledger = await actorAccount(local), { account } = ledger;
      const active = nextId(), id = nextId();
      const first = observe(account.write(active, 1, 2, false));
      await until(() => counts(active).started === 1, 'active handler blocks queue');
      const controller = new AbortController();
      const queued = observe(account.write(id, 99, 0, true, { signal: controller.signal }), () => counts(id));
      await until(() => counts(id).produced === 1, 'one queued ask with free mailbox capacity');
      controller.abort();
      await until(() => counts(id).cleaning === 1, 'queued payload cleanup while handler still blocked');
      assert(counts(id).started === 0 && counts(id).ownerDropped === 1 && !first.settled && !queued.settled, 'owner releases queued payload without running its handler');
      open(id, 2); cancelled(await queued.result, 'queued actor abort');
      assert(queued.atSettlement.cleaned === 1 && counts(id).dropped === 1, 'queued confirmation follows cleanup once');
      open(active, 1); assert((await first.result).value === 1 && await account.value() === 1, 'withdrawn write never changes ledger');
      await closeAccount(ledger);
    }
  });
  await test('actor-drain-tracks-necessary-result-return', async () => {
    for (const local of [false, true]) {
      const ledger = await actorAccount(local, 10), { id, account } = ledger, work = nextId();
      await account.schedule(work, 7);
      await until(() => counts(work).started === 1, 'managed work started');
      const drain = observe(account.drain(), () => counts(id));
      await delay(20);
      assert(!drain.settled && counts(id).dropped === 0, 'empty mailbox does not end tracked drain');
      actorError(await observe(account.value()).result, 'drain closes external admission', 'Runtime', 'Closed');
      open(work, 1);
      await until(() => counts(id).cleaning === 1, 'necessary result applied and body release begins');
      assert(counts(work).nested === 1 && counts(id).produced === 17 && !drain.settled, 'drain includes necessary completion message');
      open(id, 8); assert((await drain.result).ok && drain.atSettlement.cleaned === 1, 'drain awaits body cleanup');
      await account.close(); account.dispose();
    }
  });
  await test('actor-close-cancelled-waiter-keeps-shared-confirmation', async () => {
    for (const local of [false, true]) {
      const ledger = await actorAccount(local), { id, account } = ledger, callId = nextId(), work = nextId();
      await account.schedule(work, 20);
      await until(() => counts(work).started === 1, 'close owns managed work');
      const call = observe(account.write(callId, 2, 3, true));
      await until(() => counts(callId).started === 1, 'close target handler started');
      const controller = new AbortController();
      const first = observe(account.close({ signal: controller.signal }), () => counts(id));
      await until(() => counts(callId).polls === 1 && counts(work).cleaned === 1, 'close cancels handler and managed work');
      controller.abort();
      const second = observe(account.close(), () => counts(id));
      assert(!first.settled && !second.settled, 'cancelled close still waits for handler recovery');
      open(callId, 1);
      await until(() => counts(callId).cleaning === 1, 'close waits request cleanup');
      assert(!first.settled && !second.settled, 'both close waiters retain request cleanup');
      open(callId, 2); actorError(await call.result, 'actor close cancels request without outer abort', 'Runtime', 'Cancelled');
      await until(() => counts(id).cleaning === 1, 'actor body cleanup started');
      await delay(20); assert(!first.settled && !second.settled, 'body cleanup gate holds both waiters');
      open(id, 8); cancelled(await first.result, 'cancelled close waiter');
      assert((await second.result).ok && first.atSettlement.cleaned === 1 && second.atSettlement.cleaned === 1, 'shared close is confirmed before either settlement');
      await account.close(); account.dispose();
    }
  });
  await test('actor-lifecycle-failure-is-not-masked-by-close', async () => {
    for (const local of [false, true]) {
      const { id, account } = await actorAccount(local);
      open(id, 8);
      actorError(await observe(account.fail()).result, 'run-level failure reaches active ask', 'Runtime', 'StartFailed');
      const first = await observe(account.close()).result, second = await observe(account.close()).result;
      actorError(first, 'failed owner close', 'Runtime', 'StartFailed'); actorError(second, 'repeated failed owner close', 'Runtime', 'StartFailed');
      assert(counts(id).dropped === 1 && counts(id).cleaned === 1, 'failed lifecycle still cleans once');
      actorError(await observe(account.value()).result, 'failed actor stays closed', 'Runtime', 'Closed');
      account.dispose();
    }
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
    cancelled(result, 'pending next interrupted by return');
    assert((await stream.next()).done === true, 'pending stream stays closed');
  });
  assert(cases.length === 27 && cases.filter(name => name.startsWith('actor-')).length === 6,
    `preserved S01/S02 (21) + S05 (6) baseline: ${cases.length}`);
  const collect = async (stream) => { const out = []; for await (const item of await stream) out.push(item); return out; };
  const deferred = () => { let resolve; const promise = new Promise((r) => { resolve = r; }); return { promise, resolve }; };
  await test('s03-direct-future-boxfuture-and-rust-default', async () => {
    assert(await api.directFuture(71) === 71 && await api.directBoxFuture(72) === 72, 'direct Future declarations');
    assert(api.rustDefault() === 0 && api.rustDefault(9) === 9, 'omitted argument executes Rust Default');
  });
  await test('s03-infallible-option-stream-no-prefetch', async () => {
    const id = nextId(), stream = api.plainEvents(id);
    assert((await stream.next()).value === 7, 'plain Item first value');
    const polls = counts(id).polls;
    await api.pendingValue(0);
    assert(counts(id).polls === polls && counts(id).produced === 1, 'no demand means no further polling');
    const none = await stream.next(); assert(!none.done && none.value === null, 'None is data');
    assert((await stream.next()).value === 9 && (await stream.next()).done, 'plain EOF');
    assert(counts(id).dropped === 1, 'plain EOF destroys source');
  });
  await test('s03-sync-async-future-factory-errors-independent', async () => {
    const directId = nextId(), direct = api.resultEvents(directId);
    assert((await direct.next()).value === 7, 'direct fallible stream first item');
    const directError = await observe(direct.next()).result;
    assert(!directError.ok && directError.error instanceof api.FixtureError && directError.error.variant === 'Boom', 'direct fallible stream first error');
    assert((await direct.next()).done && counts(directId).dropped === 1, 'direct fallible stream ends at first error');
    for (const factory of [api.syncFactory, api.asyncFactory, api.futureFactory]) {
      const bad = nextId();
      const failure = await observe((async () => { const stream = await factory(bad, true); return stream.next(); })()).result;
      assert(!failure.ok && failure.error instanceof api.FixtureError && failure.error.variant === 'Factory', 'factory error is independently typed');
      assert(counts(bad).started === 0, 'failed factory never constructs a stream');
      const id = nextId(), stream = await factory(id, false);
      assert((await stream.next()).value === 7, 'factory item before error');
      const item = await observe(stream.next()).result;
      assert(!item.ok && item.error instanceof api.FixtureError && item.error.variant === 'Boom', 'item error independent of factory');
      assert((await stream.next()).done && counts(id).produced === 2 && counts(id).dropped === 1, 'first item error terminal');
    }
  });
  await test('s03-object-stream-and-rust-trait-methods', async () => {
    const service = api.StreamService.new();
    for (const method of ['values', 'valuesLater', 'valuesFuture']) {
      assert(JSON.stringify(await collect(service[method](nextId()))) === '[7,null,9]', `object ${method} resource slots`);
    }
    const provider = api.RustStreamProvider.new();
    for (const [method, expected] of [['syncValues', [11,12]], ['asyncValues', [13,14]], ['boxedValues', [15,16]], ['staticValues', [17]]]) {
      assert(JSON.stringify(await collect(provider[method]())) === JSON.stringify(expected), `Rust trait ${method}`);
    }
    provider.dispose(); service.dispose();
  });
  await test('s03-host-trait-sync-async-boxed-stream', async () => {
    const provider = {
      syncValues() { return { async *[Symbol.asyncIterator]() { yield 21; yield 22; } }; },
      async asyncValues() { return { async *[Symbol.asyncIterator]() { yield 23; } }; },
      async boxedValues() { return { async *[Symbol.asyncIterator]() { yield 24; } }; },
      async staticValues() { return { async *[Symbol.asyncIterator]() { yield 25; } }; },
      async staticConsume(source) { return (await source.next()).value; },
      async consumeInput() { return 0; },
    };
    for (const [kind, expected] of [[0, [21,22]], [1, [23]], [2, [24]], [3, [25]]]) {
      assert(JSON.stringify(await api.hostValues(provider, kind)) === JSON.stringify(expected), `host stream kind ${kind}`);
    }
  });
  await test('s03-object-input-demand-drop-await-return-onclose', async () => {
    const service = api.StreamService.new(), id = nextId(), returned = deferred(), closed = deferred();
    let pulls = 0, returning = 0, closing = 0, cleaned = 0;
    const input = root.session.createInputStream({
      async next() { pulls++; return { done: false, value: 31 }; },
      async cancel() { returning++; await returned.promise; },
    }, { async onClose() { closing++; await closed.promise; cleaned++; } });
    const operation = observe(service.consume(input, id, 1), () => ({ cleaned }));
    await until(() => counts(id).produced === 1, 'Rust consumed one host item');
    await api.pendingValue(0);
    assert(pulls === 1 && returning === 0 && !operation.settled, 'single consumer has no prefetch or premature drop');
    open(id, 1);
    await until(() => returning === 1, 'Rust input Drop requests producer return');
    assert(!operation.settled && closing === 0, 'Rust completion waits producer return');
    returned.resolve(); await until(() => closing === 1, 'onClose entered after producer return');
    assert(!operation.settled, 'Rust completion waits onClose');
    closed.resolve(); const result = await operation.result;
    assert(result.ok && result.value === 31 && operation.atSettlement.cleaned === 1, 'real producer cleanup precedes result');
    assert(returning === 1 && closing === 1, 'producer closed once'); service.dispose();
  });
  await test('s03-host-input-error-cleans-producer', async () => {
    const service = api.StreamService.new(), id = nextId(); let drops = 0;
    const input = { async *[Symbol.asyncIterator]() { try { yield 1; throw new api.FixtureError('producer boom', 'Boom'); } finally { drops++; } } };
    const result = await observe(service.consume(input, id, 3)).result;
    assert(!result.ok && result.error instanceof api.FixtureError && drops === 1, 'host producer error propagates after generator finally');
    service.dispose();
  });
  await test('s03-host-trait-input-proxy-lives-through-await', async () => {
    const wait = deferred(), returned = deferred(); let pulled = 0, returning = 0, callbackDone = false;
    const input = root.session.createInputStream({
      async next() { pulled++; return { value: 41, done: false }; },
      async cancel() { returning++; await returned.promise; },
    });
    const provider = {
      syncValues() { return { async *[Symbol.asyncIterator]() {} }; },
      async asyncValues() { return this.syncValues(); },
      async boxedValues() { return this.syncValues(); },
      async staticValues() { return this.syncValues(); },
      async staticConsume(source) { return (await source.next()).value; },
      async consumeInput(source) {
        await wait.promise;
        try {
          for await (const item of source) {
            assert(item === 41, 'foreign producer remains alive during callback await');
            return item;
          }
          throw new Error('host input unexpectedly ended');
        } finally { callbackDone = true; }
      },
    };
    const operation = observe(api.hostConsume(provider, input));
    await api.pendingValue(0);
    assert(returning === 0 && pulled === 0, 'dispatch does not drop Rust input proxy');
    wait.resolve(); await until(() => returning === 1, 'for await return starts producer cleanup');
    assert(!callbackDone && !operation.settled, 'iterator return and Rust call both wait producer cleanup');
    returned.resolve(); assert((await operation.result).value === 41 && callbackDone, 'host input callback returns after cleanup');
    for (const unpolled of [false, true]) {
      const ack = deferred(); let returning = 0, calls = 0;
      const input = root.session.createInputStream({
        async next() { return { done: false, value: 42 }; },
        async cancel() { returning++; await ack.promise; },
      });
      const ownedProvider = { ...provider, async staticConsume(source) { calls++; return (await source.next()).value; } };
      const result = observe(api.hostStaticConsume(ownedProvider, input, unpolled));
      await until(() => returning === 1, 'static callback Future releases input after original Rust handler is dropped');
      assert(!result.settled && calls === (unpolled ? 0 : 1), 'unpolled Drop or awaited static callback both wait cleanup');
      ack.resolve(); const completed = await result.result;
      assert(completed.ok && completed.value === (unpolled ? 0 : 42), 'owned static Future survives handler drop');
    }
  });
  await test('s03-stream-return-pending-next-awaits-cleanup', async () => {
    const id = nextId(), stream = api.cleanupEvents(id), next = observe(stream.next());
    await until(() => counts(id).polls > 0, 'stream is pending');
    const first = observe(stream.return(), () => counts(id)), second = observe(stream.return(), () => counts(id));
    const result = await finishCleanup(id, first);
    assert(result.ok && (await second.result).ok && second.atSettlement.cleaned === 1, 'concurrent return shares cleanup ack');
    cancelled(await next.result, 'pending next cleanup cancellation');
  });
  await test('s03-cancelled-factory-drops-undelivered-stream', async () => {
    const id = nextId(), operation = observe(api.cancelledFactory(id), () => counts(id));
    cancelled(await finishCleanup(id, operation), 'factory cancels before delivering stream');
    assert(counts(id).polls === 0 && counts(id).dropped === 1, 'raw stream was never registered or polled');
    const component = api.LocalComponent.new(1), owner = nextId(), localId = nextId(), counter = await api.OwnerCounter.new(component, owner, 0);
    const localFactory = observe(counter.cancelRawValues(localId), () => counts(localId));
    cancelled(await finishCleanup(localId, localFactory), 'local factory cancels raw !Send stream before delivery');
    assert(counts(localId).polls === 0 && counts(localId).dropped === 1, 'undelivered raw local stream never polled');
    open(owner, 8); await counter.close(); counter.dispose(); await component.close(); component.dispose();
  });
  await test('s03-local-raw-stream-owner-scope-and-trait', async () => {
    const component = api.LocalComponent.new(1), owner = nextId(), counter = await api.OwnerCounter.new(component, owner, 50);
    for (const method of ['rawValues', 'asyncRawValues', 'futureRawValues', 'traitRawValues']) {
      const id = nextId(); open(id, 2); open(id, 4);
      const stream = method === 'rawValues' ? counter[method](id, false) : counter[method](id);
      const iterator = await stream, first = await iterator.next();
      const polls = counts(id).polls;
      await counter.value();
      assert(counts(id).polls === polls && counts(id).produced === 1, 'owner subscription does not prefetch while consumer pauses');
      const values = [first.value, ...await collect(iterator)];
      assert(values[0] === null && values.length === 2, `local ${method} keeps Item Option`);
      assert(counts(id).dropped === 1 && counts(id).cleaned === 1, 'owner EOF cleanup acknowledged');
    }
    open(owner, 8); await counter.close(); counter.dispose(); await component.close(); component.dispose();
  });
  await test('s03-local-pending-stream-return-owner-cleanup', async () => {
    const component = api.LocalComponent.new(1), owner = nextId(), id = nextId(), counter = await api.OwnerCounter.new(component, owner, 60);
    const stream = await counter.rawValues(id, true), next = observe(stream.next());
    await until(() => counts(id).polls > 0, 'raw !Send stream pending on owner');
    const closing = observe(stream.return(), () => counts(id));
    assert((await finishCleanup(id, closing)).ok, 'local subscription cleanup acknowledged');
    cancelled(await next.result, 'local pending next');
    open(owner, 8); await counter.close(); counter.dispose(); await component.close(); component.dispose();
  });
  await test('s03-local-stream-cancel-owner-close-waits-cleanup', async () => {
    const component = api.LocalComponent.new(1), owner = nextId(), id = nextId(), counter = await api.OwnerCounter.new(component, owner, 60);
    const stream = await counter.rawValues(id, true), next = observe(stream.next());
    await until(() => counts(id).polls > 0, 'owner subscription pending before close');
    open(owner, 8);
    const abort = new AbortController(), closing = observe(component.close({ signal: abort.signal }), () => counts(id));
    await until(() => counts(id).cleaning === 1, 'owner closes subscription');
    abort.abort();
    const other = observe(component.close(), () => counts(id));
    cancelled(await finishCleanup(id, closing), 'cancelled owner close with live stream');
    assert((await other.result).ok && other.atSettlement.cleaned === 1, 'other owner close waits same stream cleanup');
    cancelled(await next.result, 'owner closes pending stream after cleanup');
    await stream.return(); counter.dispose(); component.dispose();
  });
  await test('r1-infallible-result-value-shapes', async () => {
    assert(api.infallibleValue(81) === 81, 'sync Result value');
    assert(await api.infallibleAsyncValue(82) === 82, 'async Result value');
    assert(await api.infallibleFutureValue(83) === 83, 'boxed Future Result value');
    assert(api.infallibleUnit() === undefined, 'sync Result unit');
    assert(await api.infallibleAsyncUnit() === undefined, 'async Result unit');
    assert(await api.infallibleFutureUnit() === undefined, 'boxed Future Result unit');
    for (const method of ['new', 'newArc', 'newFuture']) {
      const object = await api.ResultConstructed[method](84);
      assert(object.value() === 84, `${method} infallible Result constructor keeps Self/Arc conversion`);
      object.dispose();
    }
  });
  await test('r1-infallible-result-stream-shapes', async () => {
    for (const [method, expected] of [
      ['canonicalInfallible', [71, 72]], ['infallibleResultItems', [73, 74]],
      ['infallibleSyncFactory', [71, 72]], ['infallibleAsyncFactory', [75, 76]],
      ['infallibleFutureFactory', [71, 72]],
    ]) {
      const values = await collect(await api[method]());
      assert(JSON.stringify(values) === JSON.stringify(expected), `${method} keeps original Result shape`);
    }
  });
  await test('r1-host-infallible-result-shapes', async () => {
    const items = (value) => ({ async *[Symbol.asyncIterator]() { yield value; } });
    const provider = {
      unit() {}, async asyncUnit() {}, async futureUnit() {},
      value() { return 91; }, async asyncValue() { return 92; }, async futureValue() { return 93; },
      canonical() { return items(94); }, async items() { return items(95); },
      factory() { return items(96); }, async asyncFactory() { return items(97); },
      async futureFactory() { return items(98); },
    };
    for (let kind = 0; kind < 11; kind++) {
      const values = await api.hostInfallible(provider, kind);
      assert(values.length === 1 && values[0] === 91 + kind, `reverse original Result shape ${kind}`);
    }
  });
  await test('r1-method-input-stream-transfer', async () => {
    const service = api.StreamService.new();
    for (const method of ['echo', 'echoAsync']) {
      const ack = deferred(), closed = deferred(); let pulls = 0, returning = 0, closing = 0, cleaned = 0;
      const input = root.session.createInputStream({
        async next() { pulls++; return { done: false, value: 100 + pulls }; },
        async cancel() { returning++; await ack.promise; },
      }, { async onClose() { closing++; await closed.promise; cleaned++; } });
      const output = await service[method](input);
      assert(pulls === 0 && returning === 0, `${method} factory preserves the producer without prefetch`);
      assert((await output.next()).value === 101 && (await output.next()).value === 102, `${method} pulls after factory return`);
      await api.pendingValue(0);
      assert(pulls === 2 && returning === 0, `${method} producer remains subscribed`);
      const first = observe(output.return(), () => ({ cleaned })), second = observe(output.return(), () => ({ cleaned }));
      await until(() => returning === 1, `${method} subscription return closes producer`);
      assert(!first.settled && !second.settled && closing === 0, `${method} close awaits producer return gate`);
      ack.resolve(); await until(() => closing === 1, `${method} producer onClose entered`);
      assert(!first.settled && !second.settled, `${method} close awaits producer onClose gate`);
      closed.resolve();
      assert((await first.result).ok && (await second.result).ok && first.atSettlement.cleaned === 1 && second.atSettlement.cleaned === 1,
        `${method} concurrent return shares real cleanup`);
      assert(returning === 1 && closing === 1 && (await output.next()).done, `${method} producer closes once`);
    }
    for (const method of ['echoFail', 'echoCancel']) {
      const ack = deferred(); let pulls = 0, returning = 0, cleaned = 0;
      const input = root.session.createInputStream({
        async next() { pulls++; return { done: false, value: 1 }; },
        async cancel() { returning++; await ack.promise; cleaned++; },
      });
      const operation = observe(service[method](input), () => ({ cleaned }));
      await until(() => returning === 1, `${method} closes undelivered input`);
      assert(!operation.settled && pulls === 0, `${method} waits cleanup without polling producer`);
      ack.resolve(); const result = await operation.result;
      assert(operation.atSettlement.cleaned === 1 && returning === 1, `${method} cleanup precedes terminal result`);
      if (method === 'echoCancel') cancelled(result, 'cancelled echo factory');
      else assert(!result.ok && result.error instanceof api.FixtureError && result.error.variant === 'Factory', 'echo factory error stays independent');
    }
    service.dispose();
  });
  assert(cases.length === 44 && cases.filter(name => name.startsWith('s03-')).length === 13 && cases.filter(name => name.startsWith('r1-')).length === 4,
    `complete S01/S02/S03/S05 matrix: ${cases.length}`);
  await deadline(root.close(), `${realm} final session cleanup`);
  return { realm, cases, status: 'passed' };
}
