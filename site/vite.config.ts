import { resolve } from "node:path";
import { defineConfig } from "vite-plus";

export default defineConfig({
  build: {
    rollupOptions: {
      input: {
        main: resolve(import.meta.dirname, "index.html"),
        claude: resolve(import.meta.dirname, "claude-code.html"),
        examples: resolve(import.meta.dirname, "examples.html"),
        docs: resolve(import.meta.dirname, "docs.html"),
      },
    },
  },
});
