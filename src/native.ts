// SPDX-License-Identifier: Apache-2.0
//! Typed view of the native addon and the event dispatcher that turns its
//! low-level, callback-based ABI into promises and per-stream sinks.

import { loadNative } from './loader.js';
import { WebTransportError } from './errors.js';
import type { WebTransportCloseInfo } from './types.js';

/** Opaque handle to a native client session (a neon `JsBox`). */
export type NativeHandle = { readonly __brand: 'rwt-session' };

/** Opaque handle to a native server (a neon `JsBox`). */
export type NativeServerHandle = { readonly __brand: 'rwt-server' };

/** Events delivered by the native addon to the `onEvent` callback. */
export type NativeEvent =
    | { type: 'ready' }
    | {
          type: 'serverReady';
          authority: string;
          path: string;
          origin: string | null;
          headers: Record<string, string>;
      }
    | { type: 'closed'; code: number; reason: Uint8Array; remote: boolean }
    | { type: 'error'; message: string }
    | { type: 'datagram'; data: Uint8Array }
    | { type: 'stream'; streamId: number; bidi: boolean }
    | { type: 'streamData'; streamId: number; data: Uint8Array }
    | { type: 'streamFin'; streamId: number }
    | { type: 'streamReset'; streamId: number; code: number }
    | { type: 'streamStopSending'; streamId: number; code: number }
    | { type: 'streamOpened'; requestId: number; streamId: number }
    | { type: 'writeAck'; requestId: number }
    | { type: 'datagramAck'; requestId: number; sent: boolean };

/** The raw function surface exported by `rwebtransport.node`. */
export interface NativeAddon {
    connect(
        url: string,
        hashes: Uint8Array[],
        insecure: boolean,
        origin: string | null,
        headerNames: string[],
        headerValues: string[],
        onEvent: (ev: NativeEvent) => void,
    ): NativeHandle;
    openStream(handle: NativeHandle, bidi: boolean, requestId: number): void;
    writeStream(handle: NativeHandle, streamId: number, bytes: Uint8Array, requestId: number): void;
    finStream(handle: NativeHandle, streamId: number): void;
    resetStream(handle: NativeHandle, streamId: number, code: number): void;
    stopSending(handle: NativeHandle, streamId: number, code: number): void;
    setPaused(handle: NativeHandle, streamId: number, paused: boolean): void;
    sendDatagram(handle: NativeHandle, bytes: Uint8Array, requestId: number): void;
    maxDatagramSize(handle: NativeHandle): number;
    isReady(handle: NativeHandle): boolean;
    isClosed(handle: NativeHandle): boolean;
    closeSession(handle: NativeHandle, code: number, reason: Uint8Array): void;
    shutdown(handle: NativeHandle): void;

    // ---- server ----
    serverListen(
        certPath: string,
        keyPath: string,
        host: string,
        port: number,
        onEvent: (ev: ServerNativeEvent) => void,
    ): NativeServerHandle;
    serverOpenStream(
        handle: NativeServerHandle,
        session: number,
        bidi: boolean,
        requestId: number,
    ): void;
    serverWrite(
        handle: NativeServerHandle,
        session: number,
        streamId: number,
        bytes: Uint8Array,
        requestId: number,
    ): void;
    serverFin(handle: NativeServerHandle, session: number, streamId: number): void;
    serverReset(handle: NativeServerHandle, session: number, streamId: number, code: number): void;
    serverStopSending(
        handle: NativeServerHandle,
        session: number,
        streamId: number,
        code: number,
    ): void;
    serverSetPaused(
        handle: NativeServerHandle,
        session: number,
        streamId: number,
        paused: boolean,
    ): void;
    serverSendDatagram(
        handle: NativeServerHandle,
        session: number,
        bytes: Uint8Array,
        requestId: number,
    ): void;
    serverMaxDatagramSize(handle: NativeServerHandle): number;
    serverCloseSession(
        handle: NativeServerHandle,
        session: number,
        code: number,
        reason: Uint8Array,
    ): void;
    serverShutdown(handle: NativeServerHandle): void;
}

