// SPDX-License-Identifier: Apache-2.0
//! Typed view of the native addon and the event dispatcher that turns its
//! low-level, callback-based ABI into promises and per-stream sinks.

import { loadNative } from './loader.js';
import { WebTransportError } from './errors.js';
import type { WebTransportCloseInfo, WebTransportConnectionStats } from './types.js';

/**
 * Opaque handle to a native client session (a neon `JsBox`).
 *
 * @remarks
 * Returned by {@link NativeAddon.connect} and passed back into every
 * per-session native call so the addon can find the session's driver thread.
 * The `__brand` field is a compile-time nominal marker only: there is no such
 * property at runtime, and its purpose is to keep a client handle from being
 * confused with a {@link NativeServerHandle} or a plain object.
 * @internal
 */
export type NativeHandle = { readonly __brand: 'rwt-session' };

/**
 * Opaque handle to a native server (a neon `JsBox`).
 *
 * @remarks
 * Returned by {@link NativeAddon.serverListen} and passed back into every
 * `server*` native call, together with a numeric session id, to target one of
 * the server's active sessions. Like {@link NativeHandle}, `__brand` is a
 * compile-time-only nominal marker.
 * @internal
 */
export type NativeServerHandle = { readonly __brand: 'rwt-server' };

/**
 * The `type` discriminant shared by every {@link NativeEvent} and
 * {@link ServerNativeEvent}.
 *
 * @remarks
 * The string values are the exact wire contract emitted by the native addon, so
 * they must not be changed. Using an enum (rather than bare string literals)
 * gives the TypeScript layer a single, referenceable source of truth for the
 * event kinds.
 * @internal
 */
export enum NativeEventType {
    /** A client session's Extended CONNECT completed and it is established. */
    Ready = 'ready',
    /** The peer sent DRAIN_WEBTRANSPORT_SESSION (going away soon, still usable). */
    Draining = 'draining',
    /** A server session was established (a valid CONNECT arrived, answered 200). */
    ServerReady = 'serverReady',
    /** The session ended. */
    Closed = 'closed',
    /** The session failed with an error message. */
    Error = 'error',
    /** An inbound datagram payload arrived. */
    Datagram = 'datagram',
    /** The peer opened a stream. */
    Stream = 'stream',
    /** Inbound application bytes arrived on an existing stream. */
    StreamData = 'streamData',
    /** The peer finished (FIN) its side of a stream. */
    StreamFin = 'streamFin',
    /** The peer reset a stream. */
    StreamReset = 'streamReset',
    /** The peer sent STOP_SENDING for a stream. */
    StreamStopSending = 'streamStopSending',
    /** Acknowledges a locally requested {@link NativeAddon.openStream}. */
    StreamOpened = 'streamOpened',
    /** Acknowledges a {@link NativeAddon.writeStream} (the backpressure signal). */
    WriteAck = 'writeAck',
    /** Acknowledges a {@link NativeAddon.sendDatagram}. */
    DatagramAck = 'datagramAck',
    /** Result of a {@link NativeAddon.getStats} request. */
    Stats = 'stats',
    /** Result of a {@link NativeAddon.exportKeyingMaterial} request. */
    KeyingMaterial = 'keyingMaterial',
    /** Server level: the listener is bound (carries the port). */
    Listening = 'listening',
    /** Server level: a fatal server failure. */
    ServerError = 'serverError',
    /** Server level: the server stopped. */
    ServerClosed = 'serverClosed',
}

/**
 * The client session's Extended CONNECT completed and the session is established.
 * @internal
 */
export interface NativeReadyEvent {
    /** Discriminant. */
    type: NativeEventType.Ready;
}

/**
 * The peer sent a DRAIN_WEBTRANSPORT_SESSION capsule.
 * @internal
 */
export interface NativeDrainingEvent {
    /** Discriminant. */
    type: NativeEventType.Draining;
}

/**
 * A server session was established. Carries the client's CONNECT request
 * metadata and the client's remote address at establishment.
 * @internal
 */
export interface NativeServerReadyEvent {
    /** Discriminant. */
    type: NativeEventType.ServerReady;
    /** The `:authority` pseudo-header of the client's CONNECT request. */
    authority: string;
    /** The `:path` pseudo-header of the client's CONNECT request. */
    path: string;
    /** The `origin` header value, or `null` if the client sent none. */
    origin: string | null;
    /** Any additional (non-pseudo) request headers, keyed by header name. */
    headers: Record<string, string>;
    /** The client's remote IP address at session establishment. */
    remoteAddress: string;
    /** The client's remote UDP port at session establishment. */
    remotePort: number;
}

/**
 * The session ended. `code`/`reason` are the WebTransport close code and UTF-8
 * encoded reason; `remote` is true when the peer initiated the close.
 * @internal
 */
export interface NativeClosedEvent {
    /** Discriminant. */
    type: NativeEventType.Closed;
    /** The WebTransport close code. */
    code: number;
    /** The UTF-8 encoded close reason. */
    reason: Uint8Array;
    /** True when the peer initiated the close. */
    remote: boolean;
}

/**
 * The session failed with the given message.
 * @internal
 */
export interface NativeErrorEvent {
    /** Discriminant. */
    type: NativeEventType.Error;
    /** Human-readable failure message. */
    message: string;
}

/**
 * An inbound datagram payload.
 * @internal
 */
export interface NativeDatagramEvent {
    /** Discriminant. */
    type: NativeEventType.Datagram;
    /** The received datagram bytes. */
    data: Uint8Array;
}

/**
 * The peer opened a stream.
 * @internal
 */
export interface NativeStreamEvent {
    /** Discriminant. */
    type: NativeEventType.Stream;
    /** The id of the newly opened stream. */
    streamId: number;
    /** True for a bidirectional stream, false for a unidirectional one. */
    bidi: boolean;
}

/**
 * Inbound application bytes on an existing stream.
 * @internal
 */
export interface NativeStreamDataEvent {
    /** Discriminant. */
    type: NativeEventType.StreamData;
    /** The stream the bytes arrived on. */
    streamId: number;
    /** The received bytes. */
    data: Uint8Array;
}

/**
 * The peer finished (FIN) its side of a stream.
 * @internal
 */
export interface NativeStreamFinEvent {
    /** Discriminant. */
    type: NativeEventType.StreamFin;
    /** The stream that was finished. */
    streamId: number;
}

/**
 * The peer reset a stream.
 * @internal
 */
export interface NativeStreamResetEvent {
    /** Discriminant. */
    type: NativeEventType.StreamReset;
    /** The stream that was reset. */
    streamId: number;
    /** The application error code the peer sent. */
    code: number;
}

/**
 * The peer sent STOP_SENDING for a stream.
 * @internal
 */
