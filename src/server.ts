// SPDX-License-Identifier: Apache-2.0
//! The `WebTransportServer` accepts WebTransport sessions from clients. Each
//! established session is a {@link WebTransportServerSession}, which shares the
//! full stream/datagram surface with the client `WebTransport`.

import { WebTransportError } from './errors.js';
import { loadNative } from './loader.js';
import {
    NativeEventType,
    SessionCore,
    ServerTransport,
    type NativeAddon,
    type NativeServerHandle,
    type ServerNativeEvent,
} from './native.js';
import { WebTransportSession } from './webtransport.js';

/**
 * A promise paired with its externally callable `resolve` and `reject`, so a
 * value can be settled from outside the executor (for example from a native
 * event callback).
 *
 * @typeParam T - The type the promise resolves to.
 * @internal Private helper, not part of the published package API.
 */
interface Deferred<T> {
    /** The promise controlled by this deferred. */
    promise: Promise<T>;
    /** Fulfills {@link Deferred.promise} with the given value. */
    resolve: (value: T) => void;
    /** Rejects {@link Deferred.promise} with the given reason. */
    reject: (reason: unknown) => void;
}

/**
 * Create a {@link Deferred}: a fresh promise together with its `resolve` and
 * `reject` functions captured from the executor.
 *
 * @typeParam T - The type the promise resolves to.
 * @returns A deferred whose `resolve`/`reject` settle its `promise`.
 * @internal Private helper, not part of the published package API.
 */
function deferred<T>(): Deferred<T> {
    let resolve!: (value: T) => void;
    let reject!: (reason: unknown) => void;
    const promise = new Promise<T>((res, rej) => {
        resolve = res;
        reject = rej;
    });
    return { promise, resolve, reject };
}

/** Options for {@link WebTransportServer}. */
export interface WebTransportServerOptions {
    /** UDP port to listen on. Use `0` to let the OS pick (read it back via `port`). */
    port: number;
    /**
     * Interface (IP address) to bind.
     *
     * @defaultValue `'0.0.0.0'` (all IPv4 interfaces).
     */
    host?: string;
    /** Path to the PEM certificate chain file. */
    cert: string;
    /** Path to the PEM private-key file. */
    key: string;
    /**
     * Bind with `SO_REUSEPORT` (Unix only) so multiple processes, for example
     * Node `cluster` workers, can share one listening port and let the kernel
     * load-balance connections across them. Ignored on Windows, where the flag
     * does not exist.
     *
     * @defaultValue `false`
     */
    reusePort?: boolean;
}

/**
 * Metadata about the Extended CONNECT that opened a server session, extracted
 * from the client's HTTP/3 CONNECT request pseudo-headers and headers.
 */
export interface WebTransportSessionRequest {
    /** The `:authority` pseudo-header of the client's CONNECT request. */
    authority: string;
    /** The `:path` pseudo-header of the client's CONNECT request. */
    path: string;
    /** The `origin` header value, or `null` if the client did not send one. */
    origin: string | null;
    /** Any additional (non-pseudo) request headers, keyed by header name. */
    headers: Record<string, string>;
    /** The client's remote IP address at session establishment. */
    remoteAddress: string;
    /** The client's remote UDP port at session establishment. */
    remotePort: number;
}

/**
 * A server-side WebTransport session. Symmetric with the client `WebTransport`
 * (streams + datagrams inherited from {@link WebTransportSession}), plus the
 * request metadata carried on the client's CONNECT.
 */
export class WebTransportServerSession extends WebTransportSession {
    /** The `:authority` of the client's CONNECT request. */
    public readonly authority: string;
    /** The `:path` of the client's CONNECT request. */
    public readonly path: string;
    /** The `origin` header, if the client sent one, otherwise `null`. */
    public readonly origin: string | null;
    /** Any additional (non-pseudo) request headers, keyed by header name. */
    public readonly headers: Record<string, string>;
    /**
     * The client's remote IP address, as reported by the transport when the
     * session was established. The same for every stream and datagram on this
     * session, since they all share the one QUIC connection.
     */
    public readonly remoteAddress: string;
    /** The client's remote UDP port at session establishment. */
    public readonly remotePort: number;

