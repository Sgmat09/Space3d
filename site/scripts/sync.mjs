// Copies repo-owned sources (Claude Code integration, docs) into public/ so the site serves
// exactly what the repo ships, and builds llms-full.txt from docs/. Run before every build.
import { cpSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const pub = join(root, "site", "public");
mkdirSync(join(pub, "claude"), { recursive: true });
cpSync(join(root, "integrations/claude-code/AGENTS.template.md"), join(pub, "claude/AGENTS.md"));
cpSync(join(root, "integrations/claude-code/skills/space3d-game/SKILL.md"), join(pub, "claude/SKILL.md"));

const docs = readdirSync(join(root, "docs")).filter((f) => f.endsWith(".md")).sort();
const body = [readFileSync(join(root, "README.md"), "utf8")]
  .concat(docs.map((f) => `\n\n<!-- docs/${f} -->\n` + readFileSync(join(root, "docs", f), "utf8")))
  .join("\n");
writeFileSync(join(pub, "llms-full.txt"), body);
console.log(`sync: ${docs.length} docs -> llms-full.txt (${body.length} bytes)`);