export interface NativeStreamStopSendingEvent {
    /** Discriminant. */
    type: NativeEventType.StreamStopSending;
    /** The stream the peer asked us to stop sending on. */
    streamId: number;
    /** The application error code the peer sent. */
    code: number;
}

/**
 * Acknowledges a locally requested {@link NativeAddon.openStream}, pairing the
 * JS-generated `requestId` with the assigned stream id.
 * @internal
 */
export interface NativeStreamOpenedEvent {
    /** Discriminant. */
    type: NativeEventType.StreamOpened;
    /** The request id passed to {@link NativeAddon.openStream}. */
    requestId: number;
    /** The stream id the native side assigned. */
    streamId: number;
}

/**
 * Acknowledges a {@link NativeAddon.writeStream}: the bytes have been flushed
 * into quiche. This is the write backpressure signal.
 * @internal
 */
export interface NativeWriteAckEvent {
    /** Discriminant. */
    type: NativeEventType.WriteAck;
    /** The request id passed to {@link NativeAddon.writeStream}. */
    requestId: number;
}

/**
 * Acknowledges a {@link NativeAddon.sendDatagram}.
 * @internal
 */
export interface NativeDatagramAckEvent {
    /** Discriminant. */
    type: NativeEventType.DatagramAck;
    /** The request id passed to {@link NativeAddon.sendDatagram}. */
    requestId: number;
    /**
     * False when the datagram was dropped instead of sent (for example the
     * session was not ready or the payload exceeded the current flight size).
     */
    sent: boolean;
}

/**
 * The result of a {@link NativeAddon.getStats} request, keyed by `requestId`.
 * @internal
 */
export interface NativeStatsEvent {
    /** Discriminant. */
    type: NativeEventType.Stats;
    /** The request id passed to {@link NativeAddon.getStats}. */
    requestId: number;
    /** Total bytes sent. */
    bytesSent: number;
    /** Total bytes received. */
    bytesReceived: number;
    /** QUIC packets sent. */
    packetsSent: number;
    /** QUIC packets received. */
    packetsReceived: number;
    /** QUIC packets declared lost. */
    packetsLost: number;
    /** Smoothed RTT in milliseconds. */
    smoothedRtt: number;
    /** RTT variation in milliseconds. */
    rttVariation: number;
    /** Minimum observed RTT in milliseconds. */
    minRtt: number;
}

/**
 * The result of a {@link NativeAddon.exportKeyingMaterial} request, keyed by
 * `requestId`.
 * @internal
 */
export interface NativeKeyingMaterialEvent {
    /** Discriminant. */
    type: NativeEventType.KeyingMaterial;
    /** The request id passed to {@link NativeAddon.exportKeyingMaterial}. */
    requestId: number;
    /** True if the export succeeded; false if the TLS export failed. */
    ok: boolean;
    /** The exported keying material, present only when `ok` is true. */
    data?: Uint8Array;
}

/**
 * Events delivered by the native addon to a client session's, or one server
 * session's, `onEvent` callback.
 *
 * @remarks
 * A discriminated union keyed on {@link NativeEventType}. Each session receives
 * its own stream of these, dispatched by {@link SessionCore.dispatch}.
 * @internal
 */
export type NativeEvent =
    | NativeReadyEvent
    | NativeDrainingEvent
    | NativeServerReadyEvent
    | NativeClosedEvent
    | NativeErrorEvent
    | NativeDatagramEvent
    | NativeStreamEvent
    | NativeStreamDataEvent
    | NativeStreamFinEvent
    | NativeStreamResetEvent
    | NativeStreamStopSendingEvent
    | NativeStreamOpenedEvent
    | NativeWriteAckEvent
    | NativeDatagramAckEvent
    | NativeStatsEvent
    | NativeKeyingMaterialEvent;

/**
 * The raw function surface exported by `rwebtransport.node`.
 *
 * @remarks
 * This interface is the TypeScript view of the neon addon and is implemented by
 * native code, not by any TS class. Client-facing calls take a
 * {@link NativeHandle}; server-facing calls (prefixed `server`) take a
 * {@link NativeServerHandle} plus a numeric session id. Asynchronous results are
 * not returned directly: they arrive later as {@link NativeEvent}s on the
 * `onEvent` callback, correlated to the originating call by a JS-generated
 * `requestId`.
 * @see The native ABI notes for the exact wire contract.
 * @internal
 */
