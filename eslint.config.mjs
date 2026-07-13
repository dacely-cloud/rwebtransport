// @ts-check
// SPDX-License-Identifier: Apache-2.0

import eslint from '@eslint/js';
import tseslint from 'typescript-eslint';

export default tseslint.config(
    {
        ignores: [
            'dist/**',
            'target/**',
            'vendor/**',
            'prebuilds/**',
            'node_modules/**',
            'scripts/**',
            'examples/**',
            'docs/**',
            '*.config.*',
            'eslint.config.mjs',
        ],
    },
    eslint.configs.recommended,
    ...tseslint.configs.strictTypeChecked,
    {
        languageOptions: {
            parserOptions: {
                projectService: true,
                tsconfigRootDir: import.meta.dirname,
            },
        },
        rules: {
            'no-undef': 'off',
            '@typescript-eslint/no-unused-vars': 'off',
            'no-empty': 'off',
            '@typescript-eslint/restrict-template-expressions': 'off',
            '@typescript-eslint/only-throw-error': 'off',
            '@typescript-eslint/no-unnecessary-condition': 'off',
            '@typescript-eslint/unbound-method': 'warn',
            '@typescript-eslint/no-confusing-void-expression': 'off',
            '@typescript-eslint/no-extraneous-class': 'off',
            'no-async-promise-executor': 'off',
            '@typescript-eslint/no-misused-promises': 'off',
            '@typescript-eslint/no-unnecessary-type-parameters': 'off',
            '@typescript-eslint/no-duplicate-enum-values': 'off',
            'prefer-spread': 'off',
            '@typescript-eslint/no-empty-object-type': 'off',
            '@typescript-eslint/no-base-to-string': 'off',
            '@typescript-eslint/no-dynamic-delete': 'off',
            '@typescript-eslint/no-redundant-type-constituents': 'off',
            'no-useless-assignment': 'off',
            '@typescript-eslint/no-unnecessary-type-arguments': 'off',
            '@typescript-eslint/no-unnecessary-type-assertion': 'off',
            // Deferred<void> is a legitimate pattern; test non-null assertions
            // after explicit checks are fine.
            '@typescript-eslint/no-invalid-void-type': 'off',
            '@typescript-eslint/no-non-null-assertion': 'off',
        },
    },
    {
        files: ['**/*.js', '**/*.cjs', '**/*.mjs'],
        ...tseslint.configs.disableTypeChecked,
        rules: {
            ...tseslint.configs.disableTypeChecked.rules,
            // Plain CommonJS / helper scripts legitimately use require().
            '@typescript-eslint/no-require-imports': 'off',
        },
    },
);
