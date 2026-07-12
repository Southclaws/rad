// Copies Rad's canonical JSON Schemas into public/ so their $id URLs resolve
// to the real documents on the deployed site:
//
//   https://www.radengine.dev/schema.rad.json  -> the schema.rad table format
//   https://www.radengine.dev/schema/lir.json  -> the query IR
//
// The canonical copies live under the Go module's rad/ tree, which is the
// single source of truth. This runs before the build to refresh the served
// copies. On Vercel the module source may live outside the build root, so a
// missing source is not an error: the committed copy in public/ is used as is.
// Run it locally (or in CI, where the whole repo is present) to keep them
// fresh, and commit the result.

import { mkdirSync, copyFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", ".."); // home/scripts -> repo root
const publicDir = join(here, "..", "public");

const schemas = [
  {
    src: join(repo, "rad", "engine", "02_catalog", "schema", "radschema.json"),
    dst: join(publicDir, "schema.rad.json"),
  },
  {
    src: join(repo, "rad", "protocol", "lir.schema.json"),
    dst: join(publicDir, "schema", "lir.json"),
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