export interface NativeAddon {
    /**
     * Open a client session by performing a QUIC handshake and WebTransport
     * Extended CONNECT to `url`, spawning a background driver thread.
     *
     * @param url - The `https://host:port/path` session URL.
     * @param hashes - SHA-256 certificate pins as 32-byte arrays; when non-empty,
     *   a matching certificate is accepted and normal CA/hostname validation is
     *   bypassed.
     * @param insecure - Disable all certificate verification (development only).
     * @param origin - Value for the `origin` request header, or null to omit it.
     * @param headerNames - Additional request header names (parallel to `headerValues`).
     * @param headerValues - Additional request header values (parallel to `headerNames`).
     * @param onEvent - Callback invoked for every subsequent session event.
     * @returns An opaque handle to the new session.
     * @throws Synchronously on setup failures such as a malformed URL, DNS
     *   resolution failure, socket bind failure, or invalid configuration.
     */
    connect(
        url: string,
        hashes: Uint8Array[],
        insecure: boolean,
        origin: string | null,
        headerNames: string[],
        headerValues: string[],
        onEvent: (ev: NativeEvent) => void,
    ): NativeHandle;
    /**
     * Request a new outbound stream. The result arrives asynchronously as a
     * `streamOpened` event carrying the same `requestId` and the assigned stream id.
     *
     * @param handle - The session handle.
     * @param bidi - True for a bidirectional stream, false for send-only unidirectional.
     * @param requestId - JS-generated id used to correlate the `streamOpened` event.
     */
    openStream(handle: NativeHandle, bidi: boolean, requestId: number): void;
    /**
     * Queue `bytes` for sending on `streamId`. A `writeAck` event with the same
     * `requestId` fires once the bytes are flushed into quiche's send buffer,
     * which is the backpressure signal used to resolve the write promise.
     *
     * @param handle - The session handle.
     * @param streamId - The target stream id.
     * @param bytes - The payload to send.
     * @param requestId - JS-generated id used to correlate the `writeAck` event.
     */
    writeStream(handle: NativeHandle, streamId: number, bytes: Uint8Array, requestId: number): void;
    /**
     * Finish (half-close) the send side of `streamId`, sending a QUIC FIN.
     *
     * @param handle - The session handle.
     * @param streamId - The stream to finish.
     */
    finStream(handle: NativeHandle, streamId: number): void;
    /**
     * Abruptly reset the send side of `streamId` with the given application error code.
     *
     * @param handle - The session handle.
     * @param streamId - The stream to reset.
     * @param code - The application error code (unsigned 32-bit).
     */
    resetStream(handle: NativeHandle, streamId: number, code: number): void;
    /**
     * Ask the peer to stop sending on `streamId` (QUIC STOP_SENDING) with the
     * given application error code.
     *
     * @param handle - The session handle.
     * @param streamId - The stream whose inbound half should be stopped.
     * @param code - The application error code (unsigned 32-bit).
     */
    stopSending(handle: NativeHandle, streamId: number, code: number): void;
    /**
     * Toggle read backpressure on `streamId`. When paused, the native side stops
     * draining quiche's receive buffer, which flow-controls the peer.
     *
     * @param handle - The session handle.
     * @param streamId - The receive stream to pause or resume.
     * @param paused - True to pause draining, false to resume.
     */
    setPaused(handle: NativeHandle, streamId: number, paused: boolean): void;
    /**
     * Send a datagram. A `datagramAck` event with the same `requestId` reports
     * whether it was actually sent.
     *
     * @param handle - The session handle.
     * @param bytes - The datagram payload.
     * @param requestId - JS-generated id used to correlate the `datagramAck` event.
     */
    sendDatagram(handle: NativeHandle, bytes: Uint8Array, requestId: number): void;
    /**
     * @param handle - The session handle.
     * @returns The maximum datagram payload size currently permitted, in bytes
     *   (0 when datagrams cannot be sent).
     */
    maxDatagramSize(handle: NativeHandle): number;
    /**
     * @param handle - The session handle.
     * @returns Whether the session has been established (synchronous, via shared atomics).
     */
    isReady(handle: NativeHandle): boolean;
    /**
     * @param handle - The session handle.
     * @returns Whether the session has closed (synchronous, via shared atomics).
     */
    isClosed(handle: NativeHandle): boolean;
    /**
     * Gracefully close the session: send a CLOSE_WEBTRANSPORT_SESSION capsule and
     * FIN the CONNECT stream, then close the QUIC connection.
     *
     * @param handle - The session handle.
     * @param code - The WebTransport close code (unsigned 32-bit).
     * @param reason - The UTF-8 encoded close reason.
     */
    closeSession(handle: NativeHandle, code: number, reason: Uint8Array): void;
    /** Send a DRAIN_WEBTRANSPORT_SESSION capsule to the peer. */
    drain(handle: NativeHandle): void;
    /** Request connection stats; the result arrives as a `stats` event. */
    getStats(handle: NativeHandle, requestId: number): void;
    /**
     * Export TLS keying material (RFC 5705); the result arrives as a
     * `keyingMaterial` event.
     *
     * @param handle - The session handle.
     * @param requestId - Correlates the reply event.
     * @param label - The exporter label bytes.
     * @param context - The exporter context bytes (may be empty).
     * @param length - Number of bytes of keying material to produce.
     */
    exportKeyingMaterial(
        handle: NativeHandle,
        requestId: number,
        label: Uint8Array,
        context: Uint8Array,
        length: number,
    ): void;
    /**
     * Tear down the session's driver thread. Called on close; the native
     * finalizer also invokes it as a safety net.
     *
     * @param handle - The session handle.
     */
    shutdown(handle: NativeHandle): void;

    /**
     * Start a WebTransport server: bind a UDP socket and begin accepting sessions.
     *
     * @param certPath - Path to the PEM certificate chain file.
     * @param keyPath - Path to the PEM private-key file.
     * @param host - Interface address to bind.
     * @param port - UDP port to bind (0 lets the OS choose; read it back from the
     *   `listening` event).
     * @param reusePort - Bind with `SO_REUSEPORT` (Unix only) to share a port
     *   across processes.
     * @param onEvent - Callback invoked for server-level and session-scoped events.
     * @returns An opaque handle to the server.
     */
    serverListen(
        certPath: string,
        keyPath: string,
        host: string,
        port: number,
        reusePort: boolean,
        onEvent: (ev: ServerNativeEvent) => void,
    ): NativeServerHandle;
    /**
     * Server-side {@link openStream}: request a new outbound stream on the given
     * session.
     *
     * @param handle - The server handle.
     * @param session - The target session id.
     * @param bidi - True for bidirectional, false for send-only unidirectional.
     * @param requestId - JS-generated id used to correlate the `streamOpened` event.
     */
    serverOpenStream(
        handle: NativeServerHandle,
        session: number,
        bidi: boolean,
        requestId: number,
    ): void;
    /**
     * Server-side {@link writeStream}: queue bytes on a session's stream.
     *
     * @param handle - The server handle.
     * @param session - The target session id.
     * @param streamId - The target stream id.
     * @param bytes - The payload to send.
     * @param requestId - JS-generated id used to correlate the `writeAck` event.
     */
    serverWrite(
        handle: NativeServerHandle,
        session: number,
        streamId: number,
        bytes: Uint8Array,
        requestId: number,
    ): void;
    /**
     * Server-side {@link finStream}: finish the send side of a session's stream.
     *
     * @param handle - The server handle.
     * @param session - The target session id.
     * @param streamId - The stream to finish.
     */
    serverFin(handle: NativeServerHandle, session: number, streamId: number): void;
    /**
     * Server-side {@link resetStream}: reset the send side of a session's stream.
     *
     * @param handle - The server handle.
     * @param session - The target session id.
     * @param streamId - The stream to reset.
     * @param code - The application error code (unsigned 32-bit).
     */
    serverReset(handle: NativeServerHandle, session: number, streamId: number, code: number): void;
    /**
     * Server-side {@link stopSending}: ask the peer to stop sending on a session's stream.
     *
     * @param handle - The server handle.
     * @param session - The target session id.
     * @param streamId - The stream whose inbound half should be stopped.
     * @param code - The application error code (unsigned 32-bit).
     */
    serverStopSending(
        handle: NativeServerHandle,
        session: number,
        streamId: number,
        code: number,
    ): void;
    /**
     * Server-side {@link setPaused}: toggle read backpressure on a session's stream.
     *
     * @param handle - The server handle.
     * @param session - The target session id.
     * @param streamId - The receive stream to pause or resume.
     * @param paused - True to pause draining, false to resume.
     */
    serverSetPaused(
        handle: NativeServerHandle,
        session: number,
        streamId: number,
        paused: boolean,
    ): void;
    /**
     * Server-side {@link sendDatagram}: send a datagram on a session.
     *
     * @param handle - The server handle.
     * @param session - The target session id.
     * @param bytes - The datagram payload.
     * @param requestId - JS-generated id used to correlate the `datagramAck` event.
     */
    serverSendDatagram(
        handle: NativeServerHandle,
        session: number,
        bytes: Uint8Array,
        requestId: number,
    ): void;
    /**
     * Server-side {@link maxDatagramSize}.
     *
     * @param handle - The server handle.
     * @returns The maximum datagram payload size currently permitted, in bytes.
     */
    serverMaxDatagramSize(handle: NativeServerHandle): number;
    /**
     * Server-side {@link closeSession}: gracefully close one session.
     *
     * @param handle - The server handle.
     * @param session - The session id to close.
     * @param code - The WebTransport close code (unsigned 32-bit).
     * @param reason - The UTF-8 encoded close reason.
     */
    serverCloseSession(
        handle: NativeServerHandle,
        session: number,
        code: number,
        reason: Uint8Array,
    ): void;
    /** Send a DRAIN_WEBTRANSPORT_SESSION capsule to one session's peer. */
    serverDrain(handle: NativeServerHandle, session: number): void;
    /** Request one session's connection stats; the result arrives as a `stats` event. */
    serverGetStats(handle: NativeServerHandle, session: number, requestId: number): void;
    /**
     * Export TLS keying material (RFC 5705) for one session; the result arrives
     * as a `keyingMaterial` event.
     */
    serverExportKeyingMaterial(
        handle: NativeServerHandle,
        session: number,
        requestId: number,
        label: Uint8Array,
        context: Uint8Array,
        length: number,
    ): void;
    /**
     * Stop the server, closing all its sessions and freeing the listening socket.
     *
     * @param handle - The server handle.
     */
    serverShutdown(handle: NativeServerHandle): void;
}

