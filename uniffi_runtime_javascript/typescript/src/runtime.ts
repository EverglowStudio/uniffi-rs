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
