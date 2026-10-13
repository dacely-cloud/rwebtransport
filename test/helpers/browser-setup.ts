// SPDX-License-Identifier: Apache-2.0
import { resolve } from 'node:path';
import type { TestProject } from 'vitest/node';
import { WebTransportServer, type WebTransportServerSession } from '../../src/index.js';
import setupCertificate from './global-setup.js';
import { certHash } from './echo-server.js';

function echo(session: WebTransportServerSession): void {
    void session.incomingUnidirectionalStreams.cancel();
    void (async () => {
        const reader = session.incomingBidirectionalStreams.getReader();
        for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            if (value) void value.readable.pipeTo(value.writable).catch(() => undefined);
        }
    })().catch(() => undefined);
    void session.datagrams.readable.pipeTo(session.datagrams.writable).catch(() => undefined);
}

export default async function setup(project: TestProject): Promise<() => Promise<void>> {
    setupCertificate();
    const server = new WebTransportServer({
        host: '127.0.0.1',
        port: 0,
        cert: resolve('test/fixtures/cert.pem'),
        key: resolve('test/fixtures/key.pem'),
    });
    await server.ready;
    project.provide('webtransportFixture', {
        url: `https://127.0.0.1:${server.port}/echo`,
        hash: Array.from(certHash()),
    });
    void (async () => {
        const reader = server.incomingSessions.getReader();
        for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            if (value) echo(value);
        }
    })().catch(() => undefined);
    return async () => {
        server.close();
        await server.closed;
    };
}

declare module 'vitest' {
    interface ProvidedContext {
        webtransportFixture: { url: string; hash: number[] };
    }
}