/**
 * Server level: the listener is bound and accepting packets.
 * @internal
 */
export interface NativeListeningEvent {
    /** Discriminant. */
    type: NativeEventType.Listening;
    /** The UDP port the server actually bound. */
    port: number;
}

/**
 * Server level: a fatal server failure (for example a bind or certificate error).
 * @internal
 */
export interface NativeServerErrorEvent {
    /** Discriminant. */
    type: NativeEventType.ServerError;
    /** Human-readable failure message. */
    message: string;
}

/**
 * Server level: the server stopped.
 * @internal
 */
export interface NativeServerClosedEvent {
    /** Discriminant. */
    type: NativeEventType.ServerClosed;
}

/**
 * Server-level events, plus every {@link NativeEvent} widened with a numeric
 * `session` id.
 *
 * @remarks
 * The `session` field identifies which of the server's sessions a session-scoped
 * event belongs to, so the server can demultiplex a single `onEvent` stream
 * across all its active sessions.
 * @internal
 */
export type ServerNativeEvent =
    | NativeListeningEvent
    | NativeServerErrorEvent
    | NativeServerClosedEvent
    | (NativeEvent & { session: number });

/**
 * Normalized inputs for opening a session.
 *
 * @remarks
 * Produced by the client `WebTransport` layer from user-facing options and
 * passed straight through to {@link NativeAddon.connect} by
 * {@link createClientSession}. The header arrays are parallel (`headerNames[i]`
 * pairs with `headerValues[i]`).
 * @internal
 */
export interface ConnectConfig {
    /** The `https://host:port/path` session URL. */
    url: string;
    /** SHA-256 certificate pins as 32-byte arrays; empty when none are pinned. */
    hashes: Uint8Array[];
    /** Disable all certificate verification (development only). */
    insecure: boolean;
    /** Value for the `origin` request header, or null to omit it. */
    origin: string | null;
    /** Additional request header names, parallel to {@link ConnectConfig.headerValues}. */
    headerNames: string[];
    /** Additional request header values, parallel to {@link ConnectConfig.headerNames}. */
    headerValues: string[];
}

/**
 * Consumer of a receive stream's inbound events.
 *
 * @remarks
 * Registered per stream via {@link SessionCore.registerReceive}. The stream
 * layer implements this to feed a WHATWG `ReadableStream` controller.
 * @internal
 */
export interface ReceiveSink {
    /**
     * A chunk of stream data arrived.
     *
     * @param chunk - The received bytes.
     */
    onData(chunk: Uint8Array): void;
    /** The peer finished (FIN) the stream; no more data will arrive. */
    onFin(): void;
    /**
     * The peer reset the stream.
     *
     * @param code - The application error code from the reset.
     */
    onReset(code: number): void;
}

/**
 * Consumer of a send stream's control events.
 *
 * @remarks
 * Registered per stream via {@link SessionCore.registerSend}. The stream layer
 * implements this to error its WHATWG `WritableStream` controller.
 * @internal
 */
export interface SendSink {
    /**
     * The peer asked to stop receiving (QUIC STOP_SENDING).
     *
     * @param code - The application error code the peer supplied.
     */
    onStopSending(code: number): void;
}

/**
 * Consumer of peer-initiated streams.
 *
 * @remarks
 * A single handler per session, set via {@link SessionCore.setIncomingHandler};
 * invoked when the peer opens a new stream.
 * @internal
 */
export interface IncomingHandler {
    /**
     * The peer opened a bidirectional stream.
     *
     * @param streamId - The new stream's id.
     */
    onBidi(streamId: number): void;
    /**
     * The peer opened a unidirectional (receive-only) stream.
     *
     * @param streamId - The new stream's id.
     */
    onUni(streamId: number): void;
}

/**
 * Consumer of inbound datagrams.
 *
 * @remarks
 * A single sink per session, set via {@link SessionCore.setDatagramSink};
 * invoked with each datagram payload as it arrives.
 * @param data - The datagram payload.
 * @internal
 */
export type DatagramSink = (data: Uint8Array) => void;

/**
 * A promise bundled with its own `resolve`/`reject` functions.
 *
 * @typeParam T - The value the promise resolves to.
 * @remarks
 * Used to correlate a native acknowledgement event (which arrives later) with
 * the caller awaiting it.
 * @internal
 */
interface Deferred<T> {
    /** The pending promise handed to the awaiter. */
    promise: Promise<T>;
    /** Settles {@link Deferred.promise} with a value. */
    resolve: (value: T) => void;
    /** Rejects {@link Deferred.promise} with a reason. */
    reject: (reason: unknown) => void;
}

/**
 * Create a {@link Deferred}: a promise together with externally callable
 * `resolve` and `reject`.
 *
 * @typeParam T - The value the promise resolves to.
 * @returns The bundled promise and its settle functions.
 * @internal
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

/**
 * The low-level per-session command surface. Implemented by a client transport
 * (native session handle) or a server transport (server handle + session id), so
 * the same {@link SessionCore} drives both roles.
 *
 * @remarks
 * These methods are fire-and-forget: their outcomes come back later as
 * {@link NativeEvent}s that {@link SessionCore.dispatch} routes to the awaiting
 * promises and sinks.
 * @internal
 */
