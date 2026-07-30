// Shared TypeScript runtime for uniffi_bindgen_javascript.
//
// This file is copied verbatim into `common/runtime.ts` by the generator
// (via `include_str!`), so it must not depend on anything outside itself.
// It owns: the stable `UniffiError` class, the backend install hook, the
// sync/async call wrappers, a handle registry with a FinalizationRegistry
// safety net, minimal numeric normalisation, and the callback registry
// used to forward `Logger`-style foreign traits into the native backend.
//
// Contract version is bumped whenever the shape any flavor adapter relies
// on changes in a breaking way.

export const UNIFFI_JS_CONTRACT_VERSION = 1;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

export interface UniffiErrorInit {
    errorName: string;
    variant?: string | null;
    data?: unknown;
    message: string;
    stack?: string;
}

export class UniffiError extends Error {
    readonly errorName: string;
    readonly variant: string | null;
    readonly data: unknown;
    constructor(init: UniffiErrorInit) {
        super(init.message);
        this.name = "UniffiError";
        this.errorName = init.errorName;
        this.variant = init.variant ?? null;
        this.data = init.data;
        if (init.stack) this.stack = init.stack;
    }
}

/** Serialised error payload sent across the Electron contextBridge. */
export interface SerializedUniffiError {
    errorName: string;
    variant: string | null;
    data: unknown;
    message: string;
    stack?: string;
}

export function serializeUniffiError(raw: unknown): SerializedUniffiError {
    if (raw instanceof UniffiError) {
        return {
            errorName: raw.errorName,
            variant: raw.variant,
            data: raw.data,
            message: raw.message,
            stack: raw.stack,
        };
    }
    if (raw && typeof raw === "object") {
        const r = raw as Record<string, unknown>;
        return {
            errorName:
                typeof r.errorName === "string" ? r.errorName : "UniffiUnknownError",
            variant: typeof r.variant === "string" ? (r.variant as string) : null,
            data: r.data ?? null,
            message:
                typeof r.message === "string" ? (r.message as string) : String(raw),
            stack: typeof r.stack === "string" ? (r.stack as string) : undefined,
        };
    }
    return {
        errorName: "UniffiUnknownError",
        variant: null,
        data: null,
        message: String(raw),
    };
}

function wrapError(raw: unknown): UniffiError {
    if (raw instanceof UniffiError) return raw;
    const s = serializeUniffiError(raw);
    return new UniffiError(s);
}

// ---------------------------------------------------------------------------
// Backend install hook
// ---------------------------------------------------------------------------

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type UniffiBackend = any;

let __uniffiBackend: UniffiBackend = null;

export function __installBackend(backend: UniffiBackend): void {
    __uniffiBackend = backend;
}

export function __getBackend(): UniffiBackend {
    return __uniffiBackend;
}

function requireBackend(fn: string): UniffiBackend {
    if (!__uniffiBackend) {
        throw new UniffiError({
            errorName: "UniffiBackendMissing",
            message: `backend not installed before calling ${fn}`,
        });
    }
    return __uniffiBackend;
}

export function __call<T>(fn: string, ...args: unknown[]): T {
    const backend = requireBackend(fn);
    try {
        return backend[fn](...args) as T;
    } catch (raw) {
        throw wrapError(raw);
    }
}

export async function __callAsync<T>(fn: string, ...args: unknown[]): Promise<T> {
    const backend = requireBackend(fn);
    try {
        return (await backend[fn](...args)) as T;
    } catch (raw) {
        throw wrapError(raw);
    }
}

// ---------------------------------------------------------------------------
// Native stream wrapper
// ---------------------------------------------------------------------------

/**
 * Binding-internal result of one native output-stream pull.
 *
 * `done` is the sole completion discriminator. In particular, an item value
 * may be `null` when Rust yields `Some(None)` from `Stream<Option<T>>`.
 */
export type UniffiStreamNext<T> =
    | { done: false; value: T }
    | { done: true; value?: undefined };

export interface UniffiAsyncIterableOptions<T> {
    handle: unknown;
    next: (handle: unknown) => Promise<UniffiStreamNext<T>>;
    cancel: (handle: unknown) => void | Promise<void>;
}

