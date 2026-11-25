import { defineConfig } from 'vite';
export default defineConfig({
  clearScreen: false,
  server: { port: 1420, strictPort: true, watch: { ignored: ['**/src-tauri/**'] } },
  // The app imports viewers and the Markdown engine lazily. Prebundling them keeps the
  // dev server from discovering a dependency mid-session, which reloads the page and
  // stalls the first transcript render (and every Playwright run that reads it).
  optimizeDeps: {
    include: ['marked', 'dompurify', 'katex', 'three', '@pierre/diffs', '@xterm/xterm', '@xterm/addon-fit',
      'docx-preview', 'pdfjs-dist/legacy/build/pdf.mjs', 'xlsx', 'material-icon-theme/dist/material-icons.json'],
  },
  build: { target: 'es2022', assetsInlineLimit: 0 },
});