export interface SessionTransport {
    /**
     * Request a new outbound stream.
     *
     * @param bidi - True for bidirectional, false for send-only unidirectional.
     * @param requestId - Id correlating the eventual `streamOpened` event.
     */
    openStream(bidi: boolean, requestId: number): void;
    /**
     * Queue bytes for sending on a stream.
     *
     * @param streamId - The target stream id.
     * @param bytes - The payload.
     * @param requestId - Id correlating the eventual `writeAck` event.
     */
    write(streamId: number, bytes: Uint8Array, requestId: number): void;
    /**
     * Finish (half-close) the send side of a stream.
     *
     * @param streamId - The stream to finish.
     */
    fin(streamId: number): void;
    /**
     * Reset the send side of a stream.
     *
     * @param streamId - The stream to reset.
     * @param code - The application error code.
     */
    reset(streamId: number, code: number): void;
    /**
     * Ask the peer to stop sending on a stream.
     *
     * @param streamId - The stream whose inbound half should be stopped.
     * @param code - The application error code.
     */
    stopSending(streamId: number, code: number): void;
    /**
     * Toggle read backpressure on a stream.
     *
     * @param streamId - The receive stream.
     * @param paused - True to pause draining, false to resume.
     */
    setPaused(streamId: number, paused: boolean): void;
    /**
     * Send a datagram.
     *
     * @param bytes - The datagram payload.
     * @param requestId - Id correlating the eventual `datagramAck` event.
     */
    sendDatagram(bytes: Uint8Array, requestId: number): void;
    /** @returns The maximum datagram payload size, in bytes. */
    maxDatagramSize(): number;
    /**
     * Gracefully close the session.
     *
     * @param code - The WebTransport close code.
     * @param reason - The UTF-8 encoded close reason.
     */
    closeSession(code: number, reason: Uint8Array): void;
    /** Send a DRAIN_WEBTRANSPORT_SESSION capsule to the peer. */
    drain(): void;
    /** Request connection stats; the result arrives as a `stats` event. */
    getStats(requestId: number): void;
    /**
     * Export TLS keying material (RFC 5705); the result arrives as a
     * `keyingMaterial` event.
     *
     * @param requestId - Id correlating the eventual `keyingMaterial` event.
     * @param label - The exporter label bytes.
     * @param context - The exporter context bytes (may be empty).
     * @param length - Number of bytes of keying material to produce.
     */
    exportKeyingMaterial(
        requestId: number,
        label: Uint8Array,
        context: Uint8Array,
        length: number,
    ): void;
    /** Tear down the session's native resources. */
    shutdown(): void;
}

/**
 * Dispatches native events to promises / sinks and issues commands through an
 * injected {@link SessionTransport}. Exactly one `SessionCore` backs one
 * WebTransport session (client or server side).
 *
 * @remarks
 * The higher layers ({@link WebTransportReceiveStream | streams} and
 * {@link WebTransportDatagramDuplexStream | datagrams}) never touch the native
 * addon directly: they call this core, which owns the request-id bookkeeping and
 * the lifecycle promises.
 * @internal
 */
export class SessionCore {
    /**
     * The command transport, undefined until {@link SessionCore.attach} runs.
     * While undefined, outbound commands are dropped or rejected.
     */
    private transport: SessionTransport | undefined;

    /** Monotonic source of request ids handed to the transport. */
    private nextRequestId = 1;
    /** Pending {@link SessionCore.openStream} requests, keyed by request id. */
    private readonly opens = new Map<number, Deferred<number>>();
    /** Pending {@link SessionCore.write} requests, keyed by request id. */
    private readonly writes = new Map<number, Deferred<void>>();
    /** Pending {@link SessionCore.sendDatagram} requests, keyed by request id. */
    private readonly datagramAcks = new Map<number, Deferred<boolean>>();
    /** Pending getStats() requests, keyed by request id. */
    private readonly statsRequests = new Map<number, Deferred<WebTransportConnectionStats>>();
    /** Pending exportKeyingMaterial() requests, keyed by request id. */
    private readonly keyingMaterialRequests = new Map<number, Deferred<Uint8Array>>();
    /** Active receive sinks, keyed by stream id. */
    private readonly receives = new Map<number, ReceiveSink>();
    /** Active send sinks, keyed by stream id. */
    private readonly sends = new Map<number, SendSink>();

    /** Handler for peer-initiated streams, if one has been set. */
    private incoming: IncomingHandler | undefined;
    /** Sink for inbound datagrams, if one has been set. */
    private datagramSink: DatagramSink | undefined;
    /** Count of inbound datagrams dropped because the readable queue was full. */
    private droppedIncomingDatagrams = 0;

    /** True once the session has reached its terminal (closed) state. */
    private closedState = false;
    /** True once the session has been established (`ready`/`serverReady`). */
    private readyState = false;
    /** True when the session reached its terminal state through an error. */
    private failedState = false;
    /** The most recent stats reported by the native layer, cached for {@link SessionCore.getStats}. */
    private lastStats: WebTransportConnectionStats | undefined;

    /**
     * Resolves when the session is established, rejects if it fails before then.
     * Backs the public `WebTransport.ready` promise.
     */
    public readonly ready = deferred<void>();
    /**
     * Resolves with close info on a clean shutdown, rejects on an error or on a
     * close that happens before the session is ready. Backs `WebTransport.closed`.
     */
    public readonly closed = deferred<WebTransportCloseInfo>();
    /**
     * Resolves when the peer sends a DRAIN_WEBTRANSPORT_SESSION capsule (it
     * intends to close soon but the session stays usable). Backs
     * `WebTransport.draining`. Never rejects.
     */
    public readonly draining = deferred<void>();

    /**
     * Attach no-op catch handlers to {@link SessionCore.ready} and
     * {@link SessionCore.closed} so that a session whose promises the user never
     * observes does not trigger Node's unhandled-rejection warning.
     */
    public constructor() {
        // Swallow default unhandled-rejection warnings; the user observes these.
        void this.closed.promise.catch(() => {});
        void this.ready.promise.catch(() => {});
    }

    /**
     * Attach the transport once the underlying native handle exists.
     *
     * @param transport - The command sink for this session (client or server side).
     */
    public attach(transport: SessionTransport): void {
        this.transport = transport;
    }

    /**
     * Fail a session that could not be set up at all (e.g. client connect threw).
     *
     * @param message - Human-readable description of the setup failure.
     * @remarks
     * The failure is delivered on a microtask so the caller has a chance to
     * attach `.ready`/`.closed` handlers before they reject.
     */
    public failSetup(message: string): void {
        // Defer so the caller can attach `.ready`/`.closed` handlers first.
        queueMicrotask(() =>
            this.finish(null, new WebTransportError(message, { source: 'session' })),
        );
    }

    /**
     * @returns The next request id, post-incrementing the internal counter.
     */
    private nextId(): number {
        return this.nextRequestId++;
    }