/**
 * Wraps UniFFI's low-level stream handle ABI as a single-consumer
 * AsyncIterable. The Rust side is pull-based (`next(handle)`) and
 * cancellation-based (`cancel(handle)`), so the JS wrapper keeps the
 * ownership rules explicit:
 *
 * - The iterable can be consumed once.
 * - `next()` calls must not overlap.
 * - `return()` / `break` cancels exactly once.
 * - A stream error rejects `next()` and closes the iterator.
 */
export function createUniffiAsyncIterable<T>(
    options: UniffiAsyncIterableOptions<T>,
): AsyncIterable<T> {
    let consumed = false;
    let closed = false;
    let pending = false;
    let cancelStarted = false;

    const closeWithCancel = async (): Promise<void> => {
        if (cancelStarted) return;
        cancelStarted = true;
        closed = true;
        await options.cancel(options.handle);
    };

    return {
        [Symbol.asyncIterator](): AsyncIterator<T> {
            if (consumed) {
                throw new UniffiError({
                    errorName: "UniffiStreamConsumed",
                    message:
                        "uniffi stream AsyncIterable can only be consumed once",
                });
            }
            consumed = true;
            return {
                async next(): Promise<IteratorResult<T>> {
                    if (closed) return { done: true, value: undefined };
                    if (pending) {
                        throw new UniffiError({
                            errorName: "UniffiStreamConcurrentNext",
                            message:
                                "concurrent next() on a uniffi stream is not supported",
                        });
                    }
                    pending = true;
                    try {
                        const result = await options.next(options.handle);
                        if (
                            result === null ||
                            typeof result !== "object" ||
                            typeof result.done !== "boolean" ||
                            (!result.done &&
                                !Object.prototype.hasOwnProperty.call(
                                    result,
                                    "value",
                                ))
                        ) {
                            throw new UniffiError({
                                errorName: "UniffiStreamProtocolError",
                                message:
                                    "uniffi stream next returned an invalid result envelope",
                            });
                        }
                        // A concurrent return()/throw() may cancel a pending native pull.
                        // Never surface a late item after that terminal transition.
                        if (closed || result.done) {
                            closed = true;
                            return { done: true, value: undefined };
                        }
                        return { done: false, value: result.value };
                    } catch (raw) {
                        closed = true;
                        // Native error paths must close the registry entry. Cancellation is
                        // best-effort here so a cleanup failure cannot replace the stream error
                        // or become an unhandled rejection.
                        void closeWithCancel().catch(() => {});
                        throw wrapError(raw);
                    } finally {
                        pending = false;
                    }
                },
                async return(): Promise<IteratorResult<T>> {
                    await closeWithCancel();
                    return { done: true, value: undefined };
                },
                async throw(error?: unknown): Promise<IteratorResult<T>> {
                    try {
                        await closeWithCancel();
                    } catch {
                        // AsyncIterator.throw() must reject with the caller-supplied error,
                        // even if best-effort native cleanup fails.
                    }
                    throw error;
                },
            };
        },
    };
}

// ---------------------------------------------------------------------------
// Foreign input stream registry
// ---------------------------------------------------------------------------

export type UniffiInputStreamErrorShape = "flat" | "shape";

export interface UniffiInputStreamOptions<T, E> {
    lowerItem: (value: T) => unknown;
    lowerError: (error: unknown) => E;
    errorShape: UniffiInputStreamErrorShape;
}

export type UniffiInputStreamNext<T, E> =
    | { ok: true; done: true }
    | { ok: true; done: false; value: T }
    | { ok: false; error: E };

export interface UniffiInputStreamMarker<T = unknown, E = unknown> {
    __uniffiInputStream: true;
    handle: number;
    next: (...rawArgs: unknown[]) => Promise<UniffiInputStreamNext<T, E>>;
    cancel: (...rawArgs: unknown[]) => void;
}

type InputStreamSlot = {
    iterator: AsyncIterator<unknown>;
    lowerItem: (value: unknown) => unknown;
    lowerError: (error: unknown) => unknown;
    errorShape: UniffiInputStreamErrorShape;
    closed: boolean;
    pending: boolean;
    cancelStarted: boolean;
};

const INPUT_STREAMS = new Map<number, InputStreamSlot>();
let nextInputStreamHandle = 1;

