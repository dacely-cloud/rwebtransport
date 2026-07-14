// SPDX-License-Identifier: Apache-2.0
//! Public option and info types, mirroring the W3C WebTransport interface plus a
//! few Node-specific extensions.

/**
 * Any byte source accepted by the API: a typed array / DataView, or a raw
 * `ArrayBuffer`. Broader than the DOM `BufferSource` so ordinary Node `Buffer`s
 * and `Uint8Array`s (backed by `ArrayBufferLike`) are accepted without casting.
 *
 * @remarks
 * Values of this type are normalized to a `Uint8Array` before crossing into the
 * native layer: an `ArrayBuffer` is wrapped whole, while an `ArrayBufferView`
 * (typed array or `DataView`) is viewed over its exact `byteOffset` and
 * `byteLength` so only the meaningful region is used.
 */
export type BinarySource = ArrayBufferView | ArrayBuffer;

/** A pinned server-certificate hash (currently only `sha-256`). */
export interface WebTransportHash {
    /**
     * Hash algorithm name. Compared case-insensitively; only `sha-256` is
     * supported and any other value makes the {@link WebTransport} constructor
     * throw a `WebTransportError`.
     */
    algorithm: string;
    /**
     * The raw digest bytes to pin against. For `sha-256` this must be exactly 32
     * bytes once normalized, otherwise the constructor throws a
     * `WebTransportError`.
     */
    value: BinarySource;
}

/**
 * Congestion-control tuning hint.
 *
 * @remarks
 * Accepted for spec parity but not currently applied by the native transport.
 */
export type WebTransportCongestionControl = 'default' | 'throughput' | 'low-latency';

/**
 * The reliability modes a session offers. `'pending'` before it is established,
 * `'reliable-only'` for a streams-only transport, and `'supports-unreliable'`
 * when datagrams are available too (always the case for this HTTP/3 transport).
 */
export type WebTransportReliabilityMode = 'pending' | 'reliable-only' | 'supports-unreliable';

/** Options accepted by the {@link WebTransport} constructor. */
export interface WebTransportOptions {
    /**
     * Accept the server certificate by its SHA-256 fingerprint (self-signed /
     * pinned certificates), exactly like the browser API. When present, normal CA
     * and hostname validation is bypassed for the matching certificate.
     *
     * @remarks
     * Each entry must use algorithm `sha-256` and carry a 32-byte value, or the
     * constructor throws. When set, this takes precedence over `insecure`.
     */
    serverCertificateHashes?: WebTransportHash[];
    /**
     * Allow reusing a pooled QUIC connection (best-effort; currently
     * informational).
     *
     * @remarks
     * Accepted for spec parity; not currently acted on by the transport.
     */
    allowPooling?: boolean;
    /**
     * Require datagram support; reserved for parity with the spec.
     *
     * @remarks
     * Accepted for spec parity; not currently enforced.
     */
    requireUnreliable?: boolean;
    /**
     * Congestion-control hint.
     *
     * @remarks
     * Accepted for spec parity; not currently applied by the transport.
     */
    congestionControl?: WebTransportCongestionControl;

    /**
     * Disable ALL certificate verification. Development only, never use against a
     * server you do not control. Ignored when `serverCertificateHashes` is set.
     *
     * @defaultValue `false`
     */
    insecure?: boolean;
    /**
     * Additional request headers to send on the Extended CONNECT.
     *
     * @remarks
     * Each key/value pair is forwarded to the native `connect` call as parallel
     * header-name and header-value arrays.
     */
    headers?: Record<string, string>;
    /**
     * Value for the `origin` request header.
     *
     * @defaultValue `null` (no `origin` header is added)
     */
    origin?: string;
}

/**
 * Information about how a session closed.
 *
 * @remarks
 * This is the value the `WebTransport.closed` promise resolves to on a clean
 * shutdown.
 */
export interface WebTransportCloseInfo {
    /** Application-defined close code reported by (or sent to) the peer. */
    closeCode: number;
    /** UTF-8 close reason string; empty when none was provided. */
    reason: string;
}

/** Options for {@link WebTransport.close}. */
export interface WebTransportCloseOptions {
    /**
     * Application-defined close code to send to the peer.
     *
     * @defaultValue `0`
     */
    closeCode?: number;
    /**
     * Human-readable close reason, encoded as UTF-8 and sent to the peer.
     *
     * @defaultValue `''` (empty string)
     */
    reason?: string;
}

/**
 * Grouping handle that schedules the sends of its member streams relative to
 * one another (spec parity).
 *
 * @remarks
 * Placeholder for the W3C `WebTransportSendGroup` concept; typed as `unknown`
 * because it is not yet modeled or used by this implementation.
 */
export type WebTransportSendGroup = unknown;