    /**
     * Open an outbound stream and await its assigned id.
     *
     * @param bidi - True for a bidirectional stream, false for unidirectional.
     * @returns A promise for the new stream's id.
     * @remarks
     * Rejects immediately (without touching the transport) if the session is not
     * usable (unattached or closed).
     */
    public openStream(bidi: boolean): Promise<number> {
        if (!this.usable()) return Promise.reject(this.deadError());
        const requestId = this.nextId();
        const d = deferred<number>();
        this.opens.set(requestId, d);
        this.transport!.openStream(bidi, requestId);
        return d.promise;
    }

    /**
     * Write a chunk to a stream; resolves once the bytes are flushed into quiche
     * (the write backpressure signal).
     *
     * @param streamId - The target stream id.
     * @param chunk - The bytes to send.
     * @returns A promise that resolves when the write is acknowledged.
     * @remarks
     * Rejects immediately if the session is not usable.
     */
    public write(streamId: number, chunk: Uint8Array): Promise<void> {
        if (!this.usable()) return Promise.reject(this.deadError());
        const requestId = this.nextId();
        const d = deferred<void>();
        this.writes.set(requestId, d);
        this.transport!.write(streamId, chunk, requestId);
        return d.promise;
    }

    /**
     * Finish (half-close) the send side of a stream. No-op if not attached.
     *
     * @param streamId - The stream to finish.
     */
    public finStream(streamId: number): void {
        this.transport?.fin(streamId);
    }

    /**
     * Reset the send side of a stream. No-op if not attached.
     *
     * @param streamId - The stream to reset.
     * @param code - The application error code; coerced to an unsigned 32-bit value.
     */
    public resetStream(streamId: number, code: number): void {
        this.transport?.reset(streamId, code >>> 0);
    }

    /**
     * Ask the peer to stop sending on a stream. No-op if not attached.
     *
     * @param streamId - The stream whose inbound half should be stopped.
     * @param code - The application error code; coerced to an unsigned 32-bit value.
     */
    public stopSending(streamId: number, code: number): void {
        this.transport?.stopSending(streamId, code >>> 0);
    }

    /**
     * Toggle read backpressure on a stream. No-op if not attached.
     *
     * @param streamId - The receive stream.
     * @param paused - True to pause native draining (flow-control the peer), false to resume.
     */
    public setPaused(streamId: number, paused: boolean): void {
        this.transport?.setPaused(streamId, paused);
    }

    /**
     * Send a datagram.
     *
     * @param chunk - The datagram payload.
     * @returns A promise resolving to whether the datagram was actually sent;
     *   resolves to false (rather than rejecting) when the session is not usable.
     */
    public sendDatagram(chunk: Uint8Array): Promise<boolean> {
        if (!this.usable()) return Promise.resolve(false);
        const requestId = this.nextId();
        const d = deferred<boolean>();
        this.datagramAcks.set(requestId, d);
        this.transport!.sendDatagram(chunk, requestId);
        return d.promise;
    }

    /**
     * @returns The maximum datagram payload size in bytes, or 0 if not attached.
     */
    public maxDatagramSize(): number {
        return this.transport ? this.transport.maxDatagramSize() : 0;
    }

    /**
     * Gracefully close the session. No-op if already closed or not attached.
     *
     * @param code - The WebTransport close code; coerced to an unsigned 32-bit value.
     * @param reason - The close reason, UTF-8 encoded before it is sent.
     */
    public close(code: number, reason: string): void {
        if (this.closedState || !this.transport) return;
        this.transport.closeSession(code >>> 0, new TextEncoder().encode(reason));
    }

    /** Tell the peer this session is draining (send a DRAIN capsule). */
    public drain(): void {
        this.transport?.drain();
    }

    /**
     * Snapshot the connection's stats. Resolves with a
     * {@link WebTransportConnectionStats} once the driver reports back. After a
     * clean close it resolves with the last stats seen while live; it rejects
     * only when the session was never attached or terminated through an error.
     */
    public getStats(): Promise<WebTransportConnectionStats> {
        if (!this.usable()) {
            if (!this.failedState && this.lastStats !== undefined) {
                return Promise.resolve(this.lastStats);
            }
            return Promise.reject(this.deadError());
        }
        const requestId = this.nextId();
        const d = deferred<WebTransportConnectionStats>();
        this.statsRequests.set(requestId, d);
        this.transport!.getStats(requestId);
        return d.promise;
    }

    /**
     * Export TLS keying material (RFC 5705). Resolves with `length` bytes derived
     * from `label` and `context` once the driver reports back; rejects if the
     * session is not usable or the TLS export fails.
     */
    public exportKeyingMaterial(
        label: Uint8Array,
        context: Uint8Array,
        length: number,
    ): Promise<Uint8Array> {
        if (!this.usable()) return Promise.reject(this.deadError());
        const requestId = this.nextId();
        const d = deferred<Uint8Array>();
        this.keyingMaterialRequests.set(requestId, d);
        this.transport!.exportKeyingMaterial(requestId, label, context, length);
        return d.promise;
    }

    /** Tear down the native resources for this session. No-op if not attached. */
    public shutdown(): void {
        this.transport?.shutdown();
    }

    /**
     * Whether outbound operations can still be issued (attached, not closed).
     *
     * @returns True when a transport is attached and the session is not closed.
     */
    private usable(): boolean {
        return this.transport !== undefined && !this.closedState;
    }

    /**
     * @returns The error used to reject operations attempted on a dead session.
     */
    private deadError(): WebTransportError {
        return new WebTransportError('session is closed', { source: 'session' });
    }

    /**
     * Register a sink to receive a stream's inbound data/fin/reset events.
     *
     * @param streamId - The stream to observe.
     * @param sink - The receiver of that stream's inbound events.
     */
    public registerReceive(streamId: number, sink: ReceiveSink): void {
        this.receives.set(streamId, sink);
    }
    /**
     * Stop delivering inbound events for a stream.
     *
     * @param streamId - The stream to forget.
     */
    public unregisterReceive(streamId: number): void {
        this.receives.delete(streamId);
    }
    /**
     * Register a sink to receive a stream's STOP_SENDING notifications.
     *
     * @param streamId - The stream to observe.
     * @param sink - The receiver of that stream's send-side control events.
     */
    public registerSend(streamId: number, sink: SendSink): void {
        this.sends.set(streamId, sink);
    }
    /**
     * Stop delivering send-side control events for a stream.
     *
     * @param streamId - The stream to forget.
     */
    public unregisterSend(streamId: number): void {
        this.sends.delete(streamId);
    }
    /**
     * Set the handler invoked when the peer opens a stream. Replaces any prior handler.
     *
     * @param handler - The consumer of peer-initiated streams.
     */
    public setIncomingHandler(handler: IncomingHandler): void {
        this.incoming = handler;
    }
    /**
     * Set the sink invoked for each inbound datagram. Replaces any prior sink.
     *
     * @param sink - The consumer of inbound datagram payloads.
     */
    public setDatagramSink(sink: DatagramSink): void {
        this.datagramSink = sink;
    }

