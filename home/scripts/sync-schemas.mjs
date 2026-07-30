// Copies Rad's generated protocol schemas into public/ so their $id URLs
// resolve to the real documents on the deployed site:
//
//   https://www.radengine.dev/schema/lir.json  -> the query IR
//   https://www.radengine.dev/schema/pir.json  -> the program IR
//
// protocol/*.schema.yaml is the authored source of truth. `task
// generate:protocol` produces the JSON used by clients and the website. This
// build step keeps the public copies aligned when the complete repository is
// available. A deployment from the home directory alone uses the committed
// public copies.

import { mkdirSync, copyFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", ".."); // home/scripts -> repo root
const publicDir = join(here, "..", "public");

const schemas = [
  {
    src: join(repo, "clients", "go", "protocol", "lir.schema.json"),
    dst: join(publicDir, "schema", "lir.json"),
  },
  {
    src: join(repo, "clients", "go", "protocol", "pir.schema.json"),
    dst: join(publicDir, "schema", "pir.json"),
  },
];

for (const { src, dst } of schemas) {
  if (!existsSync(src)) {
    console.log(`sync-schemas: source not present, keeping committed copy: ${dst}`);
    continue;
  }
  mkdirSync(dirname(dst), { recursive: true });
  copyFileSync(src, dst);
  console.log(`sync-schemas: ${src} -> ${dst}`);
}
