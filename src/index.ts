// SPDX-License-Identifier: Apache-2.0
//! rwebtransport — a fully-compatible WebTransport client for Node.js.
//!
//! ```ts
//! import { WebTransport } from 'rwebtransport';
//! const wt = new WebTransport('https://example.com:4433/echo');
//! await wt.ready;
//! ```

export { WebTransport, WebTransportSession } from './webtransport.js';
export {
    WebTransportServer,
    WebTransportServerSession,
    type WebTransportServerOptions,
    type WebTransportSessionRequest,
} from './server.js';
export { WebTransportError } from './errors.js';
export {
    WebTransportBidirectionalStream,
    WebTransportReceiveStream,
    WebTransportSendStream,
} from './streams.js';
export { WebTransportDatagramDuplexStream } from './datagrams.js';

export type {
    WebTransportOptions,
    WebTransportHash,
    WebTransportCongestionControl,
    WebTransportCloseInfo,
    WebTransportCloseOptions,
    BinarySource,
} from './types.js';
export type { WebTransportErrorSource, WebTransportErrorOptions } from './errors.js';
