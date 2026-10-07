#!/usr/bin/env node
/**
 * 校验 config.example.toml 里的每个配置字段都在前端出现过。
 *
 * 背景：`src/config.rs` 是配置的唯一定义处，`config.example.toml` 是字段清单；
 * 前端（`web/src/lib/api.ts` 的类型 + 各页面 + 文案）必须能读写同一套字段名。
 * 后端加了字段而前端漏掉时，管理面板会把该字段以空值写回，等于静默清空配置 ——
 * 这类漂移在运行时很难发现，因此在 CI 中显式失败。
 *
 * 例外字段写在 web/scripts/config-parity-allowlist.txt（每行一个字段名 + 理由）。
 */
import { readFileSync, existsSync, readdirSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const repoRoot = dirname(root);
const configPath = join(repoRoot, 'config.example.toml');
const allowlistPath = join(root, 'scripts', 'config-parity-allowlist.txt');

/** 收集 TOML 中的字段名（含被注释掉的“可选字段”示例） */
function collectConfigKeys(text) {
  const keys = new Map();
  let section = '(root)';
  for (const rawLine of text.split('\n')) {
    let line = rawLine.trim();
    if (line.startsWith('#')) line = line.replace(/^#+/, '').trim();
    const sectionMatch = /^\[+([A-Za-z0-9_.]+)\]/.exec(line);
    if (sectionMatch) {
      section = sectionMatch[1];
      continue;
    }
    const keyMatch = /^([a-z_][a-z0-9_]*)\s*=/.exec(line);
    if (keyMatch) keys.set(keyMatch[1], section);
  }
  return keys;
}

/** 前端源码（类型 + 页面 + 组件 + 文案）整体作为检索范围 */
function collectFrontendSource(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      collectFrontendSource(full, out);
    } else if (/\.(ts|tsx|mjs|json)$/.test(entry.name)) {
      out.push(readFileSync(full, 'utf8'));
    }
  }
  return out;
}

const allowlist = new Set(
  (existsSync(allowlistPath) ? readFileSync(allowlistPath, 'utf8') : '')
    .split('\n')
    .map((l) => l.split('#')[0]?.trim())
    .filter((l) => l && !l.startsWith('#')),
);

const keys = collectConfigKeys(readFileSync(configPath, 'utf8'));
const sources = collectFrontendSource(join(root, 'src')).join('\n');

const missing = [];
for (const [key, section] of keys) {
  if (allowlist.has(key)) continue;
  if (!sources.includes(key)) missing.push(`  [${section}] ${key}`);
}

if (missing.length > 0) {
  console.error('✗ 以下配置字段未在前端出现（新增字段请同步 web/src，或加入 allowlist 并说明理由）：');
  console.error(missing.join('\n'));
  process.exit(1);
}

console.log(`✓ 配置字段与前端一致（${keys.size} 个字段，allowlist ${allowlist.size} 个）`);