function inputStreamHandleArg(rawArgs: unknown[]): number {
    const args =
        rawArgs.length >= 2 &&
        (rawArgs[0] === null ||
            rawArgs[0] === undefined ||
            rawArgs[0] instanceof Error)
            ? rawArgs.slice(1)
            : rawArgs;
    const handle = args[0];
    if (typeof handle !== "number" || !Number.isInteger(handle) || handle <= 0) {
        throw new UniffiError({
            errorName: "UniffiInputStreamHandle",
            message: "invalid uniffi input stream handle",
        });
    }
    return handle;
}

export function inputStreamErrorPayload(
    raw: unknown,
    shape: UniffiInputStreamErrorShape,
): unknown {
    const fromVariant = (
        variant: unknown,
        data: unknown,
        fallback: unknown,
    ): unknown => {
        if (shape === "flat") {
            return typeof variant === "string" ? variant : fallback;
        }
        if (typeof variant !== "string") return fallback;
        if (data !== null && typeof data === "object" && !Array.isArray(data)) {
            return { tag: variant, ...(data as Record<string, unknown>) };
        }
        return { tag: variant };
    };

    if (raw instanceof UniffiError) {
        return fromVariant(raw.variant, raw.data, raw);
    }
    if (raw !== null && typeof raw === "object") {
        const obj = raw as Record<string, unknown>;
        if (shape === "flat") {
            if (typeof obj.variant === "string") return obj.variant;
            if (typeof obj.tag === "string") return obj.tag;
            if (typeof obj.type === "string") return obj.type;
            return raw;
        }
        if (typeof obj.tag === "string") return raw;
        if (typeof obj.type === "string") {
            const { type, ...data } = obj;
            return { tag: type, ...data };
        }
        if (typeof obj.variant === "string") {
            return fromVariant(obj.variant, obj.data, raw);
        }
    }
    return raw;
}

export function createUniffiInputStream<T, E>(
    source: AsyncIterable<T>,
    options: UniffiInputStreamOptions<T, E>,
): UniffiInputStreamMarker<unknown, E> {
    if (
        source === null ||
        typeof source !== "object" ||
        typeof (source as AsyncIterable<T>)[Symbol.asyncIterator] !== "function"
    ) {
        throw new UniffiError({
            errorName: "UniffiInputStreamType",
            message: "expected an AsyncIterable for uniffi input stream argument",
        });
    }

    const iterator = source[Symbol.asyncIterator]();
    const handle = nextInputStreamHandle++;
    INPUT_STREAMS.set(handle, {
        iterator: iterator as AsyncIterator<unknown>,
        lowerItem: options.lowerItem as (value: unknown) => unknown,
        lowerError: options.lowerError as (error: unknown) => unknown,
        errorShape: options.errorShape,
        closed: false,
        pending: false,
        cancelStarted: false,
    });

    return {
        __uniffiInputStream: true,
        handle,
        next: (...rawArgs: unknown[]) =>
            nextUniffiInputStream<E>(inputStreamHandleArg(rawArgs)),
        cancel: (...rawArgs: unknown[]): void => {
            void cancelUniffiInputStream(inputStreamHandleArg(rawArgs));
        },
    };
}

export async function nextUniffiInputStream<E = unknown>(
    handle: number,
): Promise<UniffiInputStreamNext<unknown, E>> {
    const slot = INPUT_STREAMS.get(handle);
    if (!slot || slot.closed) return { ok: true, done: true };
    if (slot.pending) {
        throw new UniffiError({
            errorName: "UniffiInputStreamConcurrentNext",
            message: "concurrent next() on a uniffi input stream is not supported",
        });
    }
    slot.pending = true;
    try {
        const result = await slot.iterator.next();
        if (result.done === true) {
            slot.closed = true;
            INPUT_STREAMS.delete(handle);
            return { ok: true, done: true };
        }
        return { ok: true, done: false, value: slot.lowerItem(result.value) };
    } catch (raw) {
        slot.closed = true;
        INPUT_STREAMS.delete(handle);
        return {
            ok: false,
            error: slot.lowerError(inputStreamErrorPayload(raw, slot.errorShape)) as E,
        };
    } finally {
        slot.pending = false;
    }
}

