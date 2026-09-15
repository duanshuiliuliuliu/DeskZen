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

// 打 tag 发布时（CI 的 tag 触发里 GITHUB_REF_NAME=v1.2.3）顺带校验 tag 与版本号一致；
// 分支触发时 GITHUB_REF_NAME 是分支名，不参与校验。
const refName = process.env.GITHUB_REF_NAME;
if (cargoVersion && refName && refName.startsWith("v") && refName !== `v${cargoVersion}`) {
  problems.push(
    `tag ${refName} 与版本号 v${cargoVersion} 不一致（改完版本号要提交并重新打 tag）`,
  );
}

if (problems.length > 0) {
  for (const problem of problems) console.error(`x ${problem}`);
  process.exit(1);
}
console.log(`ok: version=${cargoVersion} identifier=${tauri.identifier}`);
