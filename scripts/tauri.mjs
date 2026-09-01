#!/usr/bin/env node
// Tauri CLI 包装器：转发参数给真实的 tauri CLI，并在 build / dev 结束后
// 默认保留 src-tauri/target/*/deps（避免每次都全量编译，太慢）。
// 需要释放磁盘时，用 CLEAN_DEPS=1 显式开启清理。

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { readdir, rm, stat } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(here, '..');
const tauriCli = join(
  projectRoot,
  'node_modules',
  '@tauri-apps',
  'cli',
  'tauri.js',
);

const args = process.argv.slice(2);
const command = args[0];
const CLEAN_DEPS = ['1', 'true', 'on', 'yes'].includes(
  (process.env.CLEAN_DEPS ?? '').toLowerCase(),
);

// GitHub 加速镜像模板：Tauri 打包时用它替换默认的 GitHub 下载地址，避免 WiX / NSIS 下载超时。
// 占位符：<owner>/<repo>/<version>/<asset>。如需换镜像，改这一行即可；已有显式设置则不覆盖。
const GITHUB_MIRROR_TEMPLATE =
  'https://ghfast.top/https://github.com/<owner>/<repo>/releases/download/<version>/<asset>';
if (!process.env.TAURI_BUNDLER_TOOLS_GITHUB_MIRROR_TEMPLATE) {
  process.env.TAURI_BUNDLER_TOOLS_GITHUB_MIRROR_TEMPLATE = GITHUB_MIRROR_TEMPLATE;
}

async function dirSize(dir) {
  let total = 0;
  const entries = await readdir(dir, { withFileTypes: true }).catch(() => []);
  for (const entry of entries) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) {
      total += await dirSize(p);
    } else if (entry.isFile()) {
      const info = await stat(p).catch(() => null);
      if (info) total += info.size;
    }
  }
  return total;
}

async function cleanDeps() {
  const target = join(projectRoot, 'src-tauri', 'target');
  if (!existsSync(target)) return;

  let removed = 0;
  let freed = 0;
  const stages = await readdir(target, { withFileTypes: true }).catch(() => []);
  for (const stage of stages) {
    if (!stage.isDirectory()) continue;
    const deps = join(target, stage.name, 'deps');
    if (!existsSync(deps)) continue;
    freed += await dirSize(deps);
    await rm(deps, { recursive: true, force: true });
    removed += 1;
  }

  if (removed) {
    const mb = (freed / 1024 / 1024).toFixed(1);
    console.log(`\n[clean-deps] 已删除 ${removed} 个 deps 目录，释放 ${mb} MB`);
  }
}

const child = spawn(process.execPath, [tauriCli, ...args], {
  cwd: projectRoot,
  stdio: 'inherit',
});

let finished = false;
async function finish(code) {
  if (finished) return;
  finished = true;

  if (CLEAN_DEPS && (command === 'build' || command === 'dev')) {
    try {
      await cleanDeps();
    } catch (err) {
      console.error(`[clean-deps] 清理失败: ${err.message}`);
    }
  }
  process.exit(code ?? 0);
}

child.on('exit', (code) => finish(code));
child.on('error', (err) => {
  console.error(`[tauri] 无法启动 CLI: ${err.message}`);
  finish(1);
});

// 转发中断信号，确保 dev 进程被用户终止后也能执行清理。
process.on('SIGINT', () => child.kill('SIGINT'));
process.on('SIGTERM', () => child.kill('SIGTERM'));