export async function cancelUniffiInputStream(handle: number): Promise<void> {
    const slot = INPUT_STREAMS.get(handle);
    if (!slot || slot.cancelStarted) return;
    slot.cancelStarted = true;
    slot.closed = true;
    INPUT_STREAMS.delete(handle);
    const returnFn = slot.iterator.return;
    if (typeof returnFn === "function") {
        await returnFn.call(slot.iterator);
    }
}

// ---------------------------------------------------------------------------
// Numeric normalisation
//
// The high-level contract exposes Rust `i64` / `u64` as JS `bigint`.
// `toI64` / `toU64` validate input and convert to `bigint` for the backend.
// Accepted inputs: `bigint` (pass-through), safe-integer `number`, decimal
// integer `string`.  Non-integer numbers, unsafe integers (beyond
// ±2^53 - 1), Infinity, and NaN are rejected with `UniffiNumericError`.
// `fromI64` / `fromU64` are the return-path counterpart: they ensure the
// value is a `bigint`.  No narrowing to `number` ever happens.
// ---------------------------------------------------------------------------

export function toI64(value: number | bigint | string): bigint {
    if (typeof value === "bigint") return value;
    if (typeof value === "string") return BigInt(value);
    if (!Number.isInteger(value) || !Number.isSafeInteger(value)) {
        throw new UniffiError({
            errorName: "UniffiNumericError",
            message: `cannot losslessly convert ${value} to i64: must be a safe integer, bigint, or decimal string`,
        });
    }
    return BigInt(value);
}

export function toU64(value: number | bigint | string): bigint {
    const n = toI64(value);
    if (n < 0n) {
        throw new UniffiError({
            errorName: "UniffiNumericError",
            message: `negative value ${value} cannot be converted to u64`,
        });
    }
    return n;
}

export function fromI64(value: bigint | number): bigint {
    if (typeof value === "bigint") return value;
    return BigInt(value);
}

export function fromU64(value: bigint | number): bigint {
    return fromI64(value);
}

// ---------------------------------------------------------------------------
// Handle registry with finalizer safety net
// ---------------------------------------------------------------------------

export class HandleMap<T> {
    private next = 1;
    private readonly slots = new Map<number, T>();

    insert(value: T): number {
        const id = this.next++;
        this.slots.set(id, value);
        return id;
    }

    get(id: number): T | undefined {
        return this.slots.get(id);
    }

    remove(id: number): T | undefined {
        const v = this.slots.get(id);
        this.slots.delete(id);
        return v;
    }

    size(): number {
        return this.slots.size;
    }
}

/**
 * Wraps an opaque native handle. Calls `drop(handle)` exactly once, either
 * via explicit `dispose()` or via the FinalizationRegistry fallback.
 */
export class UniffiObjectHandle {
    private disposed = false;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    private handle: any;
    private readonly dropFn: ((handle: unknown) => void) | null;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    constructor(handle: any, dropFn: ((handle: unknown) => void) | null) {
        this.handle = handle;
        this.dropFn = dropFn;
        if (dropFn) {
            FINALIZERS.register(
                this,
                { handle, dropFn },
                this as unknown as object,
            );
        }
    }

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    get raw(): any {
        if (this.disposed) {
            throw new UniffiError({
                errorName: "UniffiUseAfterDispose",
                message: "uniffi object was used after dispose()",
            });
        }
        return this.handle;
    }

    dispose(): void {
        if (this.disposed) return;
        this.disposed = true;
        FINALIZERS.unregister(this as unknown as object);
        try {
            this.dropFn?.(this.handle);
        } finally {
            this.handle = null;
        }
    }
}

type FinalizerPayload = {
    handle: unknown;
    dropFn: (handle: unknown) => void;
};

const FINALIZERS = new FinalizationRegistry<FinalizerPayload>((payload) => {
    try {
        payload.dropFn(payload.handle);
    } catch {
        // Swallow finalizer errors — throwing from a finalizer crashes the
        // host in most runtimes, and there is no user stack to attach to.
    }
});

// ---------------------------------------------------------------------------
// Callback registry
// ---------------------------------------------------------------------------

const CALLBACKS = new HandleMap<object>();

export function registerCallback(cb: object): number {
    return CALLBACKS.insert(cb);
}

export function lookupCallback(id: number): object | undefined {
    return CALLBACKS.get(id);
}

export function releaseCallback(id: number): void {
    CALLBACKS.remove(id);
}
