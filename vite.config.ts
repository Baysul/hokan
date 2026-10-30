import { defineConfig } from "vite-plus";

export default defineConfig({
  staged: {
    "*.{js,jsx,mjs,cjs,ts,tsx,mts,cts}": "vp check --fix",
    "*.{css,html,json,jsonc,md,mdx,yaml,yml}": "vp fmt",
  },
  fmt: {},
  lint: {
    plugins: ["eslint", "typescript", "unicorn", "oxc", "import"],
    jsPlugins: [{ name: "vite-plus", specifier: "vite-plus/oxlint-plugin" }],
    categories: {
      correctness: "error",
      suspicious: "warn",
    },
    rules: {
      "vite-plus/prefer-vite-plus-imports": "error",

      // Readability and maintainability
      "eslint/no-var": "error",
      "eslint/prefer-const": "error",
      "eslint/eqeqeq": "error",
      "eslint/curly": ["error", "multi-line"],
      "eslint/no-shadow": "error",
      "eslint/no-param-reassign": "error",
      "eslint/no-else-return": "error",
      "eslint/no-nested-ternary": "error",
      "eslint/object-shorthand": "error",

      // Complexity limits
      "eslint/complexity": ["warn", { max: 15 }],
      "eslint/max-depth": ["warn", { max: 3 }],
      "eslint/max-params": ["warn", { max: 4 }],

      // TypeScript quality
      "typescript/no-explicit-any": "error",
      "typescript/no-non-null-assertion": "warn",
      "typescript/no-empty-object-type": "error",
      "typescript/no-unsafe-function-type": "error",
      "typescript/no-inferrable-types": "error",
      "typescript/prefer-for-of": "error",
      "typescript/consistent-type-imports": "error",
      "typescript/consistent-type-exports": "error",
      "typescript/consistent-type-assertions": [
        "error",
        { assertionStyle: "as", objectLiteralTypeAssertions: "never" },
      ],
      "typescript/ban-ts-comment": [
        "error",
        {
          "ts-ignore": true,
          "ts-nocheck": true,
          "ts-expect-error": "allow-with-description",
          minimumDescriptionLength: 10,
        },
      ],

      // Imports and dependencies
      "import/no-duplicates": "error",
      "import/first": "error",
      "import/newline-after-import": "error",
      "import/no-cycle": "warn",
      // Flags side-effect imports such as `import "./style.css"`.
      "import/no-unassigned-import": "off",

      // Error handling
      "unicorn/error-message": "error",
      "unicorn/catch-error-name": "warn",

      // Type-aware checks (enabled by options.typeAware)
      "typescript/no-floating-promises": "error",
      "typescript/await-thenable": "error",
      "typescript/no-misused-promises": "error",
      "typescript/switch-exhaustiveness-check": "error",
      "typescript/no-unsafe-assignment": "error",
      "typescript/no-unsafe-argument": "error",
      "typescript/no-unsafe-call": "error",
      "typescript/no-unsafe-member-access": "error",
      "typescript/no-unsafe-return": "error",
      "typescript/prefer-nullish-coalescing": "warn",
      // no-unnecessary-condition is left off: TypeScript narrows variables that change across
      // `await` or inside closures, and DOM types hide runtime feature checks.
      // `navigator.onLine === false` is deliberate: it is undefined outside browsers.
      "typescript/no-unnecessary-boolean-literal-compare": "off",
    },
    overrides: [
      {
        files: ["**/tests/**", "**/e2e/**"],
        rules: {
          "typescript/no-non-null-assertion": "off",
          // Asymmetric matchers such as `expect.any(Number)` are typed as `any`.
          "typescript/no-unsafe-assignment": "off",
        },
      },
    ],
    options: { typeAware: true, typeCheck: true },
  },
  run: {
    cache: true,
  },
});
