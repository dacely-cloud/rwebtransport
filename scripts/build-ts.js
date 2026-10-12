#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
'use strict';

const { rmSync } = require('node:fs');
const path = require('node:path');
const { build } = require('esbuild');

const root = path.resolve(__dirname, '..');

async function main() {
    rmSync(path.join(root, 'dist'), { recursive: true, force: true });

    for (const format of ['cjs', 'esm']) {
        await build({
            absWorkingDir: root,
            entryPoints: ['src/index.ts'],
            outfile: `dist/index.${format === 'cjs' ? 'js' : 'mjs'}`,
            bundle: true,
            platform: 'node',
            target: 'node24',
            packages: 'external',
            format,
            sourcemap: true,
            // The native addon loader uses import.meta.url in both bundles.
            ...(format === 'cjs'
                ? {
                      define: { 'import.meta.url': '__rwebtransportImportMetaUrl' },
                      banner: {
                          js: "'use strict';\nconst __rwebtransportImportMetaUrl = require('node:url').pathToFileURL(__filename).href;",
                      },
                  }
                : {}),
        });
    }
}

main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
});
