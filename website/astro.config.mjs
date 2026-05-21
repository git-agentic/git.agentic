// @ts-check
import { defineConfig } from 'astro/config';

export default defineConfig({
  site: 'https://git-agentic.com',
  trailingSlash: 'never',
  build: {
    format: 'file',
    inlineStylesheets: 'always',
  },
  compressHTML: true,
});
