// SPDX-License-Identifier: Apache-2.0
import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';
import { describe, expect, it, vi } from 'vitest';
import { NativeEventType, SessionCore, type SessionTransport } from '../src/native.js';
import { WebTransportSession } from '../src/webtransport.js';
import { WebTransport, WebTransportServer } from '../src/index.js';

function session() {
    const transport = {
        openStream: vi.fn(),
        write: vi.fn(),
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
    const api = new WebTransportSession(core);
    core.dispatch({ type: NativeEventType.Ready, protocol: null, responseHeaders: [] });
    return { core, api, transport };
}

describe('Incoming stream queue limits', () => {
    it.each([false, true])('terminates an unread direction (bidi=%s)', async (bidi) => {
        const { core, api, transport } = session();
        const register = vi.spyOn(core, 'registerReceive');
        for (let i = 0; i < 1024; i++) {
            const streamId = 4 * i + (bidi ? 4 : 14);
            core.dispatch({ type: NativeEventType.Stream, streamId, bidi });
            core.dispatch({ type: NativeEventType.StreamData, streamId, data: new Uint8Array(8) });
            core.dispatch({ type: NativeEventType.StreamFin, streamId });
            if (i === 127) expect(core.isClosed).toBe(false);
            if (i === 128) expect(core.isClosed).toBe(true);
        }
        expect(core.isClosed).toBe(true);
        await expect(api.closed).rejects.toThrow('incoming stream queue limit');
        expect(transport.closeSession).toHaveBeenCalledTimes(1);
        expect(register).toHaveBeenCalledTimes(128);
        const incoming = bidi
            ? api.incomingBidirectionalStreams
            : api.incomingUnidirectionalStreams;
        await expect(incoming.getReader().read()).rejects.toThrow('incoming stream queue limit');
    });

    it.each([false, true])(
        'allows cumulative stream churn when drained (bidi=%s)',
        async (bidi) => {
            const { core, api } = session();
            const incoming = bidi
                ? api.incomingBidirectionalStreams
                : api.incomingUnidirectionalStreams;
            const reader = incoming.getReader();
            for (let i = 0; i < 1024; i++) {
                const streamId = 4 * i + (bidi ? 4 : 14);
                core.dispatch({ type: NativeEventType.Stream, streamId, bidi });
                const { value } = await reader.read();
                expect(
                    value && ('readable' in value ? value.readable.streamId : value.streamId),
                ).toBe(streamId);
                core.dispatch({ type: NativeEventType.StreamFin, streamId });
                if (bidi)
                    core.dispatch({ type: NativeEventType.StreamStopSending, streamId, code: 0 });
            }
            expect(core.isClosed).toBe(false);
            core.dispatch({
                type: NativeEventType.Closed,
                code: 0,
                reason: new Uint8Array(),
                remote: false,
            });
        },
    );

    it.each([false, true])(
        'rejects new streams after direction cancellation (bidi=%s)',
        async (bidi) => {
            const { core, api, transport } = session();
            const register = vi.spyOn(core, 'registerReceive');
            const incoming = bidi
                ? api.incomingBidirectionalStreams
                : api.incomingUnidirectionalStreams;
            await incoming.cancel();
            for (let i = 0; i < 1024; i++) {
                core.dispatch({ type: NativeEventType.Stream, streamId: i * 4 + 14, bidi });
            }
            expect(transport.stopSending).toHaveBeenCalledTimes(1024);
            expect(transport.reset).toHaveBeenCalledTimes(bidi ? 1024 : 0);
            expect(register).not.toHaveBeenCalled();
            expect(core.isClosed).toBe(false);
            core.dispatch({
                type: NativeEventType.Closed,
                code: 0,
                reason: new Uint8Array(),
                remote: false,
            });
        },
    );

    it.each([false, true])(
        'isolates a real peer flooding an unread direction (bidi=%s)',
        async (bidi) => {
            const server = new WebTransportServer({
                host: '127.0.0.1',
                port: 0,
                cert: resolve('test/fixtures/cert.pem'),
                key: resolve('test/fixtures/key.pem'),
            });
            const clients: WebTransport[] = [];
            try {
                await server.ready;
                const reader = server.incomingSessions.getReader();
                const url = `https://127.0.0.1:${server.port}/flood`;
                const client = new WebTransport(url, { insecure: true });
                clients.push(client);
                await client.ready;
                const { value: accepted } = await reader.read();
                expect(accepted).toBeDefined();
                const failure = accepted!.closed.catch((error: unknown) => error);
                for (let i = 0; i < 512; i++) {
                    try {
                        const stream = bidi
                            ? (await client.createBidirectionalStream()).writable
                            : await client.createUnidirectionalStream();
                        const writer = stream.getWriter();
                        await writer.write(new Uint8Array(8192));
                        await writer.close();
                    } catch {
                        break;
                    }
                }
                expect(await failure).toBeInstanceOf(Error);
                expect(((await failure) as Error).message).toContain('incoming stream queue limit');
                const control = new WebTransport(url, { insecure: true });
                clients.push(control);
                await control.ready;
                expect((await reader.read()).value).toBeDefined();
            } finally {
                for (const client of clients) client.close();
                server.close();
                await server.closed;
            }
        },
    );
});

function varint(value: number): Uint8Array {
    if (value < 64) return Uint8Array.of(value);
    if (value < 16384) return Uint8Array.of(0x40 | (value >>> 8), value & 255);
    return Uint8Array.of(
        0x80 | (value >>> 24),
        (value >>> 16) & 255,
        (value >>> 8) & 255,
        value & 255,
    );
}

describe('Aggregate parser limits', () => {
    it.each(['partial-capsule', 'partial-frame', 'complete'])(
        'bounds aggregate parsers (%s)',
        async (mode) => {
            const complete = mode === 'complete';
            const server = new WebTransportServer({
                host: '127.0.0.1',
                port: 0,
                cert: resolve('test/fixtures/cert.pem'),
                key: resolve('test/fixtures/key.pem'),
            });
            const clients: WebTransport[] = [];
            let failed = 0;
            let firstFailure!: (error: unknown) => void;
            const failure = new Promise<unknown>((resolve) => {
                firstFailure = resolve;
            });
            // Matched traffic: an incomplete capsule in a complete DATA frame, an
            // incomplete DATA frame, or a complete capsule. All declared lengths
            // fit the old per-frame/capsule limits and carry the same payload bytes.
            const payload = new Uint8Array(512 * 1024);
            payload.set(varint(0x42));
            payload.set(varint(complete ? payload.length - 6 : 1024 * 1024), 2);
            const length = varint(mode === 'partial-frame' ? 1024 * 1024 : payload.length);
            const frame = new Uint8Array(1 + length.length + payload.length);
            frame[0] = 0; // HTTP/3 DATA
            frame.set(length, 1);
            frame.set(payload, 1 + length.length);
            try {
                await server.ready;
                const reader = server.incomingSessions.getReader();
                const url = `https://127.0.0.1:${server.port}/parser-limit`;
                for (let i = 0; i < 160; i++) {
                    const client = new WebTransport(url, { insecure: true });
                    clients.push(client);
                    await client.ready;
                    const { value: accepted } = await reader.read();
                    void accepted!.closed.catch((error: unknown) => {
                        failed++;
                        firstFailure(error);
                    });
                    // Use the internal command surface to put arbitrary protocol
                    // fragments on CONNECT rather than a WebTransport data stream.
                    const core = (client as unknown as { core: SessionCore }).core;
                    await core.write(0, frame).catch(() => {});
                }
                if (!complete) {
                    const error = await failure;
                    expect(error).toBeInstanceOf(Error);
                    expect((error as Error).message).toMatch(
                        /memory limit|malformed HTTP\/3 CONNECT/,
                    );
                    expect(failed).toBeGreaterThan(0);
                }
                const control = new WebTransport(url, { insecure: true });
                clients.push(control);
                await control.ready;
                expect((await reader.read()).value).toBeDefined();
                if (complete) expect(failed).toBe(0);
            } finally {
                for (const client of clients) client.close();
                server.close();
                await server.closed;
            }
        },
    );
});

describe('Server session cleanup', () => {
    it.each(['error', 'closed'])('survives %s without an unhandled rejection', (type) => {
        // Exercise the real server's cleanup in a separate Node process using
        // strict rejection handling, so a leaked derived promise fails the test.
        const script = `
            import assert from 'node:assert/strict';
            import { WebTransport, WebTransportServer } from 'rwebtransport';
            const server = new WebTransportServer({host:'127.0.0.1',port:0,cert:'test/fixtures/cert.pem',key:'test/fixtures/key.pem'});
            await server.ready;
            const reader = server.incomingSessions.getReader();
            const client = new WebTransport('https://127.0.0.1:' + server.port + '/test', {insecure:true});
            await client.ready;
            const {value: session} = await reader.read();
            void session.closed.catch(() => {});
            const id = [...server.sessions.keys()][0];
            server.onEvent({type:${JSON.stringify(type)},session:id,message:'QUIC idle timeout',code:0,reason:new Uint8Array(),remote:true});
            await new Promise(resolve => setImmediate(resolve));
            assert.equal(server.sessions.size, 0);
            const control = new WebTransport('https://127.0.0.1:' + server.port + '/control', {insecure:true});
            await control.ready;
            const {value: healthy} = await reader.read();
            assert.ok(healthy);
            client.close();
            control.close();
            server.close();
            await server.closed;
            console.log('survived');
        `;
        const output = execFileSync(
            process.execPath,
            ['--unhandled-rejections=strict', '--input-type=module', '-e', script],
            { encoding: 'utf8', timeout: 10000 },
        );
        expect(output).toContain('survived');
    });
});
