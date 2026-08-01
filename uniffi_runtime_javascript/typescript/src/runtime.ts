// Shared TypeScript runtime for uniffi_bindgen_javascript.
//
// This file is copied verbatim into `common/runtime.ts` by the generator
// (via `include_str!`), so it must not depend on anything outside itself.
// It owns: the stable `UniffiError` class, the backend install hook, the
// sync/async call wrappers, a handle registry with a FinalizationRegistry
// safety net, minimal numeric normalisation, and the callback registry
// used to forward `Logger`-style foreign traits into the native backend.
//
// This is deliberately private to the generated runtime.  Flavor adapters
// may be distributed independently from the high-level API, so installation
// verifies their exact ABI rather than silently accepting an older backend.
const __UNIFFI_JS_ABI_VERSION = 2;

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
type UniffiBackend = any;

let __uniffiBackend: UniffiBackend = null;

export function __installBackend(backend: UniffiBackend): void {
    let actualVersion: unknown;
    try {
        actualVersion =
            backend !== null && backend !== undefined
                ? backend.__uniffiAbiVersion
                : undefined;
    } catch {
        actualVersion = undefined;
    }
    if (actualVersion !== __UNIFFI_JS_ABI_VERSION) {
        throw new UniffiError({
            errorName: "UniffiAbiMismatch",
            data: {
                expected: __UNIFFI_JS_ABI_VERSION,
                actual: actualVersion,
            },
            message: `incompatible UniFFI JavaScript backend ABI: expected ${__UNIFFI_JS_ABI_VERSION}, got ${String(actualVersion)}`,
        });
    }
    __uniffiBackend = backend;
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

/** The public, single-consumer output-stream API. */
export interface UniFfiStream<T> extends AsyncIterable<T> {
    next(): Promise<IteratorResult<T>>;
    cancel(): Promise<void>;
}

// This is intentionally binding-internal.  Output bridges must use a tagged
// union; neither nullable values nor IteratorResult-style `done` envelopes are
// accepted at the native boundary.
type RawStreamStep<T, E> =
    | { kind: "item"; value: T }
    | { kind: "done" }
    | { kind: "error"; error: E };

interface UniFfiStreamOptions<T, E> {
    start: () => unknown;
    next: (handle: unknown) => Promise<RawStreamStep<T, E>>;
    cancel: (handle: unknown) => void | Promise<void>;
}

type UniffiStreamState =
    | "idle"
    | "starting"
    | "active"
    | "done"
    | "failed"
    | "cancelled";

type UniffiStreamCleanupState = {
    cancelStarted: boolean;
};

type UniffiStreamFinalizerPayload = {
    handle: unknown;
    cancel: (handle: unknown) => void | Promise<void>;
    cleanup: UniffiStreamCleanupState;
};

// The payload has no reference to the stream or iterator.  A finalizer is a
// best-effort fallback only and must never retain the object it is meant to
// observe, throw synchronously, or leave a rejected cleanup Promise behind.
const STREAM_FINALIZERS = new FinalizationRegistry<UniffiStreamFinalizerPayload>(
    (payload) => {
        if (payload.cleanup.cancelStarted) return;
        payload.cleanup.cancelStarted = true;
        try {
            void Promise.resolve(payload.cancel(payload.handle)).catch(() => {});
        } catch {
            // Finalization cannot report failures to an application.
        }
    },
);

function streamProtocolError(message: string): UniffiError {
    return new UniffiError({
        errorName: "UniffiStreamProtocolError",
        message,
    });
}

function streamConsumedError(): UniffiError {
    return new UniffiError({
        errorName: "UniffiStreamConsumed",
        message: "a UniFFI output stream can only have one consumer",
    });
}

function streamConcurrentNextError(): UniffiError {
    return new UniffiError({
        errorName: "UniffiStreamConcurrentNext",
        message: "concurrent next() on a UniFFI output stream is not supported",
    });
}

function terminalStreamResult<T>(): IteratorResult<T> {
    return { done: true, value: undefined as T };
}

function hasOwn(object: object, key: string): boolean {
    return Object.prototype.hasOwnProperty.call(object, key);
}

function hasOnlyStreamKeys(object: object, allowed: readonly string[]): boolean {
    return Object.keys(object).every((key) => allowed.includes(key));
}

function validateRawStreamStep<T, E>(raw: unknown): RawStreamStep<T, E> {
    if (raw === null || typeof raw !== "object" || !hasOwn(raw, "kind")) {
        throw streamProtocolError(
            "uniffi stream next returned an invalid tagged step",
        );
    }
    const step = raw as Record<string, unknown>;
    switch (step.kind) {
        case "item":
            if (
                !hasOwn(step, "value") ||
                hasOwn(step, "error") ||
                !hasOnlyStreamKeys(step, ["kind", "value"])
            ) {
                throw streamProtocolError(
                    "uniffi stream Item step must contain only kind and value",
                );
            }
            return { kind: "item", value: step.value as T };
        case "done":
            if (!hasOnlyStreamKeys(step, ["kind"])) {
                throw streamProtocolError(
                    "uniffi stream Done step must contain only kind",
                );
            }
            return { kind: "done" };
        case "error":
            if (
                !hasOwn(step, "error") ||
                hasOwn(step, "value") ||
                !hasOnlyStreamKeys(step, ["kind", "error"])
            ) {
                throw streamProtocolError(
                    "uniffi stream Error step must contain only kind and error",
                );
            }
            return { kind: "error", error: step.error as E };
        default:
            throw streamProtocolError(
                "uniffi stream next returned an unknown tagged step kind",
            );
    }
}

/**
 * Binding-internal factory for a lazy, pull-based output stream.  `start` is
 * deliberately a closure: constructing the public stream never allocates a
 * native handle, and only the first consumer pull starts one.
 */
export function createUniFfiStream<T, E>(
    options: UniFfiStreamOptions<T, E>,
): UniFfiStream<T> {
    let state: UniffiStreamState = "idle";
    let handle: unknown;
    let pending = false;
    let consumer: "direct" | "iterator" | null = null;
    let finalizerRegistered = false;
    const cleanup: UniffiStreamCleanupState = { cancelStarted: false };
    const finalizerToken = {};

    // Native callbacks can synchronously re-enter this stream (and async
    // callbacks can change it while a pull is awaiting). Reading through a
    // function deliberately preserves the full state union for TypeScript's
    // control-flow analysis instead of treating a transition as immutable.
    const currentState = (): UniffiStreamState => state;

    const unregisterFinalizer = (): void => {
        if (!finalizerRegistered) return;
        STREAM_FINALIZERS.unregister(finalizerToken);
        finalizerRegistered = false;
    };

    const registerFinalizer = (owner: object): void => {
        if (finalizerRegistered || state !== "active") return;
        STREAM_FINALIZERS.register(
            owner,
            { handle, cancel: options.cancel, cleanup },
            finalizerToken,
        );
        finalizerRegistered = true;
    };

    const finishWithoutCancel = (terminal: "done" | "failed"): void => {
        state = terminal;
        handle = undefined;
        unregisterFinalizer();
    };

    const finishActiveWithCancel = async (
        terminal: "failed" | "cancelled",
    ): Promise<void> => {
        if (state !== "active") return;
        const activeHandle = handle;
        state = terminal;
        handle = undefined;
        unregisterFinalizer();
        if (cleanup.cancelStarted) return;
        cleanup.cancelStarted = true;
        await options.cancel(activeHandle);
    };

    const failActive = async (raw: unknown): Promise<never> => {
        const error = wrapError(raw);
        if (state === "active") {
            try {
                await finishActiveWithCancel("failed");
            } catch {
                // Preserve the original transport/protocol/business error.
            }
        } else if (state === "starting" || state === "idle") {
            finishWithoutCancel("failed");
        }
        throw error;
    };

    const ensureActive = async (owner: object): Promise<unknown | null> => {
        if (state === "active") return handle;
        if (state !== "idle") return null;

        // Publish the starting state before invoking user/native code.  A
        // re-entrant next() therefore observes a pending pull instead of
        // running start twice.
        state = "starting";
        let startedHandle: unknown;
        try {
            startedHandle = options.start();
        } catch (raw) {
            if (currentState() !== "cancelled") finishWithoutCancel("failed");
            throw wrapError(raw);
        }

        // A synchronous/re-entrant cancel may have closed the stream while
        // start was executing.  The handle now exists, so clean it up once,
        // but do not register a finalizer for this already-terminal stream.
        if (currentState() === "cancelled") {
            handle = startedHandle;
            if (!cleanup.cancelStarted) {
                cleanup.cancelStarted = true;
                try {
                    await options.cancel(startedHandle);
                } catch {
                    // The initiating cancellation was necessarily best-effort
                    // here because it happened before a handle existed.
                }
            }
            handle = undefined;
            return null;
        }

        handle = startedHandle;
        state = "active";
        registerFinalizer(owner);
        return startedHandle;
    };

    const pull = async (owner: object): Promise<IteratorResult<T>> => {
        if (
            state === "done" ||
            state === "failed" ||
            state === "cancelled"
        ) {
            return terminalStreamResult<T>();
        }
        if (pending) throw streamConcurrentNextError();
        pending = true;
        try {
            const activeHandle = await ensureActive(owner);
            if (activeHandle === null || currentState() !== "active") {
                return terminalStreamResult<T>();
            }
            const raw = await options.next(activeHandle);

            // return()/throw()/cancel() may have won while the native pull was
            // pending.  A late item/error must never revive a terminal stream.
            if (currentState() !== "active") return terminalStreamResult<T>();

            const step = validateRawStreamStep<T, E>(raw);
            if (step.kind === "item") {
                return { done: false, value: step.value };
            }
            if (step.kind === "done") {
                finishWithoutCancel("done");
                return terminalStreamResult<T>();
            }
            return await failActive(step.error);
        } catch (raw) {
            const terminalState = currentState();
            if (terminalState === "cancelled" || terminalState === "done") {
                return terminalStreamResult<T>();
            }
            // Error steps are deliberately routed through `failActive` above.
            // It throws the original (possibly typed) error after cleanup, so
            // do not run the terminal transition a second time here.
            if (terminalState === "failed") {
                throw wrapError(raw);
            }
            return await failActive(raw);
        } finally {
            pending = false;
        }
    };

    const stream: UniFfiStream<T> = {
        next(): Promise<IteratorResult<T>> {
            if (consumer === null) consumer = "direct";
            if (consumer !== "direct") {
                return Promise.reject(streamConsumedError());
            }
            return pull(stream);
        },
        async cancel(): Promise<void> {
            if (state === "idle" || state === "starting") {
                // There is no active native handle in idle/starting state.
                // Starting is only observable through synchronous re-entry;
                // ensureActive performs the deferred cleanup if start wins.
                state = "cancelled";
                return;
            }
            if (state !== "active") return;
            await finishActiveWithCancel("cancelled");
        },
        [Symbol.asyncIterator](): AsyncIterator<T> {
            if (consumer !== null) throw streamConsumedError();
            let iterator: AsyncIterator<T> & AsyncIterable<T>;
            iterator = {
                next: (): Promise<IteratorResult<T>> => pull(iterator),
                async return(): Promise<IteratorResult<T>> {
                    try {
                        await stream.cancel();
                    } catch {
                        // Iterator return is best-effort cleanup.  `break`
                        // must still complete with the standard terminal value.
                    }
                    return terminalStreamResult<T>();
                },
                async throw(error?: unknown): Promise<IteratorResult<T>> {
                    try {
                        await stream.cancel();
                    } catch {
                        // The caller's error remains authoritative.
                    }
                    throw error;
                },
                [Symbol.asyncIterator](): AsyncIterator<T> {
                    return iterator;
                },
            };
            consumer = "iterator";
            return iterator;
        },
    };
    return stream;
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
