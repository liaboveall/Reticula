import type { Plugin } from "vite";

/**
 * What the app uses of React, `createElement` by way of JSX with a key after what
 * is spread into it. A name that is missing here fails the build.
 */
export const NAMES: Readonly<Record<string, readonly string[]>> = {
  react: [
    "cloneElement",
    "Component",
    "createElement",
    "lazy",
    "startTransition",
    "StrictMode",
    "Suspense",
    "useCallback",
    "useEffect",
    "useId",
    "useImperativeHandle",
    "useLayoutEffect",
    "useMemo",
    "useRef",
    "useState",
    "useSyncExternalStore",
  ],
  "react/jsx-runtime": ["Fragment", "jsx", "jsxs"],
};

const PREFIX = "\0named:";

/**
 * React is made of CommonJS modules. Every chunk has an object of its own for
 * each of them, and calls what it uses by way of that object. Here the app gets
 * what it uses by name instead, from one module for all chunks.
 */
export function namedReact(): Plugin {
  return {
    name: "reticula:named-react",
    apply: "build",
    enforce: "pre",
    // React announces itself to the developer tools, which have nothing to show of this build.
    config: () => ({ define: { __REACT_DEVTOOLS_GLOBAL_HOOK__: "undefined" } }),
    resolveId: {
      filter: { id: /^react(?:\/jsx-runtime)?$/ },
      handler(source, importer = "") {
        const isOurs = !importer.startsWith(PREFIX) && !/[\\/]node_modules[\\/]/.test(importer);
        return isOurs ? PREFIX + source : null;
      },
    },
    load: {
      filter: { id: new RegExp(`^${PREFIX}`) },
      handler(id) {
        const source = id.slice(PREFIX.length);
        const names = NAMES[source] ?? [];
        return [
          `import { ${names.map((name) => `${name} as $${name}`).join(", ")} } from "${source}";`,
          ...names.map((name) => `export const ${name} = $${name};`),
        ].join("\n");
      },
    },
  };
}
