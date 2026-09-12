// Version consistency check: Cargo.toml is the source of truth; package.json
// and tauri.conf.json must match it. Also rejects an identifier ending in ".app".
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));
const problems = [];

function readJson(relativePath) {
  return JSON.parse(readFileSync(new URL(relativePath, `file://${repoRoot}/`), "utf8"));
}

const pkg = readJson("package.json");
const tauri = readJson("src-tauri/tauri.conf.json");
const cargoToml = readFileSync(new URL("src-tauri/Cargo.toml", `file://${repoRoot}/`), "utf8");
const cargoVersion = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)?.[1];

if (!cargoVersion) {
  problems.push("src-tauri/Cargo.toml has no version field");
} else {
  if (pkg.version !== cargoVersion) {
    problems.push(
      `package.json version ${pkg.version} != Cargo.toml version ${cargoVersion}`,
    );
  }
  if (tauri.version !== cargoVersion) {
    problems.push(
      `tauri.conf.json version ${tauri.version} != Cargo.toml version ${cargoVersion}`,
    );
  }
}

if (typeof tauri.identifier === "string" && tauri.identifier.endsWith(".app")) {
  problems.push(`tauri.conf.json identifier must not end with ".app": ${tauri.identifier}`);
}

if (problems.length > 0) {
  for (const problem of problems) console.error(`x ${problem}`);
  process.exit(1);
}
console.log(`ok: version=${cargoVersion} identifier=${tauri.identifier}`);
