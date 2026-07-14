// SPDX-License-Identifier: Apache-2.0
import { configDefaults, defineConfig } from 'vitest/config';

export default defineConfig({
    test: {
        globals: true,
        environment: 'node',
        include: ['test/**/*.test.ts', 'test/**/*.spec.ts'],
        exclude: [...configDefaults.exclude],
        // Mint a fresh short-lived test certificate before the suite: the client
        // enforces the serverCertificateHashes 14-day validity ceiling, so the
        // fixture must be generated per run rather than committed long-lived.
        globalSetup: ['./test/helpers/global-setup.ts'],
        // The native driver spawns background threads and a real UDP server; keep
        // the suite serial so ports/handles don't contend.
        pool: 'forks',
        fileParallelism: false,
        testTimeout: 30_000,
        hookTimeout: 30_000,
        coverage: {
            provider: 'v8',
            reporter: ['text', 'json', 'html'],
            exclude: ['node_modules/', 'dist/', 'target/', 'vendor/', 'test/', '**/*.d.ts', '**/*.config.*'],
        },
    },
});
