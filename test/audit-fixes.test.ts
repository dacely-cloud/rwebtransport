// SPDX-License-Identifier: Apache-2.0
import { createSocket, type RemoteInfo } from 'node:dgram';
import { resolve } from 'node:path';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
    WebTransport,
    WebTransportError,
    WebTransportSendStream,
    WebTransportServer,
} from '../src/index.js';
import { NativeEventType, SessionCore, type SessionTransport } from '../src/native.js';
import { certHash } from './helpers/echo-server.js';

const servers: WebTransportServer[] = [];
const clients: WebTransport[] = [];
afterEach(() => {
    for (const client of clients) client.close();
    for (const server of servers) server.close();
    clients.length = 0;
    servers.length = 0;
});

async function server(host = '127.0.0.1'): Promise<WebTransportServer> {
    const value = new WebTransportServer({
        host,
        port: 0,
        cert: resolve('test/fixtures/cert.pem'),
        key: resolve('test/fixtures/key.pem'),
    });
    servers.push(value);
    await value.ready;
    return value;
}

function client(url: string): WebTransport {
    const value = new WebTransport(url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash() }],
    });
    clients.push(value);
    return value;
}

function coreWithPendingWrites() {
    const transport = {
        openStream: vi.fn(),
        write: vi.fn<(streamId: number, chunk: Uint8Array, requestId: number) => void>(),
        fin: vi.fn(),
        reset: vi.fn(),
        stopSending: vi.fn(),
        setPaused: vi.fn(),
        sendDatagram: vi.fn(),
        maxDatagramSize: () => 1200,
        closeSession: vi.fn(),
        drain: vi.fn(),
        getStats: vi.fn(),
        exportKeyingMaterial: vi.fn(),
        shutdown: vi.fn(),
    } satisfies SessionTransport;
    const core = new SessionCore();
    core.attach(transport);
    core.dispatch({ type: NativeEventType.Ready, protocol: null, responseHeaders: [] });
    return { core, transport };
}

