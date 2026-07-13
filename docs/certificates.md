# Certificates and TLS

WebTransport runs over HTTP/3, which runs over QUIC, which mandates **TLS 1.3**. There is no TLS 1.2 fallback and no plaintext mode: every session is encrypted and the server always presents a certificate. This page covers what `rwebtransport` puts on the wire (the default Chrome TLS profile), the three ways the client can decide to trust a server certificate, how the server loads its certificate and key, and how to generate a development certificate.

For the option shapes referenced here see [client.md](./client.md) (`WebTransportOptions`) and [server.md](./server.md) (`WebTransportServerOptions`). For how a failed handshake surfaces see [errors.md](./errors.md).

## The default TLS profile (Chrome fingerprint)

By default the client puts **Chrome on the wire**. Because QUIC pins the TLS version at 1.3, the parts of the ClientHello a server can fingerprint are the named-group preference, the signature-algorithm list, GREASE, and extension ordering. `rwebtransport` mirrors Chrome's choices so the handshake is browser-indistinguishable, which matters for servers (Cloudflare in particular) that fingerprint clients.

The profile is fixed and applies to every client connection regardless of the verification mode you pick:

| Parameter             | Value                                                                                                                                                                                                 |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| TLS version           | 1.3 only (min and max both TLS 1.3)                                                                                                                                                                   |
| ALPN                  | `h3`                                                                                                                                                                                                  |
| Named groups (curves) | `X25519`, `P-256` (secp256r1), `P-384` (secp384r1), in that order                                                                                                                                     |
| Signature algorithms  | `ecdsa_secp256r1_sha256`, `rsa_pss_rsae_sha256`, `rsa_pkcs1_sha256`, `ecdsa_secp384r1_sha384`, `rsa_pss_rsae_sha384`, `rsa_pkcs1_sha384`, `rsa_pss_rsae_sha512`, `rsa_pkcs1_sha512`, `rsa_pkcs1_sha1` |
| Cipher suites         | `TLS_AES_128_GCM_SHA256`, `TLS_AES_256_GCM_SHA384`, `TLS_CHACHA20_POLY1305_SHA256` (BoringSSL's fixed TLS 1.3 order, which already matches Chrome)                                                    |
| ClientHello shaping   | GREASE enabled, extensions permuted                                                                                                                                                                   |

The `congestionControl`, `allowPooling`, and `requireUnreliable` options are accepted for spec parity but do not currently change the profile or the transport.

## The three verification modes

The client decides whether to trust the server certificate in exactly one of three ways. The mode is chosen by which options you pass, with this precedence:

1. If `serverCertificateHashes` is set (non-empty), **hash pinning** is used.
2. Otherwise, if `insecure: true`, **all verification is disabled**.
3. Otherwise, **full PKI validation** is used (the default).

Because pinning wins over `insecure`, setting `insecure: true` alongside `serverCertificateHashes` has no effect: the pin is still enforced.

| Mode         | Option                    | CA chain checked | Hostname checked | Use for                                     |
| ------------ | ------------------------- | ---------------- | ---------------- | ------------------------------------------- |
| Default PKI  | (none)                    | yes              | yes              | public servers with a CA-issued certificate |
| Hash pinning | `serverCertificateHashes` | no               | no               | self-signed or pinned certificates          |
| Insecure     | `insecure: true`          | no               | no               | local development only                      |

### Default: full PKI plus hostname validation

When you pass neither `serverCertificateHashes` nor `insecure`, the client performs standard public-key infrastructure validation: it builds and verifies the certificate chain against the system trust store and checks that the certificate is valid for the hostname in the URL. On Linux the trust anchors are read from `/etc/ssl/certs`. The hostname check compares the host in your `https://host:port/path` URL against the certificate's subjectAltName, exactly like a browser.

```ts
import { WebTransport } from 'rwebtransport';

// No cert options: full CA + hostname validation.
const wt = new WebTransport('https://example.com:4433/wt');
await wt.ready; // rejects with a WebTransportError if the chain or hostname fails to validate
```

If validation fails (untrusted issuer, expired certificate, or a hostname mismatch), `ready` rejects and `closed` rejects with a `WebTransportError` whose `source` is `"session"`. See [errors.md](./errors.md).

### Pinning with `serverCertificateHashes`

To trust a specific certificate without a CA, pin it by the **SHA-256 hash of its DER encoding**. This mirrors the browser's `serverCertificateHashes` option: no CA chain is required and the hostname is not checked, so you can connect to `https://127.0.0.1:4433/` even if the certificate names `localhost`. Only the matching leaf certificate is accepted.

The option is an array of `{ algorithm: 'sha-256'; value: BufferSource }`. Only `sha-256` is supported (any other algorithm throws), and `value` must be exactly 32 bytes (a wrong length throws).

Compute the hash from a PEM certificate file with `node:crypto`. `X509Certificate.raw` is the DER of the leaf certificate, and its SHA-256 digest is what the option expects:

```ts
import { readFileSync } from 'node:fs';
import { X509Certificate, createHash } from 'node:crypto';
import { WebTransport } from 'rwebtransport';

// SHA-256 of the certificate's DER encoding (a 32-byte value).
const der = new X509Certificate(readFileSync('cert.pem')).raw;
const value = new Uint8Array(createHash('sha256').update(der).digest());

const wt = new WebTransport('https://127.0.0.1:4433/echo', {
    serverCertificateHashes: [{ algorithm: 'sha-256', value }],
});
await wt.ready; // resolves only if the server's leaf cert hashes to `value`
```

You can pass more than one entry to pin during a certificate rotation (accept the old and the new fingerprint at once). If the presented certificate matches none of them, the handshake fails and `ready` rejects.

The same fingerprint can be read straight from the certificate file with `openssl`, which is handy for logging or for pinning from another language:

```sh
openssl x509 -in cert.pem -outform DER | openssl dgst -sha256
```

### Insecure (development only)

`insecure: true` disables **all** certificate verification: no chain, no hostname, no expiry. Use it only against a server you control on your own machine. Never use it against a server on the public internet, since it accepts any certificate and defeats the point of TLS.

```ts
import { WebTransport } from 'rwebtransport';

const wt = new WebTransport('https://localhost:4433/wt', { insecure: true });
await wt.ready;
```

Prefer `serverCertificateHashes` over `insecure` even in development: pinning still authenticates the exact certificate, so a stray process on the same port cannot impersonate your server.

## Server certificate and key

The server loads a certificate and private key as **PEM file paths** through `WebTransportServerOptions`:

```ts
import { WebTransportServer } from 'rwebtransport';

const server = new WebTransportServer({
    port: 4433,
    host: '127.0.0.1',
    cert: './cert.pem', // PEM certificate chain, a FILE PATH (not the PEM text)
    key: './key.pem', // PEM private key, a FILE PATH (not the PEM text)
});
await server.ready;
console.log('listening on', server.port);
```

Both `cert` and `key` are paths on disk, not inline PEM strings. `cert` may contain a full chain (leaf first, then intermediates). The server also speaks TLS 1.3 only with ALPN `h3`. If either file is missing, malformed, or the key does not match the certificate, `server.ready` rejects with a `WebTransportError`. See [server.md](./server.md) for consuming `incomingSessions`.

## Generating a development certificate with openssl

A self-signed EC (P-256) certificate is enough for local development and for pinning. The repository ships a ready-made script at [../examples/generate-cert.sh](../examples/generate-cert.sh); the commands it runs are:

```sh
# 1. Private key: EC on the prime256v1 (P-256) curve.
openssl ecparam -name prime256v1 -genkey -noout -out key.pem

# 2. Self-signed certificate. The subjectAltName is what a hostname-checking
#    client (default PKI mode) validates against, so include every name and IP
#    you will connect to.
openssl req -new -x509 \
  -key key.pem \
  -out cert.pem \
  -days 90 \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1"

# 3. Print the DER SHA-256 fingerprint to pin via serverCertificateHashes.
openssl x509 -in cert.pem -outform DER | openssl dgst -sha256
```

Point the server at `cert.pem` and `key.pem`, then have the client either pin the printed fingerprint (works from any host or IP, since hostname is not checked) or use the default PKI mode against `https://localhost:PORT/` (the subjectAltName above makes the hostname check pass, though a self-signed certificate is not in the system trust store, so plain PKI mode still rejects it unless you install it as a local trust anchor).

## The 14-day validity rule for pinned certificates

Real browsers accept a certificate pinned through `serverCertificateHashes` **only if its validity period is at most 14 days** (measured from `notBefore` to `notAfter`). This is a deliberate limit in the WebTransport specification: a pinned, un-revocable certificate should be short-lived.

`rwebtransport` does **not** enforce this limit. The client will accept a matching pinned certificate with any validity period, which is why the development script above can issue a 90-day certificate and still pin it. If you intend the same server and pin to also be reachable from a browser, keep the certificate's validity window to 14 days or less and rotate it on a schedule, or the browser will refuse it even though this Node client accepts it.

## See also

- [client.md](./client.md) The `WebTransport` client, `WebTransportOptions`, and the `ready`/`closed` promises.
- [server.md](./server.md) Binding a `WebTransportServer` with a certificate and key.
- [errors.md](./errors.md) How a rejected certificate or a failed handshake surfaces as a `WebTransportError`.
- [troubleshooting.md](./troubleshooting.md) Diagnosing certificate and hostname failures.
- [../examples/generate-cert.sh](../examples/generate-cert.sh) The self-signed development certificate generator.
