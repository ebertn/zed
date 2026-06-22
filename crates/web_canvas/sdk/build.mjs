// Builds the canvas SDK into `../assets/canvas-sdk.js` (a single minified IIFE
// that exposes React + the component library as globals). Run `npm run build`
// after editing `src/sdk.tsx`; the output is committed and embedded by the
// `web_canvas` crate via `include_str!`, so a normal cargo build needs no Node.

import * as esbuild from "esbuild";

await esbuild.build({
  entryPoints: ["src/sdk.tsx"],
  bundle: true,
  format: "iife",
  minify: true,
  // WKWebView on a recent macOS; keep the target modern but safe.
  target: ["safari15"],
  jsx: "transform",
  // React reads NODE_ENV; define it so the production paths are used and there
  // is no bare `process` reference in the browser bundle.
  define: { "process.env.NODE_ENV": '"production"' },
  legalComments: "none",
  outfile: "../assets/canvas-sdk.js",
});

console.log("built crates/web_canvas/assets/canvas-sdk.js");