/** Server-level events, plus session-scoped events tagged with a `session` id. */
export type ServerNativeEvent =
    | { type: 'listening'; port: number }
    | { type: 'serverError'; message: string }
    | { type: 'serverClosed' }
    | (NativeEvent & { session: number });

/** Normalized inputs for opening a session. */
export interface ConnectConfig {
    url: string;
    hashes: Uint8Array[];
    insecure: boolean;
    origin: string | null;
    headerNames: string[];
    headerValues: string[];
}

/** Consumer of a receive stream's inbound events. */
export interface ReceiveSink {
    onData(chunk: Uint8Array): void;
    onFin(): void;
    onReset(code: number): void;
}

/** Consumer of a send stream's control events. */
export interface SendSink {
    onStopSending(code: number): void;
}

/** Consumer of peer-initiated streams. */
export interface IncomingHandler {
    onBidi(streamId: number): void;
    onUni(streamId: number): void;
}

/** Consumer of inbound datagrams. */
export type DatagramSink = (data: Uint8Array) => void;

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

/**
 * The low-level per-session command surface. Implemented by a client transport
 * (native session handle) or a server transport (server handle + session id), so
 * the same {@link SessionCore} drives both roles.
 */
export interface SessionTransport {
    openStream(bidi: boolean, requestId: number): void;
    write(streamId: number, bytes: Uint8Array, requestId: number): void;
    fin(streamId: number): void;
    reset(streamId: number, code: number): void;
    stopSending(streamId: number, code: number): void;
    setPaused(streamId: number, paused: boolean): void;
    sendDatagram(bytes: Uint8Array, requestId: number): void;
    maxDatagramSize(): number;
    closeSession(code: number, reason: Uint8Array): void;
    shutdown(): void;
}

/**
 * Dispatches native events to promises / sinks and issues commands through an
 * injected {@link SessionTransport}. Exactly one `SessionCore` backs one
 * WebTransport session (client or server side).
 */
export class SessionCore {
    private transport: SessionTransport | undefined;

    private nextRequestId = 1;
    private readonly opens = new Map<number, Deferred<number>>();
    private readonly writes = new Map<number, Deferred<void>>();
    private readonly datagramAcks = new Map<number, Deferred<boolean>>();
    private readonly receives = new Map<number, ReceiveSink>();
    private readonly sends = new Map<number, SendSink>();

    private incoming: IncomingHandler | undefined;
    private datagramSink: DatagramSink | undefined;

    private closedState = false;
    private readyState = false;

    public readonly ready = deferred<void>();
    public readonly closed = deferred<WebTransportCloseInfo>();

    public constructor() {
        // Swallow default unhandled-rejection warnings; the user observes these.
        void this.closed.promise.catch(() => {});
        void this.ready.promise.catch(() => {});
    }

    /** Attach the transport once the underlying native handle exists. */
    public attach(transport: SessionTransport): void {
        this.transport = transport;
    }

    /** Fail a session that could not be set up at all (e.g. client connect threw). */
    public failSetup(message: string): void {
        // Defer so the caller can attach `.ready`/`.closed` handlers first.
        queueMicrotask(() =>
            this.finish(null, new WebTransportError(message, { source: 'session' })),
        );
    }

    private nextId(): number {
        return this.nextRequestId++;
    }

    // ---- outbound commands ----------------------------------------------------

    public openStream(bidi: boolean): Promise<number> {
        if (!this.usable()) return Promise.reject(this.deadError());
        const requestId = this.nextId();
        const d = deferred<number>();
        this.opens.set(requestId, d);
        this.transport!.openStream(bidi, requestId);
        return d.promise;
    }

    public write(streamId: number, chunk: Uint8Array): Promise<void> {
        if (!this.usable()) return Promise.reject(this.deadError());
        const requestId = this.nextId();
        const d = deferred<void>();
        this.writes.set(requestId, d);
        this.transport!.write(streamId, chunk, requestId);
        return d.promise;
    }

    public finStream(streamId: number): void {
        this.transport?.fin(streamId);
    }

    public resetStream(streamId: number, code: number): void {
        this.transport?.reset(streamId, code >>> 0);
    }

    public stopSending(streamId: number, code: number): void {
        this.transport?.stopSending(streamId, code >>> 0);
    }

