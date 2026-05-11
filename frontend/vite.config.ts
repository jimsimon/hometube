import { defineConfig } from 'vite';
import { resolve } from 'node:path';
import { readdirSync } from 'node:fs';

/**
 * HomeTube uses a multi-page-app architecture: the Rust server renders
 * HTML, and each page imports only the Lit components it needs. Vite is
 * configured in library/multi-entry mode so that each component file in
 * `src/components/` is bundled into its own ES module under
 * `dist/components/<name>.js`. The server's `<script type="module">`
 * tags reference these files directly.
 */

const componentsDir = resolve(__dirname, 'src/components');

function discoverComponentEntries(): Record<string, string> {
  const entries: Record<string, string> = {};
  try {
    for (const file of readdirSync(componentsDir)) {
      if (file.endsWith('.ts')) {
        const name = `components/${file.replace(/\.ts$/, '')}`;
        entries[name] = resolve(componentsDir, file);
      }
    }
  } catch {
    // Components directory may be empty during scaffolding.
  }
  return entries;
}

export default defineConfig({
  root: __dirname,
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
    sourcemap: true,
    rollupOptions: {
      input: {
        // Service worker (registered by the app shell).
        sw: resolve(__dirname, 'src/sw.ts'),
        // Per-component bundles.
        ...discoverComponentEntries(),
      },
      output: {
        // Stable filenames so the askama templates can reference them
        // without cache-busting hashes.
        entryFileNames: '[name].js',
        chunkFileNames: 'chunks/[name]-[hash].js',
        assetFileNames: 'styles/[name][extname]',
        format: 'es',
      },
      preserveEntrySignatures: 'allow-extension',
    },
  },
});
