import { defineConfig } from 'vite';
import { resolve } from 'node:path';
import { readdirSync } from 'node:fs';
import { VitePWA } from 'vite-plugin-pwa';

/**
 * HomeTube uses a multi-page-app architecture: the Rust server renders
 * HTML, and each page imports only the Lit components it needs. Vite is
 * configured in library/multi-entry mode so that each component file in
 * `src/components/` is bundled into its own ES module under
 * `dist/components/<name>.js`. The server's `<script type="module">`
 * tags reference these files directly.
 *
 * The `vite-plugin-pwa` plugin runs in `injectManifest` mode: it takes
 * our hand-written `src/sw.ts`, injects a Workbox precache manifest at
 * build time, and emits the result as `/sw.js` at the root of the dist
 * directory.
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
  plugins: [
    VitePWA({
      strategies: 'injectManifest',
      srcDir: 'src',
      filename: 'sw.ts',
      injectRegister: false,
      // Vite-plugin-pwa emits the SW to `dist/sw.js` by default.
      manifest: {
        name: 'HomeTube',
        short_name: 'HomeTube',
        description: 'Self-hosted YouTube frontend for kids.',
        start_url: '/',
        display: 'standalone',
        background_color: '#ffffff',
        theme_color: '#2563eb',
        icons: [],
      },
      injectManifest: {
        globPatterns: ['**/*.{js,css,html}'],
        // Don't fail the build if a referenced asset is missing —
        // we share the dist folder with the askama-rendered HTML, so
        // some references are dangling.
        maximumFileSizeToCacheInBytes: 5 * 1024 * 1024,
      },
      // We don't ship icons in this repo yet, so disable the manifest
      // emission to keep the build clean for the askama frontend.
      includeAssets: [],
    }),
  ],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
    sourcemap: true,
    // The video-player bundle pulls in vidstack + dashjs; ~1.2 MB
    // unminified is expected. Bump the soft warning ceiling to silence
    // the noisy "chunk larger than 500 kB" output.
    chunkSizeWarningLimit: 2000,
    rollupOptions: {
      input: {
        // Per-component bundles.
        ...discoverComponentEntries(),
        // SW registration shim served from `<base>` template.
        'services/sw-register': resolve(__dirname, 'src/services/sw-register.ts'),
        // MPA View Transitions (directional animation support).
        'services/view-transitions': resolve(__dirname, 'src/services/view-transitions.ts'),
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