    public setPaused(streamId: number, paused: boolean): void {
        this.transport?.setPaused(streamId, paused);
    }

    public sendDatagram(chunk: Uint8Array): Promise<boolean> {
        if (!this.usable()) return Promise.resolve(false);
        const requestId = this.nextId();
        const d = deferred<boolean>();
        this.datagramAcks.set(requestId, d);
        this.transport!.sendDatagram(chunk, requestId);
        return d.promise;
    }

    public maxDatagramSize(): number {
        return this.transport ? this.transport.maxDatagramSize() : 0;
    }

    public close(code: number, reason: string): void {
        if (this.closedState || !this.transport) return;
        this.transport.closeSession(code >>> 0, new TextEncoder().encode(reason));
    }

    public shutdown(): void {
        this.transport?.shutdown();
    }

    /** Whether outbound operations can still be issued (attached, not closed). */
    private usable(): boolean {
        return this.transport !== undefined && !this.closedState;
    }

    private deadError(): WebTransportError {
        return new WebTransportError('session is closed', { source: 'session' });
    }

    // ---- registration ---------------------------------------------------------

    public registerReceive(streamId: number, sink: ReceiveSink): void {
        this.receives.set(streamId, sink);
    }
    public unregisterReceive(streamId: number): void {
        this.receives.delete(streamId);
    }
    public registerSend(streamId: number, sink: SendSink): void {
        this.sends.set(streamId, sink);
    }
    public unregisterSend(streamId: number): void {
        this.sends.delete(streamId);
    }
    public setIncomingHandler(handler: IncomingHandler): void {
        this.incoming = handler;
    }
    public setDatagramSink(sink: DatagramSink): void {
        this.datagramSink = sink;
    }

    // ---- event dispatch -------------------------------------------------------

    public dispatch(ev: NativeEvent): void {
        switch (ev.type) {
            case 'ready':
            case 'serverReady': {
                this.readyState = true;
                this.ready.resolve();
                break;
            }
            case 'closed': {
                const reason = new TextDecoder().decode(ev.reason);
                this.finish({ closeCode: ev.code, reason }, undefined);
                break;
            }
            case 'error': {
                const err = new WebTransportError(ev.message, { source: 'session' });
                this.finish(null, err);
                break;
            }
            case 'datagram': {
                this.datagramSink?.(ev.data);
                break;
            }
            case 'stream': {
                if (ev.bidi) this.incoming?.onBidi(ev.streamId);
                else this.incoming?.onUni(ev.streamId);
                break;
            }
            case 'streamData': {
                this.receives.get(ev.streamId)?.onData(ev.data);
                break;
            }
            case 'streamFin': {
                this.receives.get(ev.streamId)?.onFin();
                break;
            }
            case 'streamReset': {
                this.receives.get(ev.streamId)?.onReset(ev.code);
                break;
            }
            case 'streamStopSending': {
                this.sends.get(ev.streamId)?.onStopSending(ev.code);
                break;
            }
            case 'streamOpened': {
                this.opens.get(ev.requestId)?.resolve(ev.streamId);
                this.opens.delete(ev.requestId);
                break;
            }
            case 'writeAck': {
                this.writes.get(ev.requestId)?.resolve();
                this.writes.delete(ev.requestId);
                break;
            }
            case 'datagramAck': {
                this.datagramAcks.get(ev.requestId)?.resolve(ev.sent);
                this.datagramAcks.delete(ev.requestId);
                break;
            }
        }
    }

    /** Terminal transition: resolve/reject everything and tear down. */
    private finish(info: WebTransportCloseInfo | null, error: WebTransportError | undefined): void {
        if (this.closedState) return;
        this.closedState = true;

        if (!this.readyState) {
            // The session never established: both `ready` and `closed` reject
            // (a session that closes before it is ready has failed to connect).
            const err =
                error ??
                new WebTransportError('session closed before it was established', {
                    source: 'session',
                });
            this.ready.reject(err);
            this.closed.reject(err);
        } else if (error) {
            // Established, then terminated abnormally.
            this.closed.reject(error);
        } else {
            // Established, then closed cleanly.
            this.closed.resolve(info ?? { closeCode: 0, reason: '' });
        }

        const err = error ?? new WebTransportError('session closed', { source: 'session' });
        for (const d of this.opens.values()) d.reject(err);
        for (const d of this.writes.values()) d.reject(err);
        for (const d of this.datagramAcks.values()) d.resolve(false);
        for (const sink of this.receives.values()) sink.onReset(0);
        for (const sink of this.sends.values()) sink.onStopSending(0);
        this.opens.clear();
        this.writes.clear();
        this.datagramAcks.clear();
        this.receives.clear();
        this.sends.clear();

        this.transport?.shutdown();
    }