describe('Audit regressions', () => {
    it('aborting a pending sink write resets it immediately and leaves other streams pending', async () => {
        const { core, transport } = coreWithPendingWrites();
        const writer = new WebTransportSendStream(core, 4).getWriter();
        const other = core.write(8, new Uint8Array([2]));
        const write = writer.write(new Uint8Array([1]));
        const rejected = expect(write).rejects.toThrow('stream aborted');
        await Promise.resolve();
        await writer.abort(
            new WebTransportError('cancelled', { source: 'stream', streamErrorCode: 42 }),
        );
        await rejected;
        expect(transport.reset).toHaveBeenCalledExactlyOnceWith(4, 42);
        let otherSettled = false;
        void other.then(
            () => {
                otherSettled = true;
            },
            () => {
                otherSettled = true;
            },
        );
        await Promise.resolve();
        expect(otherSettled).toBe(false);
        core.dispatch({
            type: NativeEventType.WriteAck,
            requestId: transport.write.mock.calls[0]![2],
        });
        await other;
    });

    it.each([-1, 0, 1.5, 4097, 2 ** 32, Number.NaN, Number.POSITIVE_INFINITY])(
        'rejects invalid exporter length %s before invoking native code',
        async (length) => {
            const { core, transport } = coreWithPendingWrites();
            await expect(
                core.exportKeyingMaterial(
                    new TextEncoder().encode('test'),
                    new Uint8Array(),
                    length,
                ),
            ).rejects.toBeInstanceOf(RangeError);
            expect(transport.exportKeyingMaterial).not.toHaveBeenCalled();
        },
    );

    it.each([
        'http://localhost/',
        'https://localhost/#',
        'https://localhost/#fragment',
        'https://user:password@localhost/',
    ])('rejects unsupported URL %s', (url) => {
        expect(() => new WebTransport(url)).toThrow();
    });

    it.each([
        { protocols: [''] },
        { protocols: ['same', 'same'] },
        { protocols: ['é'] },
        { protocols: ['x'.repeat(513)] },
    ])('rejects invalid offered protocols %j', ({ protocols }) => {
        expect(() => new WebTransport('https://localhost/', { protocols })).toThrow();
    });

    it('canonicalizes a query without a slash and negotiates a comma-containing protocol', async () => {
        const srv = new WebTransportServer({
            host: '127.0.0.1',
            port: 0,
            cert: resolve('test/fixtures/cert.pem'),
            key: resolve('test/fixtures/key.pem'),
            supportedProtocols: ['a,b'],
        });
        servers.push(srv);
        await srv.ready;
        const incoming = srv.incomingSessions.getReader().read();
        const conn = new WebTransport(`https://127.0.0.1:${srv.port}?query=1`, {
            protocols: ['a,b'],
            serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash() }],
        });
        clients.push(conn);
        await conn.ready;
        expect((await incoming).value?.path).toBe('/?query=1');
        expect(conn.protocol).toBe('a,b');
    });

    it.each(['::1', '[::1]'])('binds the IPv6 host %s', async (host) => {
        const srv = await server(host);
        const incoming = srv.incomingSessions.getReader().read();
        const conn = client(`https://[::1]:${srv.port}/`);
        await conn.ready;
        expect((await incoming).value?.remoteAddress).toBe('::1');
    });

    it('bounds unread finished sessions and resumes admission after the application drains them', async () => {
        const srv = await server();
        for (let i = 0; i < 132; i++) {
            const conn = client(`https://127.0.0.1:${srv.port}/`);
            await conn.ready;
            if (i < 128) conn.close();
            await conn.closed;
        }
        const reader = srv.incomingSessions.getReader();
        for (let i = 0; i < 128; i++) expect((await reader.read()).value).toBeDefined();
        const incoming = reader.read();
        const conn = client(`https://127.0.0.1:${srv.port}/`);
        await conn.ready;
        expect((await incoming).value).toBeDefined();
    });

    it('cancelling session admission closes newly established peers', async () => {
        const srv = await server();
        await srv.incomingSessions.cancel();
        const conn = client(`https://127.0.0.1:${srv.port}/`);
        await conn.ready;
        await expect(conn.closed).resolves.toMatchObject({ closeCode: 0 });
    });

    it('a real backpressured write and abort both settle without the receiver draining', async () => {
        const srv = await server();
        const incoming = srv.incomingSessions.getReader().read();
        const conn = client(`https://127.0.0.1:${srv.port}/`);
        await conn.ready;
        const peer = (await incoming).value!;
        const stream = await conn.createBidirectionalStream();
        const receiving = await peer.incomingBidirectionalStreams.getReader().read();
        expect(receiving.value).toBeDefined();
        const writer = stream.writable.getWriter();
        let settled = false;
        const write = writer.write(new Uint8Array(8 * 1024 * 1024));
        const rejected = expect(write).rejects.toThrow('stream aborted');
        void write.then(
            () => {
                settled = true;
            },
            () => {
                settled = true;
            },
        );
        await new Promise((resolve) => setTimeout(resolve, 100));
        expect(settled).toBe(false);
        await writer.abort();
        await rejected;
    }, 5000);

    it('ignores unauthenticated empty UDP packets from another source', async () => {
        const srv = await server();
        const proxy = createSocket('udp4');
        const attacker = createSocket('udp4');
        let clientAddress: RemoteInfo | undefined;
        proxy.on('message', (data, from) => {
            if (from.port === srv.port) {
                if (clientAddress) proxy.send(data, clientAddress.port, clientAddress.address);
            } else {
                clientAddress = from;
                proxy.send(data, srv.port, '127.0.0.1');
            }
        });
        await new Promise<void>((resolve) => proxy.bind(0, '127.0.0.1', resolve));
        try {
            const incoming = srv.incomingSessions.getReader().read();
            const conn = client(`https://127.0.0.1:${proxy.address().port}/`);
            await conn.ready;
            const peer = (await incoming).value!;
            const receiving = peer.incomingBidirectionalStreams.getReader().read();
            expect(clientAddress).toBeDefined();
            await new Promise<void>((resolve, reject) =>
                attacker.send(
                    new Uint8Array(),
                    clientAddress!.port,
                    clientAddress!.address,
                    (error) => (error ? reject(error) : resolve()),
                ),
            );
            await new Promise((resolve) => setTimeout(resolve, 50));
            const stream = await conn.createBidirectionalStream();
            const writer = stream.writable.getWriter();
            await writer.write(new Uint8Array([1, 2, 3]));
            await writer.close();
            expect((await receiving).value).toBeDefined();
            conn.close();
            await conn.closed;
        } finally {
            proxy.close();
            attacker.close();
        }
    });
});
