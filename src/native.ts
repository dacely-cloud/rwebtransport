// SPDX-License-Identifier: Apache-2.0
//! Typed view of the native addon and the event dispatcher that turns its
//! low-level, callback-based ABI into promises and per-stream sinks.

import { loadNative } from './loader.js';
import { WebTransportError } from './errors.js';
import type { WebTransportCloseInfo } from './types.js';

/** Opaque handle to a native session (a neon `JsBox`). */
export type NativeHandle = { readonly __brand: 'rwt-session' };

/** Events delivered by the native addon to the `onEvent` callback. */
export type NativeEvent =
    | { type: 'ready' }
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
}

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
 * Owns the native handle and dispatches native events to promises / sinks.
 * Exactly one `Session` backs one `WebTransport` object.
 */
export class Session {
    private readonly native: NativeAddon;
    private handle: NativeHandle | undefined;

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

    public constructor(config: ConnectConfig) {
        this.native = loadNative();
        // Swallow default unhandled-rejection warnings; the user observes these.
        void this.closed.promise.catch(() => {});
        void this.ready.promise.catch(() => {});
        try {
            this.handle = this.native.connect(
                config.url,
                config.hashes,
                config.insecure,
                config.origin,
                config.headerNames,
                config.headerValues,
                (ev) => this.dispatch(ev),
            );
        } catch (e) {
            const message = e instanceof Error ? e.message : String(e);
            // Defer so the caller can attach `.ready`/`.closed` handlers first.
            queueMicrotask(() =>
                this.finish(null, new WebTransportError(message, { source: 'session' })),
            );
        }
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
        this.native.openStream(this.handle!, bidi, requestId);
        return d.promise;
    }

    public write(streamId: number, chunk: Uint8Array): Promise<void> {
        if (!this.usable()) return Promise.reject(this.deadError());
        const requestId = this.nextId();
        const d = deferred<void>();
        this.writes.set(requestId, d);
        this.native.writeStream(this.handle!, streamId, chunk, requestId);
        return d.promise;
    }

    public finStream(streamId: number): void {
        if (this.handle) this.native.finStream(this.handle, streamId);
    }

    public resetStream(streamId: number, code: number): void {
        if (this.handle) this.native.resetStream(this.handle, streamId, code >>> 0);
    }

    public stopSending(streamId: number, code: number): void {
        if (this.handle) this.native.stopSending(this.handle, streamId, code >>> 0);
    }

    public setPaused(streamId: number, paused: boolean): void {
        if (this.handle) this.native.setPaused(this.handle, streamId, paused);
    }

    public sendDatagram(chunk: Uint8Array): Promise<boolean> {
        if (!this.usable()) return Promise.resolve(false);
        const requestId = this.nextId();
        const d = deferred<boolean>();
        this.datagramAcks.set(requestId, d);
        this.native.sendDatagram(this.handle!, chunk, requestId);
        return d.promise;
    }

    public maxDatagramSize(): number {
        return this.handle ? this.native.maxDatagramSize(this.handle) : 0;
    }

    public close(code: number, reason: string): void {
        if (this.closedState || !this.handle) return;
        this.native.closeSession(this.handle, code >>> 0, new TextEncoder().encode(reason));
    }

    public shutdown(): void {
        if (this.handle) this.native.shutdown(this.handle);
    }

    /** Whether outbound operations can still be issued (handle live, not closed). */
    private usable(): boolean {
        return this.handle !== undefined && !this.closedState;
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

    private dispatch(ev: NativeEvent): void {
        switch (ev.type) {
            case 'ready': {
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

        if (this.handle) this.native.shutdown(this.handle);
    }

    public get isClosed(): boolean {
        return this.closedState;
    }
}
