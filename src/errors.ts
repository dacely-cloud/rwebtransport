// SPDX-License-Identifier: Apache-2.0
//! The `WebTransportError` type, matching the W3C interface.

export type WebTransportErrorSource = 'stream' | 'session';

export interface WebTransportErrorOptions {
    source?: WebTransportErrorSource;
    streamErrorCode?: number | null;
}

/**
 * Error thrown / rejected for WebTransport failures, mirroring the browser
 * `WebTransportError`.
 */
export class WebTransportError extends Error {
    /** Whether the error originated from a stream or the session as a whole. */
    readonly source: WebTransportErrorSource;
    /** The application error code, if the error came from a stream reset. */
    readonly streamErrorCode: number | null;

    constructor(message?: string, options: WebTransportErrorOptions = {}) {
        super(message ?? 'WebTransport error');
        this.name = 'WebTransportError';
        this.source = options.source ?? 'session';
        this.streamErrorCode = options.streamErrorCode ?? null;
        Object.setPrototypeOf(this, WebTransportError.prototype);
    }
}
