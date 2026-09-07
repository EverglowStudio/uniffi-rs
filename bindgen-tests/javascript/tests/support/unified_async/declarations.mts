import { unified_async_fixture as api } from './node/index.js';
import { ready } from './browser/index.js';

export async function declarations() {
  const value: Promise<number> = api.directFuture(1);
  const boxed: Promise<number> = api.directBoxFuture(2);
  const defaulted: number = api.rustDefault();
  const plain: AsyncIterable<number | null> = api.plainEvents(1);
  const syncFactory: AsyncIterable<number | null> = api.syncFactory(2, false);
  const asyncFactory: Promise<AsyncIterable<number | null>> = api.asyncFactory(3, false);
  const futureFactory: Promise<AsyncIterable<number | null>> = api.futureFactory(4, false);
  const service = api.StreamService.new();
  const method: AsyncIterable<number | null> = service.values(5);
  const methodFactory: Promise<AsyncIterable<number | null>> = service.valuesLater(6);
  const resultUnit: void = api.infallibleUnit();
  const resultUnitFuture: Promise<void> = api.infallibleFutureUnit();
  const resultValue: number = api.infallibleValue(1);
  const resultFuture: Promise<number> = api.infallibleFutureValue(2);
  const canonical: AsyncIterable<number> = api.canonicalInfallible();
  const resultItems: AsyncIterable<number> = api.infallibleResultItems();
  const resultFactory: AsyncIterable<number> = api.infallibleSyncFactory();
  const futureResultFactory: Promise<AsyncIterable<number>> = api.infallibleFutureFactory();
  const input: AsyncIterableIterator<number> = (async function* () { yield 1; })();
  const echo: AsyncIterable<number> = service.echo(input);
  const echoAsync: Promise<AsyncIterable<number>> = service.echoAsync(input);
  const browser = await ready;
  const browserFuture: Promise<number> = browser.unified_async_fixture.directBoxFuture(3);
  return { value, boxed, defaulted, plain, syncFactory, asyncFactory, futureFactory, method, methodFactory, browserFuture, resultValue, resultFuture, canonical, resultItems, resultFactory, futureResultFactory, echo, echoAsync, resultUnit, resultUnitFuture };
}