    /**
     * Record that one inbound datagram was dropped because the readable queue
     * was full. Surfaced through getStats() as `datagrams.droppedIncoming`.
     */
    public recordDroppedIncomingDatagram(): void {
        this.droppedIncomingDatagrams++;
    }

    /**
     * Route one native event to the appropriate promise, sink, or handler.
     *
     * @param ev - The event delivered by the native addon.
     * @remarks
     * This is the single entry point the native `onEvent` callback funnels into.
     * `ready`/`serverReady` mark the session established and resolve `ready`;
     * `closed`/`error` drive {@link SessionCore.finish}; the `stream*` events look
     * up the matching sink (silently ignored if none is registered); and the
     * `*Ack`/`streamOpened` events settle and then remove the pending deferred
     * for their `requestId`.
     */
    public dispatch(ev: NativeEvent): void {
        switch (ev.type) {
            case NativeEventType.Ready:
            case NativeEventType.ServerReady: {
                this.readyState = true;
                this.ready.resolve();
                break;
            }
            case NativeEventType.Draining: {
                this.draining.resolve();
                break;
            }
            case NativeEventType.Closed: {
                const reason = new TextDecoder().decode(ev.reason);
                this.finish({ closeCode: ev.code, reason }, undefined);
                break;
            }
            case NativeEventType.Error: {
                const err = new WebTransportError(ev.message, { source: 'session' });
                this.finish(null, err);
                break;
            }
            case NativeEventType.Datagram: {
                this.datagramSink?.(ev.data);
                break;
            }
            case NativeEventType.Stream: {
                if (ev.bidi) this.incoming?.onBidi(ev.streamId);
                else this.incoming?.onUni(ev.streamId);
                break;
            }
            case NativeEventType.StreamData: {
                this.receives.get(ev.streamId)?.onData(ev.data);
                break;
            }
            case NativeEventType.StreamFin: {
                this.receives.get(ev.streamId)?.onFin();
                break;
            }
            case NativeEventType.StreamReset: {
                this.receives.get(ev.streamId)?.onReset(ev.code);
                break;
            }
            case NativeEventType.StreamStopSending: {
                this.sends.get(ev.streamId)?.onStopSending(ev.code);
                break;
            }
            case NativeEventType.StreamOpened: {
                this.opens.get(ev.requestId)?.resolve(ev.streamId);
                this.opens.delete(ev.requestId);
                break;
            }
            case NativeEventType.WriteAck: {
                this.writes.get(ev.requestId)?.resolve();
                this.writes.delete(ev.requestId);
                break;
            }
            case NativeEventType.DatagramAck: {
                this.datagramAcks.get(ev.requestId)?.resolve(ev.sent);
                this.datagramAcks.delete(ev.requestId);
                break;
            }
            case NativeEventType.Stats: {
                const stats: WebTransportConnectionStats = {
                    bytesSent: ev.bytesSent,
                    bytesReceived: ev.bytesReceived,
                    packetsSent: ev.packetsSent,
                    packetsReceived: ev.packetsReceived,
                    packetsLost: ev.packetsLost,
                    smoothedRtt: ev.smoothedRtt,
                    rttVariation: ev.rttVariation,
                    minRtt: ev.minRtt,
                    datagrams: {
                        expiredOutgoing: 0,
                        droppedIncoming: this.droppedIncomingDatagrams,
                        lostOutgoing: 0,
                        expiredIncoming: 0,
                    },
                };
                this.lastStats = stats;
                this.statsRequests.get(ev.requestId)?.resolve(stats);
                this.statsRequests.delete(ev.requestId);
                break;
            }
            case NativeEventType.KeyingMaterial: {
                const d = this.keyingMaterialRequests.get(ev.requestId);
                if (d) {
                    if (ev.ok && ev.data) {
                        d.resolve(ev.data);
                    } else {
                        d.reject(
                            new WebTransportError('keying material export failed', {
                                source: 'session',
                            }),
                        );
                    }
                    this.keyingMaterialRequests.delete(ev.requestId);
                }
                break;
            }
        }
    }

    /**
     * Terminal transition: resolve/reject everything and tear down.
     *
     * @param info - Close info for a clean close, or null when closing on error.
     * @param error - The error that ended the session, or undefined for a clean close.
     * @remarks
     * Idempotent: returns immediately if the session already closed. Settles the
     * lifecycle promises according to state: if the session never became ready,
     * both `ready` and `closed` reject (a pre-ready close is a failed connect); if
     * it was ready and `error` is set, only `closed` rejects; otherwise `closed`
     * resolves with `info` (defaulting to code 0 / empty reason). It then rejects
     * every pending open and write, resolves every pending datagram ack to false,
     * signals a reset (code 0) to all receive sinks and STOP_SENDING (code 0) to
     * all send sinks, clears the bookkeeping maps, and finally shuts down the
     * transport.
     */
    private finish(info: WebTransportCloseInfo | null, error: WebTransportError | undefined): void {
        if (this.closedState) return;
        this.closedState = true;
        if (error) this.failedState = true;
        // A session that closes without a prior DRAIN never emitted `draining`;
        // settle it here so `await session.draining` cannot hang past close.
        this.draining.resolve();

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
        for (const d of this.statsRequests.values()) d.reject(err);
        for (const d of this.keyingMaterialRequests.values()) d.reject(err);
        for (const sink of this.receives.values()) sink.onReset(0);
        for (const sink of this.sends.values()) sink.onStopSending(0);
        this.opens.clear();
        this.writes.clear();
        this.datagramAcks.clear();
        this.statsRequests.clear();
        this.keyingMaterialRequests.clear();
        this.receives.clear();
        this.sends.clear();

        this.transport?.shutdown();
    }

    /** @returns Whether the session has reached its terminal closed state. */
    public get isClosed(): boolean {
        return this.closedState;
    }

    /** @returns Whether the session has been established (ready). */
    public get isReady(): boolean {
        return this.readyState;
    }
}

/**
 * Routes session commands through a native client session handle.
 *
 * @remarks
 * The client-side {@link SessionTransport}: each method forwards to the matching
 * {@link NativeAddon} function with the captured {@link NativeHandle}.
 * @internal
 */