    public get isClosed(): boolean {
        return this.closedState;
    }
}

/** Routes session commands through a native client session handle. */
class ClientTransport implements SessionTransport {
    public constructor(
        private readonly native: NativeAddon,
        private readonly handle: NativeHandle,
    ) {}
    public openStream(bidi: boolean, requestId: number): void {
        this.native.openStream(this.handle, bidi, requestId);
    }
    public write(streamId: number, bytes: Uint8Array, requestId: number): void {
        this.native.writeStream(this.handle, streamId, bytes, requestId);
    }
    public fin(streamId: number): void {
        this.native.finStream(this.handle, streamId);
    }
    public reset(streamId: number, code: number): void {
        this.native.resetStream(this.handle, streamId, code);
    }
    public stopSending(streamId: number, code: number): void {
        this.native.stopSending(this.handle, streamId, code);
    }
    public setPaused(streamId: number, paused: boolean): void {
        this.native.setPaused(this.handle, streamId, paused);
    }
    public sendDatagram(bytes: Uint8Array, requestId: number): void {
        this.native.sendDatagram(this.handle, bytes, requestId);
    }
    public maxDatagramSize(): number {
        return this.native.maxDatagramSize(this.handle);
    }
    public closeSession(code: number, reason: Uint8Array): void {
        this.native.closeSession(this.handle, code, reason);
    }
    public shutdown(): void {
        this.native.shutdown(this.handle);
    }
}

/** Create and connect a client-role session. */
export function createClientSession(config: ConnectConfig): SessionCore {
    const native = loadNative();
    const core = new SessionCore();
    try {
        const handle = native.connect(
            config.url,
            config.hashes,
            config.insecure,
            config.origin,
            config.headerNames,
            config.headerValues,
            (ev) => core.dispatch(ev),
        );
        core.attach(new ClientTransport(native, handle));
    } catch (e) {
        core.failSetup(e instanceof Error ? e.message : String(e));
    }
    return core;
}

/** Routes session commands through a native server handle + session id. */
export class ServerTransport implements SessionTransport {
    public constructor(
        private readonly native: NativeAddon,
        private readonly handle: NativeServerHandle,
        private readonly session: number,
    ) {}
    public openStream(bidi: boolean, requestId: number): void {
        this.native.serverOpenStream(this.handle, this.session, bidi, requestId);
    }
    public write(streamId: number, bytes: Uint8Array, requestId: number): void {
        this.native.serverWrite(this.handle, this.session, streamId, bytes, requestId);
    }
    public fin(streamId: number): void {
        this.native.serverFin(this.handle, this.session, streamId);
    }
    public reset(streamId: number, code: number): void {
        this.native.serverReset(this.handle, this.session, streamId, code);
    }
    public stopSending(streamId: number, code: number): void {
        this.native.serverStopSending(this.handle, this.session, streamId, code);
    }
    public setPaused(streamId: number, paused: boolean): void {
        this.native.serverSetPaused(this.handle, this.session, streamId, paused);
    }
    public sendDatagram(bytes: Uint8Array, requestId: number): void {
        this.native.serverSendDatagram(this.handle, this.session, bytes, requestId);
    }
    public maxDatagramSize(): number {
        return this.native.serverMaxDatagramSize(this.handle);
    }
    public closeSession(code: number, reason: Uint8Array): void {
        this.native.serverCloseSession(this.handle, this.session, code, reason);
    }
    public shutdown(): void {
        // The server driver owns the session lifecycle; nothing to tear down here.
    }
}
