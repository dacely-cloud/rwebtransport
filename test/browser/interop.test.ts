// SPDX-License-Identifier: Apache-2.0
import { expect, inject, it } from 'vitest';

it('Chrome exchanges streams and datagrams with the native server', async () => {
    const { url, hash } = inject('webtransportFixture');
    const transport = new WebTransport(url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: new Uint8Array(hash) }],
    });
    void transport.closed.catch(() => undefined);
    try {
        await transport.ready;
        const stream = await transport.createBidirectionalStream();
        const payload = new TextEncoder().encode('browser WebTransport echo');
        const response = new Response(stream.readable).arrayBuffer();
        const writer = stream.writable.getWriter();
        await writer.write(payload);
        await writer.close();
        expect(new Uint8Array(await response)).toEqual(payload);

        const datagramReader = transport.datagrams.readable.getReader();
        const datagramWriter = transport.datagrams.writable.getWriter();
        const received = datagramReader.read();
        await datagramWriter.write(payload);
        expect((await received).value).toEqual(payload);
        datagramReader.releaseLock();
        datagramWriter.releaseLock();
        transport.close({ closeCode: 7, reason: 'browser complete' });
        await transport.closed;
    } finally {
        transport.close();
    }
});
