// SPDX-License-Identifier: Apache-2.0
//! The `WebTransportServer` — accepts WebTransport sessions from clients. Each
//! established session is a {@link WebTransportServerSession}, which shares the
//! full stream/datagram surface with the client `WebTransport`.

import { WebTransportError } from './errors.js';
import { loadNative } from './loader.js';
import {
    SessionCore,
    ServerTransport,
    type NativeAddon,
    type NativeServerHandle,
    type ServerNativeEvent,
} from './native.js';
import { WebTransportSession } from './webtransport.js';

interface Deferred<T> {
    promise: Promise<T>;
    resolve: (value: T) => void;
    reject: (reason: unknown) => void;
}

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
    /** Interface to bind (default `0.0.0.0`). */
    host?: string;
    /** Path to the PEM certificate chain file. */
    cert: string;
    /** Path to the PEM private-key file. */
    key: string;
    /**
     * Bind with `SO_REUSEPORT` (Unix only) so multiple processes, for example
     * Node `cluster` workers, can share one listening port and let the kernel
     * load-balance connections across them. Defaults to `false`. Ignored on
     * Windows, where the flag does not exist.
     */
    reusePort?: boolean;
}

/** Metadata about the Extended CONNECT that opened a server session. */
export interface WebTransportSessionRequest {
    authority: string;
    path: string;
    origin: string | null;
    headers: Record<string, string>;
}

/**
 * A server-side WebTransport session. Symmetric with the client `WebTransport`
 * (streams + datagrams), plus the request metadata from the client's CONNECT.
 */
export class WebTransportServerSession extends WebTransportSession {
    /** The `:authority` of the client's CONNECT request. */
    public readonly authority: string;
    /** The `:path` of the client's CONNECT request. */
    public readonly path: string;
    /** The `origin` header, if the client sent one. */
    public readonly origin: string | null;
    /** Any additional (non-pseudo) request headers. */
    public readonly headers: Record<string, string>;

    public constructor(core: SessionCore, request: WebTransportSessionRequest) {
        super(core);
        this.authority = request.authority;
        this.path = request.path;
        this.origin = request.origin;
        this.headers = request.headers;
    }
}

/**
 * A WebTransport server. Bind a port with a certificate, then consume
 * {@link incomingSessions}.
 *
 * ```ts
 * const server = new WebTransportServer({ port: 4433, cert, key });
 * await server.ready;
 * for await (const session of readable(server.incomingSessions)) {
 *   // handle the session
 * }
 * ```
 */
export class WebTransportServer {
    private readonly native: NativeAddon;
    private readonly handle: NativeServerHandle;
    private readonly sessions = new Map<number, SessionCore>();
    private readonly readyD = deferred<void>();
    private readonly closedD = deferred<void>();
    private incomingController!: ReadableStreamDefaultController<WebTransportServerSession>;
    private boundPort = 0;
    private closeCalled = false;

    /** The sessions clients open, as they are established. */
    public readonly incomingSessions: ReadableStream<WebTransportServerSession>;

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

    /** Resolves once the server is listening. */
    public get ready(): Promise<void> {
        return this.readyD.promise;
    }

    /** Resolves when the server has stopped, rejects on a fatal server error. */
    public get closed(): Promise<void> {
        return this.closedD.promise;
    }

    /** The UDP port the server is bound to (valid after `ready`). */
    public get port(): number {
        return this.boundPort;
    }

    /** Stop the server and all its sessions. */
    public close(): void {
        if (this.closeCalled) return;
        this.closeCalled = true;
        this.native.serverShutdown(this.handle);
    }

    private onEvent(ev: ServerNativeEvent): void {
        switch (ev.type) {
            case 'listening': {
                this.boundPort = ev.port;
                this.readyD.resolve();
                return;
            }
            case 'serverError': {
                const err = new WebTransportError(ev.message, { source: 'session' });
                this.readyD.reject(err);
                this.closedD.reject(err);
                this.finishAllSessions();
                safeCloseController(this.incomingController);
                return;
            }
            case 'serverClosed': {
                this.closedD.resolve();
                this.finishAllSessions();
                safeCloseController(this.incomingController);
                return;
            }
        }

        // Session-scoped event.
        const sessionId = ev.session;
        if (ev.type === 'serverReady') {
            const core = new SessionCore();
            core.attach(new ServerTransport(this.native, this.handle, sessionId));
            this.sessions.set(sessionId, core);
            void core.closed.promise.finally(() => this.sessions.delete(sessionId));
            const session = new WebTransportServerSession(core, {
                authority: ev.authority,
                path: ev.path,
                origin: ev.origin,
                headers: ev.headers,
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
     */
    private finishAllSessions(): void {
        for (const core of this.sessions.values()) {
            core.dispatch({ type: 'closed', code: 0, reason: new Uint8Array(), remote: false });
        }
        this.sessions.clear();
    }
}

function safeCloseController(controller: { close(): void }): void {
    try {
        controller.close();
    } catch {
        // already closed
    }
}