    /**
     * Wrap an established session core together with the CONNECT metadata that
     * opened it. Called by {@link WebTransportServer} when a new session becomes
     * ready; not intended for direct construction by users.
     *
     * @param core - The dispatcher/command core backing this session, already
     *   attached to a {@link ServerTransport}.
     * @param request - The CONNECT metadata (authority, path, origin, headers)
     *   plus the client's remote address, copied onto this session's public fields.
     */
    public constructor(core: SessionCore, request: WebTransportSessionRequest) {
        super(core);
        this.authority = request.authority;
        this.path = request.path;
        this.origin = request.origin;
        this.headers = request.headers;
        this.remoteAddress = request.remoteAddress;
        this.remotePort = request.remotePort;
    }
}

/**
 * A WebTransport server. Bind a port with a certificate, then consume
 * {@link incomingSessions}.
 *
 * @example
 * ```ts
 * const server = new WebTransportServer({ port: 4433, cert, key });
 * await server.ready;
 * for await (const session of readable(server.incomingSessions)) {
 *   // handle the session
 * }
 * ```
 *
 * @see {@link WebTransportServerSession} for the per-session surface.
 */
export class WebTransportServer {
    /** The loaded native addon, source of the `server*` command functions. */
    private readonly native: NativeAddon;
    /** Opaque native server handle returned by `serverListen`. */
    private readonly handle: NativeServerHandle;
    /** Live sessions keyed by their native session id, for event routing. */
    private readonly sessions = new Map<number, SessionCore>();
    /** Settled when the native side reports `listening`; backs {@link ready}. */
    private readonly readyD = deferred<void>();
    /**
     * Settled when the server stops (`serverClosed`) or fatally fails
     * (`serverError`); backs {@link closed}.
     */
    private readonly closedD = deferred<void>();
    /** Controller for {@link incomingSessions}; captured in its `start`. */
    private incomingController!: ReadableStreamDefaultController<WebTransportServerSession>;
    /** The UDP port actually bound, learned from the `listening` event. */
    private boundPort = 0;
    /** Guards {@link close} so shutdown is issued at most once. */
    private closeCalled = false;

    /**
     * The sessions clients open, delivered as they are established. Each pull
     * yields a {@link WebTransportServerSession}. The stream is closed when the
     * server stops or errors.
     */
    public readonly incomingSessions: ReadableStream<WebTransportServerSession>;

    /**
     * Load the native addon and start listening immediately.
     *
     * Sets up {@link incomingSessions} (capturing its controller), attaches
     * no-op catch handlers to the internal ready/closed promises so they never
     * surface as unhandled rejections, then calls the native `serverListen` with
     * the given certificate, key, host, port and reuse-port flag, wiring every
     * native event to {@link onEvent}.
     *
     * @param options - Bind address, port, certificate/key paths and reuse-port
     *   flag. `host` defaults to `'0.0.0.0'` and `reusePort` defaults to `false`.
     * @see {@link ready} to await binding and {@link WebTransportServerOptions}.
     */
    public constructor(options: WebTransportServerOptions) {
        this.native = loadNative();
        void this.readyD.promise.catch(() => {});
        void this.closedD.promise.catch(() => {});

        this.incomingSessions = new ReadableStream<WebTransportServerSession>({
            start: (c) => {
                this.incomingController = c;
            },
        });

        this.handle = this.native.serverListen(
            options.cert,
            options.key,
            options.host ?? '0.0.0.0',
            options.port,
            options.reusePort ?? false,
            (ev) => this.onEvent(ev),
        );
    }

    /**
     * Resolves once the server is listening (the native `listening` event has
     * arrived); rejects if the server fails before it binds.
     */
    public get ready(): Promise<void> {
        return this.readyD.promise;
    }

    /**
     * Resolves when the server has stopped cleanly (`serverClosed`), rejects on
     * a fatal server error (`serverError`).
     */
    public get closed(): Promise<void> {
        return this.closedD.promise;
    }

