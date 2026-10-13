// SPDX-License-Identifier: Apache-2.0
import { configDefaults, defineConfig } from 'vitest/config';
import browserConfig from './vitest.browser.config.js';

export default defineConfig({
    test: {
        projects: [
            {
                test: {
                    name: 'node',
                    globals: true,
                    environment: 'node',
                    include: ['test/**/*.test.ts', 'test/**/*.spec.ts'],
                    exclude: [...configDefaults.exclude, 'test/browser/**'],
                    // Ensure a valid short-lived test certificate before the suite: the client
                    // enforces the serverCertificateHashes 14-day validity ceiling, so the
                    // fixture is generated when needed rather than committed long-lived.
                    globalSetup: ['./test/helpers/global-setup.ts'],
                    // The native driver spawns background threads and a real UDP server; keep
                    // the suite serial so ports/handles don't contend.
                    pool: 'forks',
                    fileParallelism: false,
                    testTimeout: 30_000,
                    hookTimeout: 30_000,
                },
            },
            browserConfig,
        ],
        coverage: {
            provider: 'v8',
            reporter: ['text', 'json', 'html'],
            include: ['src/**/*.ts'],
            exclude: ['**/*.d.ts'],
        },
    },
});
