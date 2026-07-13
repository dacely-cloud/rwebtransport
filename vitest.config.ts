// SPDX-License-Identifier: Apache-2.0
import { configDefaults, defineConfig } from 'vitest/config';

export default defineConfig({
    test: {
        globals: true,
        environment: 'node',
        include: ['test/**/*.test.ts', 'test/**/*.spec.ts'],
        exclude: [...configDefaults.exclude],
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