    /** The UDP port the server is bound to (valid after {@link ready}). */
    public get port(): number {
        return this.boundPort;
    }

    /**
     * Stop the server and all its sessions. Idempotent: repeated calls after the
     * first are ignored. Triggers the native `serverShutdown`, which eventually
     * drives a `serverClosed` event that settles {@link closed} and finishes any
     * remaining sessions.
     */
    public close(): void {
        if (this.closeCalled) return;
        this.closeCalled = true;
        this.native.serverShutdown(this.handle);
    }

    /**
     * Handle one native server event. Server-level events (`listening`,
     * `serverError`, `serverClosed`) settle the server's ready/closed promises
     * and, on stop or error, finish all sessions and close
     * {@link incomingSessions}. Session-scoped events carry a `session` id: a
     * `serverReady` creates and registers a new {@link SessionCore} (attached to
     * a {@link ServerTransport}), wraps it in a {@link WebTransportServerSession}
     * and enqueues it; every session-scoped event is then dispatched to the
     * matching core.
     *
     * @param ev - The event delivered by the native `serverListen` callback.
     * @remarks Enqueue into {@link incomingSessions} is guarded: if the consumer
     *   has stopped reading, the thrown error is swallowed so it cannot escape
     *   the native event dispatch. On `serverError`, the `WebTransportError` is
     *   created with `source: 'session'`.
     */
    private onEvent(ev: ServerNativeEvent): void {
        switch (ev.type) {
            case NativeEventType.Listening: {
                this.boundPort = ev.port;
                this.readyD.resolve();
                return;
            }
            case NativeEventType.ServerError: {
                const err = new WebTransportError(ev.message, { source: 'session' });
                this.readyD.reject(err);
                this.closedD.reject(err);
                this.finishAllSessions();
                safeCloseController(this.incomingController);
                return;
            }
            case NativeEventType.ServerClosed: {
                this.closedD.resolve();
                this.finishAllSessions();
                safeCloseController(this.incomingController);
                return;
            }
        }

        // Session-scoped event.
        const sessionId = ev.session;
        if (ev.type === NativeEventType.ServerReady) {
            const core = new SessionCore();
            core.attach(new ServerTransport(this.native, this.handle, sessionId));
            this.sessions.set(sessionId, core);
            void core.closed.promise.finally(() => this.sessions.delete(sessionId));
            const session = new WebTransportServerSession(core, {
                authority: ev.authority,
                path: ev.path,
                origin: ev.origin,
                headers: ev.headers,
                remoteAddress: ev.remoteAddress,
                remotePort: ev.remotePort,
            });
            try {
                this.incomingController.enqueue(session);
            } catch {
                // consumer no longer reading incoming sessions
            }
        }

        this.sessions.get(sessionId)?.dispatch(ev);
    }

    /**
     * Settle every live session when the server stops, so their `ready`/`closed`
     * promises never hang and the session map does not leak.
     *
     * @remarks Dispatches a synthetic clean `closed` event (code `0`, empty
     *   reason, `remote: false`) to each session core, then clears the map. The
     *   per-core `closed.finally` handler registered in {@link onEvent} would
     *   also remove entries, but the map is cleared here unconditionally.
     */
    private finishAllSessions(): void {
        for (const core of this.sessions.values()) {
            core.dispatch({
                type: NativeEventType.Closed,
                code: 0,
                reason: new Uint8Array(),
                remote: false,
            });
        }
        this.sessions.clear();
    }
}

/**
 * Close a ReadableStream controller, ignoring the error thrown if it is already
 * closed. Used when the server stops or errors so shutting the incoming-session
 * stream is safe regardless of its current state.
 *
 * @param controller - Any object exposing a `close()` method (the
 *   {@link WebTransportServer.incomingSessions} controller).
 * @internal Private helper, not part of the published package API.
 */
function safeCloseController(controller: { close(): void }): void {
    try {
        controller.close();
    } catch {
        // already closed
    }
}
