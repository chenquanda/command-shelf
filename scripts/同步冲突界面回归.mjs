/**
 * 文件职责：验证三栏同步冲突窗口的固定结构、典型冲突样例与前端脚本语法。
 * 主要内容：直接检查正式单文件前端，防止冲突窗口、选择动作或浏览器验收入口在后续修改中丢失。
 * 重要约束：本脚本不访问真实数据仓库、不执行 Git，也不启动新的浏览器实例。
 */

import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";

/** 项目根目录，由脚本位置反推，避免调用方工作目录影响检查结果。 */
const repositoryRoot = path.resolve(import.meta.dirname, "..");
/** 正式前端文件路径。 */
const frontendPath = path.join(repositoryRoot, "frontend", "index.html");

/**
 * 从单文件前端提取最后一段内联业务脚本，并交给 JavaScript 引擎做语法编译。
 * @param {string} html 完整前端源码。
 * @returns {string} 页面业务脚本源码。
 */
function extractApplicationScript(html) {
  const scripts = [...html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/gi)];
  assert.ok(scripts.length > 0, "正式前端必须包含内联业务脚本");
  return scripts.at(-1)[1];
}

const html = await fs.readFile(frontendPath, "utf8");
const applicationScript = extractApplicationScript(html);

assert.doesNotThrow(() => new Function(applicationScript), "正式前端业务脚本应通过语法编译");

const requiredFragments = [
  'id="merge-dialog-layer"',
  'class="merge-column-headings"',
  'id="merge-file-tabs"',
  'id="merge-record-nav"',
  'id="merge-records"',
  'id="merge-full-json"',
  'id="merge-diff-view-button"',
  'id="merge-json-view-button"',
  'data-merge-choice="local"',
  'data-merge-choice="remote"',
  'data-merge-choice="custom"',
  'function createPrototypeMergeSession()',
  'openMergeConflict()',
  '本机删除了此命令，远端修改了内容',
  '已按共同基线自动累计：10 + 3 + 5',
  '两边修改结果不同，请选择或编辑最终内容',
  '"start_pull_repository"',
  '"start_push_repository"',
  '"complete_pull_conflict"',
  '"complete_push_conflict"',
  'trapDialogFocus(event, mergeDialog)',
  'mergeState.error = [error?.message, error?.action]',
  'buildMergePreviewDocument(plan)',
];

for (const fragment of requiredFragments) {
  assert.ok(html.includes(fragment), `同步冲突窗口缺少关键内容：${fragment}`);
}

assert.match(html, /\.merge-column-headings,[\s\S]*?\.merge-field-row\s*\{[^}]*grid-template-columns:\s*minmax\(0,\s*31fr\)\s+minmax\(0,\s*38fr\)\s+minmax\(0,\s*31fr\)/s, "冲突窗口应保持本机、合并结果、远端三栏布局");
assert.match(html, /@media\s*\(max-width:\s*1080px\)[\s\S]*?\.merge-dialog\s*\{[^}]*width:\s*calc\(100vw\s*-\s*32px\)/s, "窄窗口下冲突窗口应保留安全边距");

console.log("同步冲突界面回归通过：三栏结构、冲突样例、选择动作、完整 JSON 与脚本语法均有效。");
