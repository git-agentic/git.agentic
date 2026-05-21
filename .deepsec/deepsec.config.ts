import { defineConfig } from "deepsec/config";

export default defineConfig({
  projects: [
    { id: "git.agentic", root: ".." },
    // <deepsec:projects-insert-above>
  ],
});
