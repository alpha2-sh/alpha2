// Lint configuration for the published JavaScript/TypeScript packages.
//
// Scope is `packages/` (what npm ships, so it carries the contract downstream code compiles
// against, and the build scripts that make it) plus the editor consumer fixture under
// `tests/fixtures/`. Generated output (`dist`,
// `wasm`) is never linted. Add another `files` entry to extend this to `scripts/`.

import js from "@eslint/js";
import prettier from "eslint-config-prettier/flat";
import globals from "globals";
import tseslint from "typescript-eslint";

/**
 * Underscore marks a deliberately unused binding, and destructuring a field out of
 * an object to build the rest is how these modules omit a key rather than a mistake.
 */
const unusedVars = {
  argsIgnorePattern: "^_",
  varsIgnorePattern: "^_",
  ignoreRestSiblings: true,
};

export default tseslint.config(
  {
    ignores: ["**/dist/**", "**/wasm/**", "**/node_modules/**", "**/target/**"],
  },
  {
    files: ["packages/*/src/**/*.{ts,tsx}"],
    extends: [js.configs.recommended, tseslint.configs.recommendedTypeChecked],
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
      globals: { ...globals.browser, ...globals.worker },
    },
    rules: {
      "@typescript-eslint/no-unused-vars": ["error", unusedVars],
      "prefer-const": ["error", { ignoreReadBeforeAssign: true }],
    },
  },
  {
    files: ["packages/*/test/**/*.mjs", "packages/*/scripts/**/*.mjs"],
    extends: [js.configs.recommended],
    languageOptions: {
      ecmaVersion: 2023,
      sourceType: "module",
      globals: { ...globals.node },
    },
    rules: {
      "no-unused-vars": ["error", unusedVars],
    },
  },
  {
    files: [
      "packages/alpha2-editor/src/**/*.js",
      "tests/fixtures/editor-consumer/*.js",
    ],
    extends: [js.configs.recommended],
    languageOptions: {
      ecmaVersion: 2023,
      sourceType: "module",
      globals: { ...globals.browser, ...globals.worker },
    },
    rules: { "no-unused-vars": ["error", unusedVars] },
  },
  prettier,
);
