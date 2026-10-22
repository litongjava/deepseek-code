/// <reference types="vitest" />
import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
import path from 'path';

export default defineConfig({
  plugins: [react()],
  test: {
    globals: true,
    environment: 'jsdom',
    setupFiles: ['./src/__tests__/setup.ts'],
    include: ['src/**/*.{test,spec}.{ts,tsx}'],
    exclude: ['node_modules', 'dist', 'src-tauri'],
    // react-syntax-highlighter's CommonJS build require()s the ESM-only
    // refractor; transform its ES module build like the app build does.
    server: {
      deps: {
        inline: ['react-syntax-highlighter'],
      },
    },
    coverage: {
      provider: 'v8',
      reporter: ['text', 'json', 'html'],
      exclude: [
        'node_modules/',
        'src/__tests__/',
        '**/*.d.ts',
        '**/*.config.*',
        'src-tauri/',
      ],
    },
  },
  resolve: {
    alias: [
      { find: '@', replacement: path.resolve(__dirname, './src') },
      // Use the ES module build; see test.server.deps.inline above.
      { find: /^react-syntax-highlighter$/, replacement: 'react-syntax-highlighter/dist/esm/index.js' },
    ],
  },
});
