// SPDX-License-Identifier: Apache-2.0
import { webdriverio } from '@vitest/browser-webdriverio';
import { defineConfig } from 'vitest/config';

export default defineConfig({
    test: {
        name: 'browser',
        include: ['test/browser/**/*.test.ts'],
        globalSetup: ['./test/helpers/browser-setup.ts'],
        testTimeout: 30_000,
        hookTimeout: 30_000,
        browser: {
            enabled: true,
            headless: true,
            provider: webdriverio({
                capabilities: {
                    browserName: 'chrome',
                    'wdio:enforceWebDriverClassic': true,
                    'goog:chromeOptions': {
                        args: ['--no-sandbox', '--disable-dev-shm-usage'],
                    },
                },
            }),
            instances: [{ browser: 'chrome' }],
        },
    },
});