class ClientTransport implements SessionTransport {
    /**
     * @param native - The loaded native addon.
     * @param handle - The client session handle these commands target.
     */
    public constructor(
        private readonly native: NativeAddon,
        private readonly handle: NativeHandle,
    ) {}
    /**
     * @param bidi - True for bidirectional, false for unidirectional.
     * @param requestId - Id correlating the eventual `streamOpened` event.
     */
    public openStream(bidi: boolean, requestId: number): void {
        this.native.openStream(this.handle, bidi, requestId);
    }
    /**
     * @param streamId - The target stream id.
     * @param bytes - The payload.
     * @param requestId - Id correlating the eventual `writeAck` event.
     */
    public write(streamId: number, bytes: Uint8Array, requestId: number): void {
        this.native.writeStream(this.handle, streamId, bytes, requestId);
    }
    /** @param streamId - The stream to finish. */
    public fin(streamId: number): void {
        this.native.finStream(this.handle, streamId);
    }
    /**
     * @param streamId - The stream to reset.
     * @param code - The application error code.
     */
    public reset(streamId: number, code: number): void {
        this.native.resetStream(this.handle, streamId, code);
    }
    /**
     * @param streamId - The stream whose inbound half should be stopped.
     * @param code - The application error code.
     */
    public stopSending(streamId: number, code: number): void {
        this.native.stopSending(this.handle, streamId, code);
    }
    /**
     * @param streamId - The receive stream.
     * @param paused - True to pause draining, false to resume.
     */
    public setPaused(streamId: number, paused: boolean): void {
        this.native.setPaused(this.handle, streamId, paused);
    }
    /**
     * @param bytes - The datagram payload.
     * @param requestId - Id correlating the eventual `datagramAck` event.
     */
    public sendDatagram(bytes: Uint8Array, requestId: number): void {
        this.native.sendDatagram(this.handle, bytes, requestId);
    }
    /** @returns The maximum datagram payload size, in bytes. */
    public maxDatagramSize(): number {
        return this.native.maxDatagramSize(this.handle);
    }
    /**
     * @param code - The WebTransport close code.
     * @param reason - The UTF-8 encoded close reason.
     */
    public closeSession(code: number, reason: Uint8Array): void {
        this.native.closeSession(this.handle, code, reason);
    }
    /** Send a DRAIN_WEBTRANSPORT_SESSION capsule to the peer. */
    public drain(): void {
        this.native.drain(this.handle);
    }
    /** Request connection stats; the result arrives as a `stats` event. */
    public getStats(requestId: number): void {
        this.native.getStats(this.handle, requestId);
    }
    /** Export TLS keying material; the result arrives as a `keyingMaterial` event. */
    public exportKeyingMaterial(
        requestId: number,
        label: Uint8Array,
        context: Uint8Array,
        length: number,
    ): void {
        this.native.exportKeyingMaterial(this.handle, requestId, label, context, length);
    }
    /** Tear down the session's native driver thread. */
    public shutdown(): void {
        this.native.shutdown(this.handle);
    }
}

/**
 * Create and connect a client-role session.
 *
 * @param config - The normalized connect parameters.
 * @returns A {@link SessionCore} wired to a fresh native client session; if the
 *   native `connect` throws during setup, the returned core is failed
 *   asynchronously via {@link SessionCore.failSetup} rather than throwing here.
 * @remarks
 * Loads the native addon, constructs the core, calls {@link NativeAddon.connect}
 * with an `onEvent` callback bound to {@link SessionCore.dispatch}, and attaches
 * a {@link ClientTransport} around the returned handle.
 * @internal
 */
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

/**
 * Routes session commands through a native server handle + session id.
 *
 * @remarks
 * The server-side {@link SessionTransport}: each method forwards to the matching
 * `server*` {@link NativeAddon} function with the captured
 * {@link NativeServerHandle} and numeric session id.
 * @internal
 */
export class ServerTransport implements SessionTransport {
    /**
     * @param native - The loaded native addon.
     * @param handle - The server handle these commands target.
     * @param session - The id of the session these commands act on.
     */
    public constructor(
        private readonly native: NativeAddon,
        private readonly handle: NativeServerHandle,
        private readonly session: number,
    ) {}
    /**
     * @param bidi - True for bidirectional, false for unidirectional.
     * @param requestId - Id correlating the eventual `streamOpened` event.
     */
    public openStream(bidi: boolean, requestId: number): void {
        this.native.serverOpenStream(this.handle, this.session, bidi, requestId);
    }
    /**
     * @param streamId - The target stream id.
     * @param bytes - The payload.
     * @param requestId - Id correlating the eventual `writeAck` event.
     */
    public write(streamId: number, bytes: Uint8Array, requestId: number): void {
        this.native.serverWrite(this.handle, this.session, streamId, bytes, requestId);
    }
    /** @param streamId - The stream to finish. */
    public fin(streamId: number): void {
        this.native.serverFin(this.handle, this.session, streamId);
    }
    /**
     * @param streamId - The stream to reset.
     * @param code - The application error code.
     */
    public reset(streamId: number, code: number): void {
        this.native.serverReset(this.handle, this.session, streamId, code);
    }
    /**
     * @param streamId - The stream whose inbound half should be stopped.
     * @param code - The application error code.
     */
    public stopSending(streamId: number, code: number): void {
        this.native.serverStopSending(this.handle, this.session, streamId, code);
    }
    /**
     * @param streamId - The receive stream.
     * @param paused - True to pause draining, false to resume.
     */
    public setPaused(streamId: number, paused: boolean): void {
        this.native.serverSetPaused(this.handle, this.session, streamId, paused);
    }
    /**
     * @param bytes - The datagram payload.
     * @param requestId - Id correlating the eventual `datagramAck` event.
     */
    public sendDatagram(bytes: Uint8Array, requestId: number): void {
        this.native.serverSendDatagram(this.handle, this.session, bytes, requestId);
    }
    /** @returns The maximum datagram payload size, in bytes. */
    public maxDatagramSize(): number {
        return this.native.serverMaxDatagramSize(this.handle);
    }
    /**
     * @param code - The WebTransport close code.
     * @param reason - The UTF-8 encoded close reason.
     */
    public closeSession(code: number, reason: Uint8Array): void {
        this.native.serverCloseSession(this.handle, this.session, code, reason);
    }
    /** Send a DRAIN_WEBTRANSPORT_SESSION capsule to this session's peer. */
    public drain(): void {
        this.native.serverDrain(this.handle, this.session);
    }
    /** Request this session's connection stats; the result arrives as a `stats` event. */
    public getStats(requestId: number): void {
        this.native.serverGetStats(this.handle, this.session, requestId);
    }
    /** Export TLS keying material; the result arrives as a `keyingMaterial` event. */
    public exportKeyingMaterial(
        requestId: number,
        label: Uint8Array,
        context: Uint8Array,
        length: number,
    ): void {
        this.native.serverExportKeyingMaterial(
            this.handle,
            this.session,
            requestId,
            label,
            context,
            length,
        );
    }
    /**
     * No-op: the server driver owns the session lifecycle, so there is nothing to
     * tear down per session on the JS side.
     */
    public shutdown(): void {
        // The server driver owns the session lifecycle; nothing to tear down here.
    }
}
