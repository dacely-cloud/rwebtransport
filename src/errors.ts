// SPDX-License-Identifier: Apache-2.0
//! The `WebTransportError` type, matching the W3C interface.

/**
 * Identifies which part of the transport a {@link WebTransportError} came from.
 *
 * @remarks
 * Mirrors the `source` attribute of the W3C `WebTransportError` interface.
 * `'stream'` means the failure was raised by an individual send or receive
 * stream (for example a stream reset), while `'session'` means it applies to
 * the whole WebTransport session (for example the connection closing).
 *
 * @see {@link WebTransportError.source}
 */
export type WebTransportErrorSource = 'stream' | 'session';

/**
 * Optional settings passed to the {@link WebTransportError} constructor.
 *
 * @remarks
 * Both fields are optional; the constructor applies defaults when they are
 * omitted. Corresponds to the `WebTransportErrorOptions` dictionary in the
 * W3C WebTransport specification.
 */
export interface WebTransportErrorOptions {
    /**
     * Which part of the transport the error originated from.
     *
     * @defaultValue `'session'` (applied by the constructor when omitted)
     * @see {@link WebTransportErrorSource}
     */
    source?: WebTransportErrorSource;
    /**
     * The application-supplied error code carried by a stream reset, or `null`
     * when the error is not associated with a specific stream error code.
     *
     * @defaultValue `null` (applied by the constructor when omitted)
     */
    streamErrorCode?: number | null;
}

/**
 * Error thrown / rejected for WebTransport failures, mirroring the browser
 * `WebTransportError`.
 *
 * @remarks
 * Extends the built-in {@link Error}. Instances are surfaced when a session or
 * stream fails, for example as the rejection reason of a pending promise or as
 * the error a {@link ReadableStream} / {@link WritableStream} is aborted with.
 * The constructor re-applies the prototype via {@link Object.setPrototypeOf}
 * so that `instanceof WebTransportError` works even when the class is
 * transpiled down to an ES5-style constructor function.
 *
 * @example
 * ```ts
 * throw new WebTransportError('stream reset', {
 *     source: 'stream',
 *     streamErrorCode: 42,
 * });
 * ```
 *
 * @see {@link WebTransportErrorOptions}
 */
export class WebTransportError extends Error {
    /** Whether the error originated from a stream or the session as a whole. */
    public readonly source: WebTransportErrorSource;
    /** The application error code, if the error came from a stream reset. */
    public readonly streamErrorCode: number | null;

    /**
     * Creates a new {@link WebTransportError}.
     *
     * @param message - Human-readable description of the failure. When omitted,
     * the message defaults to the string `'WebTransport error'`.
     * @param options - Optional {@link WebTransportErrorOptions}; defaults to an
     * empty object so that both fields fall back to their own defaults.
     *
     * @remarks
     * Sets {@link Error.name} to `'WebTransportError'`, resolves
     * {@link WebTransportError.source} (defaulting to `'session'`) and
     * {@link WebTransportError.streamErrorCode} (defaulting to `null`), then
     * restores the prototype chain with {@link Object.setPrototypeOf} so
     * `instanceof` checks succeed under downlevel compilation.
     */
    public constructor(message?: string, options: WebTransportErrorOptions = {}) {
        super(message ?? 'WebTransport error');
        this.name = 'WebTransportError';
        this.source = options.source ?? 'session';
        this.streamErrorCode = options.streamErrorCode ?? null;
        Object.setPrototypeOf(this, WebTransportError.prototype);
    }
}
