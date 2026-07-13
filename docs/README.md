# rwebtransport documentation

`rwebtransport` is a native [WebTransport](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport) **client and server** for Node.js: the same `WebTransport` API you use in Chrome, backed by a real HTTP/3 endpoint speaking QUIC over UDP (Cloudflare's `quiche` transport and BoringSSL, bound to Node through neon). You get bidirectional and unidirectional streams as WHATWG `ReadableStream`/`WritableStream`, unreliable datagrams, and a matching `WebTransportServer`, all over one multiplexed QUIC connection. It runs on **Node 24 and Node 26 only**, ships as both ESM and CJS, and installs with prebuilt binaries (falling back to compiling the Rust core from source). This folder is the full guide; each page below covers one part of the surface in depth.

## Pages

- **[getting-started.md](./getting-started.md)** Install the package, open your first session, and run a minimal client and server end to end.
- **[client.md](./client.md)** The `WebTransport` client class: constructor and `WebTransportOptions`, the `ready`/`closed` promises, opening streams, and closing a session.
- **[server.md](./server.md)** The `WebTransportServer` class: binding a port with a cert and key, consuming `incomingSessions`, and the per-request `WebTransportServerSession` metadata.
- **[streams.md](./streams.md)** Bidirectional and unidirectional streams, reading and writing with `getReader()`/`getWriter()`, backpressure, and cancel/abort (STOP_SENDING and RESET_STREAM).
- **[datagrams.md](./datagrams.md)** The `WebTransportDatagramDuplexStream`: sending and receiving unreliable, unordered datagrams, `maxDatagramSize`, high-water marks, and max age.
- **[certificates.md](./certificates.md)** TLS trust models: full PKI validation, pinning a self-signed cert with `serverCertificateHashes`, the `insecure` dev option, and generating server certs.
- **[errors.md](./errors.md)** The `WebTransportError` type, its `source` and `streamErrorCode` fields, and how connect failures, stream resets, and session termination surface.
- **[threading.md](./threading.md)** The per-session background-thread model, non-blocking hand-off to the event loop, `worker_threads` support, and `cluster` caveats.
- **[building.md](./building.md)** Building the native addon and TypeScript layer from source: toolchain requirements and the `build`, `build:rust`, `build:rust:debug`, and `build:ts` scripts.
- **[troubleshooting.md](./troubleshooting.md)** Common failures and fixes: unsupported Node versions, cert and hostname errors, 4xx CONNECT rejections, wedged streams, and dropped datagrams.

## See also

- **[../README.md](../README.md)** The repository README: overview, quick start, the why, and the architecture diagram.
- **[../examples](../examples)** Runnable client and server programs and a one-file end-to-end demo.
