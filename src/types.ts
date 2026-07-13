// SPDX-License-Identifier: Apache-2.0
//! Public option and info types, mirroring the W3C WebTransport interface plus a
//! few Node-specific extensions.

/** A pinned server-certificate hash (currently only `sha-256`). */
export interface WebTransportHash {
  algorithm: string;
  value: BufferSource;
}

/** Congestion-control tuning hint. */
export type WebTransportCongestionControl = 'default' | 'throughput' | 'low-latency';

/** Options accepted by the {@link WebTransport} constructor. */
export interface WebTransportOptions {
  /**
   * Accept the server certificate by its SHA-256 fingerprint (self-signed /
   * pinned certificates), exactly like the browser API. When present, normal CA
   * and hostname validation is bypassed for the matching certificate.
   */
  serverCertificateHashes?: WebTransportHash[];
  /** Allow reusing a pooled QUIC connection (best-effort; currently informational). */
  allowPooling?: boolean;
  /** Require datagram support; reserved for parity with the spec. */
  requireUnreliable?: boolean;
  /** Congestion-control hint. */
  congestionControl?: WebTransportCongestionControl;

  // ---- Node-specific extensions ----
  /**
   * Disable ALL certificate verification. Development only — never use against a
   * server you do not control. Ignored when `serverCertificateHashes` is set.
   */
  insecure?: boolean;
  /** Additional request headers to send on the Extended CONNECT. */
  headers?: Record<string, string>;
  /** Value for the `origin` request header. */
  origin?: string;
}

/** Information about how a session closed. */
export interface WebTransportCloseInfo {
  closeCode: number;
  reason: string;
}

/** Options for {@link WebTransport.close}. */
export interface WebTransportCloseOptions {
  closeCode?: number;
  reason?: string;
}

/** Reliability mode of a send stream (spec parity). */
export type WebTransportSendGroup = unknown;
