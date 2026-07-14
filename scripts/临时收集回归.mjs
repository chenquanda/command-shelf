/**
 * 文件职责：验证临时收集固定入口、时间流、新增保存和响应式边界。
 * 主要内容：使用隔离 Edge DevTools 会话驱动正式单文件前端，并以受控 Tauri 替身验证保存与回滚。
 * 重要约束：浏览器配置和截图只写入项目 `.local`，不读取个人配置或真实命令仓库。
 */

import fs from "node:fs/promises";
import path from "node:path";
import { spawn } from "node:child_process";
import { pathToFileURL } from "node:url";

/** 项目根目录，由脚本位置稳定反推。 */
const repositoryRoot = path.resolve(import.meta.dirname, "..");
/** 正式前端文件地址；浏览器预览分支提供不落盘的临时收集样例。 */
const frontendUrl = pathToFileURL(path.join(repositoryRoot, "frontend", "index.html")).href;
/** 本轮测试隔离目录，避免接触用户浏览器配置。 */
const evidenceDirectory = path.join(repositoryRoot, ".local", "inbox-regression");
/** 每次运行使用唯一配置目录，防止并发回归互相占用锁文件。 */
const browserProfile = path.join(evidenceDirectory, `profile-${process.pid}-${Date.now()}`);
/** 高位随机端口降低与本机调试会话冲突的概率。 */
const debuggingPort = 9500 + Math.floor(Math.random() * 300);

/** 短暂等待异步页面状态，不使用阻塞休眠。 */
function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

/** 查找 Windows 自带 Edge，作为当前环境没有浏览器连接器时的隔离回归载体。 */
async function findEdge() {
  const candidates = [
    path.join(process.env["ProgramFiles(x86)"] || "", "Microsoft", "Edge", "Application", "msedge.exe"),
    path.join(process.env.ProgramFiles || "", "Microsoft", "Edge", "Application", "msedge.exe"),
  ];
  for (const candidate of candidates) {
    try {
      await fs.access(candidate);
      return candidate;
    } catch {
      // 当前标准位置不存在时继续检查另一个安装目录。
    }
  }
  throw new Error("未找到 Microsoft Edge，无法执行临时收集前端回归");
}

/** 轮询 DevTools 页面列表，直到隔离浏览器完成启动。 */
async function waitForPageTarget() {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${debuggingPort}/json/list`);
      const targets = await response.json();
      const page = targets.find((target) => target.type === "page" && target.webSocketDebuggerUrl);
      if (page) return page;
    } catch {
      // 调试端口尚未监听时继续短暂轮询。
    }
    await delay(100);
  }
  throw new Error("15 秒内未发现临时收集回归页面");
}

/** 建立最小 CDP 客户端，并记录页面脚本异常作为验收门禁。 */
async function connectCdp(webSocketUrl) {
  const socket = new WebSocket(webSocketUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
  let nextRequestId = 1;
  const pendingRequests = new Map();
  const exceptions = [];
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data));
    if (message.method === "Runtime.exceptionThrown") exceptions.push(message.params);
    const pending = pendingRequests.get(message.id);
    if (!pending) return;
    pendingRequests.delete(message.id);
    if (message.error) pending.reject(new Error(message.error.message));
    else pending.resolve(message.result);
  });
  return {
    send(method, params = {}) {
      const id = nextRequestId++;
      return new Promise((resolve, reject) => {
        pendingRequests.set(id, { resolve, reject });
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
    getExceptions() {
      return [...exceptions];
    },
    close() {
      socket.close();
    },
  };
}

/** 在页面主世界执行表达式，并把浏览器异常转换为明确测试失败。 */
async function evaluate(send, expression) {
  const result = await send("Runtime.evaluate", {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.exception?.description || "页面表达式执行失败");
  }
  return result.result?.value;
}

/** 等待页面条件成立；超时信息包含业务场景。 */
async function waitForCondition(send, expression, description) {
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline) {
    if (await evaluate(send, expression)) return;
    await delay(100);
  }
  throw new Error(`等待超时：${description}`);
}

/** 切换视口并返回横向溢出证据。 */
async function inspectViewport(send, width, height) {
  await send("Emulation.setDeviceMetricsOverride", {
    width,
    height,
    deviceScaleFactor: 1,
    mobile: false,
  });
  await delay(120);
  return evaluate(send, `({
    width: ${width},
    height: ${height},
    documentWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
    hasHorizontalOverflow: document.documentElement.scrollWidth > document.documentElement.clientWidth
  })`);
}

let browserProcess;
let cdp;
try {
  await fs.mkdir(evidenceDirectory, { recursive: true });
  const edge = await findEdge();
  browserProcess = spawn(edge, [
    "--headless=new",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    `--remote-debugging-port=${debuggingPort}`,
    `--user-data-dir=${browserProfile}`,
    "--window-size=1440,1024",
    frontendUrl,
  ], { stdio: "ignore", windowsHide: true });

  const target = await waitForPageTarget();
  cdp = await connectCdp(target.webSocketDebuggerUrl);
  await cdp.send("Runtime.enable");
  await cdp.send("Page.enable");
  await waitForCondition(cdp.send, "document.readyState === 'complete' && Boolean(document.querySelector('#inbox-nav-button'))", "正式前端完成加载");

  await evaluate(cdp.send, "document.querySelector('#inbox-nav-button').click()");
  await waitForCondition(cdp.send, "document.querySelector('#category-title')?.textContent === '临时收集'", "切换到临时收集页");

  const readOnlyEvidence = await evaluate(cdp.send, `(() => {
    const inboxButton = document.querySelector('#inbox-nav-button');
    const categoryNav = document.querySelector('.category-nav');
    const groups = [...document.querySelectorAll('[data-inbox-group]')];
    return {
      fixedEntryBeforeCategories: Boolean(inboxButton.compareDocumentPosition(categoryNav) & Node.DOCUMENT_POSITION_FOLLOWING),
      activeEntry: inboxButton.getAttribute('aria-current'),
      count: document.querySelector('#inbox-nav-count')?.textContent,
      title: document.querySelector('#category-title')?.textContent,
      description: document.querySelector('#category-description')?.textContent,
      itemIds: [...document.querySelectorAll('[data-inbox-id]')].map((item) => item.dataset.inboxId),
      groupLabels: groups.map((group) => group.querySelector('h2')?.textContent),
      firstContent: document.querySelector('[data-inbox-id="preview-inbox-1"] .inbox-content')?.textContent,
      linkHref: document.querySelector('[data-inbox-id="preview-inbox-1"] a')?.href,
      commandListHidden: getComputedStyle(document.querySelector('#command-list')).display === 'none',
      commandActionsHidden: ['ask-codex-button', 'copy-sort-button', 'add-command-button'].every((id) => getComputedStyle(document.getElementById(id)).display === 'none'),
      newInboxVisible: getComputedStyle(document.querySelector('#new-inbox-button')).display !== 'none',
      composerVisible: getComputedStyle(document.querySelector('#inbox-composer')).display !== 'none',
      categoryCount: document.querySelectorAll('[data-category-id]').length
    };
  })()`);

  const compactViewport = await inspectViewport(cdp.send, 1024, 768);
  const largeViewport = await inspectViewport(cdp.send, 1440, 1024);
  const screenshot = await cdp.send("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
  const screenshotPath = path.join(evidenceDirectory, "临时收集页-1440x1024.png");
  await fs.writeFile(screenshotPath, Buffer.from(screenshot.data, "base64"));

  const navigationEvidence = await evaluate(cdp.send, `(() => {
    document.querySelector('[data-category-id="linux"]').click();
    const categoryTitle = document.querySelector('#category-title')?.textContent;
    const actionsVisible = ['ask-codex-button', 'copy-sort-button', 'add-command-button'].every((id) => getComputedStyle(document.getElementById(id)).display !== 'none');
    document.querySelector('#inbox-nav-button').click();
    return { categoryTitle, actionsVisible, returnedTitle: document.querySelector('#category-title')?.textContent };
  })()`);

  /* 重载前注入最小 Tauri 契约，覆盖真实桌面保存、失败回滚、忙碌禁用和重启恢复。 */
  await evaluate(cdp.send, "localStorage.removeItem('command-shelf-inbox-regression')");
  await cdp.send("Page.addScriptToEvaluateOnNewDocument", {
    source: `(() => {
      const storageKey = 'command-shelf-inbox-regression';
      const readState = () => JSON.parse(localStorage.getItem(storageKey) || '{"items":[],"revision":1}');
      const writeState = (state) => localStorage.setItem(storageKey, JSON.stringify(state));
      window.__inboxMock = { failNextSave: false, saveDelay: 0 };
      window.__TAURI__ = { core: { invoke: async (command, args = {}) => {
        if (command === 'load_app') return {
          repositoryPath: 'D:\\\\mock-command-data', syncState: 'synced', statusMessage: '测试仓库已连接。',
          document: { schemaVersion: 1, categories: [] }, documentHash: 'commands-hash', error: null
        };
        if (command === 'load_inbox_document') {
          const state = readState();
          return { document: { schemaVersion: 1, items: state.items }, documentHash: 'inbox-hash-' + state.revision, initializedEmptyDocument: false };
        }
        if (command === 'save_inbox_document') {
          if (window.__inboxMock.saveDelay > 0) await new Promise((resolve) => setTimeout(resolve, window.__inboxMock.saveDelay));
          if (window.__inboxMock.failNextSave) {
            window.__inboxMock.failNextSave = false;
            throw { message: '测试保存失败', action: '请保留输入后重试。' };
          }
          const state = readState();
          state.items = structuredClone(args.document.items);
          state.revision += 1;
          writeState(state);
          return { document: structuredClone(args.document), documentHash: 'inbox-hash-' + state.revision, initializedEmptyDocument: false };
        }
        throw { message: '未实现的测试命令：' + command };
      } } };
    })();`
  });
  await cdp.send("Page.reload", { ignoreCache: true });
  await waitForCondition(cdp.send, "document.readyState === 'complete' && document.querySelector('#repository-button')?.textContent.includes('mock-command-data')", "桌面替身完成应用启动");
  await evaluate(cdp.send, "document.querySelector('#inbox-nav-button').click()");
  await waitForCondition(cdp.send, "document.querySelector('#category-title')?.textContent === '临时收集' && !document.querySelector('#inbox-save-button').disabled", "桌面替身加载空临时文档");

  const blankEvidence = await evaluate(cdp.send, `(() => {
    const input = document.querySelector('#inbox-content-input');
    input.value = '   ';
    document.querySelector('#inbox-composer').requestSubmit();
    return { count: document.querySelector('#inbox-nav-count').textContent, error: document.querySelector('#inbox-composer-error').textContent };
  })()`);

  await evaluate(cdp.send, `(() => {
    document.querySelector('#inbox-content-input').value = '只记一段文字';
    document.querySelector('#inbox-save-button').click();
  })()`);
  await waitForCondition(cdp.send, "document.querySelector('#inbox-nav-count')?.textContent === '1' && !document.querySelector('#inbox-save-button').disabled", "按钮保存纯文字");
  await evaluate(cdp.send, `(() => {
    const input = document.querySelector('#inbox-content-input');
    input.value = 'https://tauri.app/';
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true, bubbles: true }));
  })()`);
  await waitForCondition(cdp.send, "document.querySelector('#inbox-nav-count')?.textContent === '2' && !document.querySelector('#inbox-save-button').disabled", "快捷键保存链接");
  await evaluate(cdp.send, `(() => {
    document.querySelector('#inbox-content-input').value = '稍后阅读\\nhttps://github.com/charmbracelet/glow';
    document.querySelector('#inbox-save-button').click();
  })()`);
  await waitForCondition(cdp.send, "document.querySelector('#inbox-nav-count')?.textContent === '3' && !document.querySelector('#inbox-save-button').disabled", "连续保存混合内容");

  await evaluate(cdp.send, `(() => {
    window.__inboxMock.failNextSave = true;
    document.querySelector('#inbox-content-input').value = '保存失败时保留我';
    document.querySelector('#inbox-save-button').click();
  })()`);
  await waitForCondition(cdp.send, "document.querySelector('#inbox-composer-error')?.textContent.includes('输入内容已保留')", "保存失败完成界面回滚");
  const failureEvidence = await evaluate(cdp.send, `({
    count: document.querySelector('#inbox-nav-count').textContent,
    input: document.querySelector('#inbox-content-input').value,
    storedCount: JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items.length
  })`);

  await evaluate(cdp.send, `(() => {
    window.__inboxMock.saveDelay = 350;
    document.querySelector('#inbox-content-input').value = '验证同步期间禁用';
    document.querySelector('#inbox-save-button').click();
  })()`);
  await delay(80);
  const busyEvidence = await evaluate(cdp.send, `({
    inputDisabled: document.querySelector('#inbox-content-input').disabled,
    saveDisabled: document.querySelector('#inbox-save-button').disabled,
    pullDisabled: document.querySelector('#pull-button').disabled,
    newDisabled: document.querySelector('#new-inbox-button').disabled
  })`);
  await waitForCondition(cdp.send, "document.querySelector('#inbox-nav-count')?.textContent === '4' && !document.querySelector('#inbox-save-button').disabled", "延迟保存完成");

  await cdp.send("Page.reload", { ignoreCache: true });
  await waitForCondition(cdp.send, "document.readyState === 'complete' && document.querySelector('#repository-button')?.textContent.includes('mock-command-data')", "重启替身应用");
  await evaluate(cdp.send, "document.querySelector('#inbox-nav-button').click()");
  await waitForCondition(cdp.send, "document.querySelector('#inbox-nav-count')?.textContent === '4'", "重启后恢复已保存记录");
  const saveEvidence = await evaluate(cdp.send, `({
    count: document.querySelector('#inbox-nav-count').textContent,
    firstContent: document.querySelector('[data-inbox-id] .inbox-content')?.textContent,
    storedItems: JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items.map((item) => item.content)
  })`);

  await evaluate(cdp.send, "document.querySelector('[data-inbox-edit]').click()");
  await waitForCondition(cdp.send, "Boolean(document.querySelector('[data-inbox-edit-input]'))", "打开临时记录编辑器");
  const cancelEvidence = await evaluate(cdp.send, `(() => {
    const input = document.querySelector('[data-inbox-edit-input]');
    input.value = '取消按钮不得保存';
    document.querySelector('[data-inbox-edit-cancel]').click();
    const stored = JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items[0];
    return { storedContent: stored.content, visibleContent: document.querySelector('[data-inbox-id] .inbox-content')?.textContent, focusRestored: document.activeElement.matches('[data-inbox-edit]') };
  })()`);

  await evaluate(cdp.send, "document.querySelector('[data-inbox-edit]').click()");
  await waitForCondition(cdp.send, "Boolean(document.querySelector('[data-inbox-edit-input]'))", "再次打开临时记录编辑器");
  const escapeEvidence = await evaluate(cdp.send, `(() => {
    const input = document.querySelector('[data-inbox-edit-input]');
    input.value = 'Escape 不得保存';
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    return { editorClosed: !document.querySelector('[data-inbox-edit-input]'), focusRestored: document.activeElement.matches('[data-inbox-edit]') };
  })()`);

  await evaluate(cdp.send, "document.querySelector('[data-inbox-edit]').click()");
  await waitForCondition(cdp.send, "Boolean(document.querySelector('[data-inbox-edit-input]'))", "打开空白编辑校验");
  const emptyEditEvidence = await evaluate(cdp.send, `(() => {
    document.querySelector('[data-inbox-edit-input]').value = '   ';
    document.querySelector('[data-inbox-edit-form]').requestSubmit();
    return { error: document.querySelector('[data-inbox-edit-error]').textContent, editorOpen: Boolean(document.querySelector('[data-inbox-edit-input]')) };
  })()`);

  const beforeEdit = await evaluate(cdp.send, "JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items[0]");
  await evaluate(cdp.send, `(() => {
    document.querySelector('[data-inbox-edit-input]').value = '已完成一次有效编辑';
    document.querySelector('[data-inbox-edit-form]').requestSubmit();
  })()`);
  await waitForCondition(cdp.send, "document.querySelector('[data-inbox-id] .inbox-content')?.textContent === '已完成一次有效编辑' && !document.querySelector('#inbox-save-button').disabled", "有效编辑保存完成");
  const editEvidence = await evaluate(cdp.send, `(() => {
    const stored = JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items[0];
    return { stored, visibleContent: document.querySelector('[data-inbox-id] .inbox-content')?.textContent, focusRestored: document.activeElement.matches('[data-inbox-edit]') };
  })()`);

  await evaluate(cdp.send, "document.querySelector('[data-inbox-edit]').click()");
  await waitForCondition(cdp.send, "Boolean(document.querySelector('[data-inbox-edit-input]'))", "打开失败编辑场景");
  await evaluate(cdp.send, `(() => {
    window.__inboxMock.failNextSave = true;
    document.querySelector('[data-inbox-edit-input]').value = '这次修改必须回滚';
    document.querySelector('[data-inbox-edit-form]').requestSubmit();
  })()`);
  await waitForCondition(cdp.send, "document.querySelector('[data-inbox-id] .inbox-content')?.textContent === '已完成一次有效编辑' && document.activeElement.matches('[data-inbox-edit]')", "编辑保存失败完成回滚与焦点恢复");
  const editFailureEvidence = await evaluate(cdp.send, `({
    visibleContent: document.querySelector('[data-inbox-id] .inbox-content')?.textContent,
    storedContent: JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items[0].content,
    focusRestored: document.activeElement.matches('[data-inbox-edit]')
  })`);

  /* 把最后一条测试记录调整到昨天，验证删除分组最后一条时日期标题会随之消失。 */
  const yesterdayItemId = await evaluate(cdp.send, `(() => {
    const state = JSON.parse(localStorage.getItem('command-shelf-inbox-regression'));
    const item = state.items.at(-1);
    const yesterday = new Date();
    yesterday.setDate(yesterday.getDate() - 1);
    item.createdAt = yesterday.toISOString();
    item.updatedAt = item.createdAt;
    localStorage.setItem('command-shelf-inbox-regression', JSON.stringify(state));
    return item.id;
  })()`);
  await cdp.send("Page.reload", { ignoreCache: true });
  await waitForCondition(cdp.send, "document.readyState === 'complete' && document.querySelector('#repository-button')?.textContent.includes('mock-command-data')", "重载删除测试数据");
  await evaluate(cdp.send, "document.querySelector('#inbox-nav-button').click()");
  await waitForCondition(cdp.send, "document.querySelector('#inbox-nav-count')?.textContent === '4' && [...document.querySelectorAll('.inbox-group-title')].some((title) => title.textContent === '昨天')", "形成昨天日期分组");

  const cancelDeleteEvidence = await evaluate(cdp.send, `(() => {
    window.confirm = () => false;
    const button = document.querySelector('[data-inbox-delete]');
    button.focus();
    button.click();
    return {
      count: document.querySelector('#inbox-nav-count').textContent,
      storedCount: JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items.length,
      focusStayed: document.activeElement === button
    };
  })()`);

  const failedDeleteId = await evaluate(cdp.send, `(() => {
    window.confirm = () => true;
    window.__inboxMock.failNextSave = true;
    const button = document.querySelector('[data-inbox-delete]');
    const itemId = button.dataset.inboxDelete;
    button.click();
    return itemId;
  })()`);
  await waitForCondition(cdp.send, `document.querySelector('#inbox-nav-count')?.textContent === '4' && !document.querySelector('#inbox-save-button').disabled && document.activeElement?.dataset.inboxDelete === '${failedDeleteId}'`, "删除保存失败完成顺序与焦点回滚");
  const deleteFailureEvidence = await evaluate(cdp.send, `({
    count: document.querySelector('#inbox-nav-count').textContent,
    storedCount: JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items.length,
    restoredId: document.activeElement?.dataset.inboxDelete,
    visibleIds: [...document.querySelectorAll('[data-inbox-id]')].map((item) => item.dataset.inboxId)
  })`);

  await evaluate(cdp.send, `document.querySelector('[data-inbox-delete="${yesterdayItemId}"]').click()`);
  await waitForCondition(cdp.send, "document.querySelector('#inbox-nav-count')?.textContent === '3' && !document.querySelector('#inbox-save-button').disabled", "删除昨天分组最后一条");
  const groupDeleteEvidence = await evaluate(cdp.send, `({
    storedCount: JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items.length,
    groupLabels: [...document.querySelectorAll('.inbox-group-title')].map((title) => title.textContent)
  })`);

  for (const expectedCount of [2, 1, 0]) {
    await evaluate(cdp.send, "document.querySelector('[data-inbox-delete]').click()");
    await waitForCondition(cdp.send, `document.querySelector('#inbox-nav-count')?.textContent === '${expectedCount}' && !document.querySelector('#inbox-save-button').disabled`, `连续删除后剩余 ${expectedCount} 条`);
  }
  const emptyDeleteEvidence = await evaluate(cdp.send, `({
    count: document.querySelector('#inbox-nav-count').textContent,
    storedCount: JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items.length,
    emptyText: document.querySelector('#inbox-state').textContent,
    emptyVisible: getComputedStyle(document.querySelector('#inbox-state')).display !== 'none'
  })`);
  await cdp.send("Page.reload", { ignoreCache: true });
  await waitForCondition(cdp.send, "document.readyState === 'complete' && document.querySelector('#repository-button')?.textContent.includes('mock-command-data')", "重启检查删除持久化");
  await evaluate(cdp.send, "document.querySelector('#inbox-nav-button').click()");
  await waitForCondition(cdp.send, "document.querySelector('#inbox-nav-count')?.textContent === '0'", "重启后保持空临时收集");
  const deleteRestartEvidence = await evaluate(cdp.send, `({
    count: document.querySelector('#inbox-nav-count').textContent,
    storedCount: JSON.parse(localStorage.getItem('command-shelf-inbox-regression')).items.length
  })`);

  const failures = [];
  if (!readOnlyEvidence.fixedEntryBeforeCategories) failures.push("临时收集入口不在分类目录之前");
  if (readOnlyEvidence.activeEntry !== "page") failures.push("临时收集入口没有选中语义");
  if (readOnlyEvidence.itemIds.length !== 5) failures.push("时间流没有按样例完整渲染");
  if (!readOnlyEvidence.groupLabels.includes("今天") || !readOnlyEvidence.groupLabels.includes("昨天")) failures.push("今天或昨天分组缺失");
  if (!readOnlyEvidence.linkHref?.startsWith("https://github.com/")) failures.push("内容链接没有转换为安全链接");
  if (!readOnlyEvidence.commandListHidden || !readOnlyEvidence.commandActionsHidden) failures.push("分类页控件仍显示在临时收集页");
  if (!readOnlyEvidence.newInboxVisible || !readOnlyEvidence.composerVisible) failures.push("新建速记入口或录入区不可见");
  if (compactViewport.hasHorizontalOverflow || largeViewport.hasHorizontalOverflow) failures.push("目标视口出现横向溢出");
  if (navigationEvidence.categoryTitle !== "Linux" || !navigationEvidence.actionsVisible || navigationEvidence.returnedTitle !== "临时收集") failures.push("分类与临时收集往返失败");
  if (blankEvidence.count !== "0" || !blankEvidence.error.includes("请先输入")) failures.push("空白内容未被拒绝");
  if (failureEvidence.count !== "3" || failureEvidence.storedCount !== 3 || failureEvidence.input !== "保存失败时保留我") failures.push("保存失败没有完整回滚并保留输入");
  if (!Object.values(busyEvidence).every(Boolean)) failures.push("保存期间没有禁用录入或同步入口");
  if (saveEvidence.count !== "4" || saveEvidence.firstContent !== "验证同步期间禁用" || saveEvidence.storedItems.length !== 4) failures.push("连续保存或重启恢复失败");
  if (cancelEvidence.storedContent !== "验证同步期间禁用" || cancelEvidence.visibleContent !== "验证同步期间禁用" || !cancelEvidence.focusRestored) failures.push("取消编辑改变了内容或没有恢复焦点");
  if (!escapeEvidence.editorClosed || !escapeEvidence.focusRestored) failures.push("Escape 没有取消编辑并恢复焦点");
  if (!emptyEditEvidence.editorOpen || !emptyEditEvidence.error.includes("不能为空")) failures.push("空白编辑没有被拒绝");
  if (editEvidence.stored.id !== beforeEdit.id || editEvidence.stored.createdAt !== beforeEdit.createdAt || editEvidence.stored.updatedAt === beforeEdit.updatedAt || editEvidence.visibleContent !== "已完成一次有效编辑" || !editEvidence.focusRestored) failures.push("有效编辑没有保持标识与创建时间或更新修改时间");
  if (editFailureEvidence.visibleContent !== "已完成一次有效编辑" || editFailureEvidence.storedContent !== "已完成一次有效编辑" || !editFailureEvidence.focusRestored) failures.push("编辑保存失败没有回滚内容与焦点");
  if (cancelDeleteEvidence.count !== "4" || cancelDeleteEvidence.storedCount !== 4 || !cancelDeleteEvidence.focusStayed) failures.push("取消删除改变了数据或焦点");
  if (deleteFailureEvidence.count !== "4" || deleteFailureEvidence.storedCount !== 4 || deleteFailureEvidence.restoredId !== failedDeleteId || deleteFailureEvidence.visibleIds.length !== 4) failures.push("删除保存失败没有恢复记录、顺序和焦点");
  if (groupDeleteEvidence.storedCount !== 3 || groupDeleteEvidence.groupLabels.includes("昨天")) failures.push("删除分组最后一条后日期分组没有更新");
  if (emptyDeleteEvidence.count !== "0" || emptyDeleteEvidence.storedCount !== 0 || !emptyDeleteEvidence.emptyVisible || !emptyDeleteEvidence.emptyText.includes("还没有临时记录")) failures.push("删除全部后空状态不正确");
  if (deleteRestartEvidence.count !== "0" || deleteRestartEvidence.storedCount !== 0) failures.push("删除结果没有在重启后保持");
  if (cdp.getExceptions().length > 0) failures.push("页面运行期间出现 JavaScript 异常");
  if (failures.length > 0) throw new Error(failures.join("；"));

  console.log(JSON.stringify({
    status: "passed",
    readOnlyEvidence,
    navigationEvidence,
    blankEvidence,
    failureEvidence,
    busyEvidence,
    saveEvidence,
    cancelEvidence,
    escapeEvidence,
    emptyEditEvidence,
    editEvidence,
    editFailureEvidence,
    cancelDeleteEvidence,
    deleteFailureEvidence,
    groupDeleteEvidence,
    emptyDeleteEvidence,
    deleteRestartEvidence,
    viewports: [compactViewport, largeViewport],
    screenshotPath,
    runtimeExceptions: cdp.getExceptions().length,
  }));
} finally {
  cdp?.close();
  if (browserProcess && !browserProcess.killed) browserProcess.kill();
  await fs.rm(browserProfile, { recursive: true, force: true }).catch(() => {});
}
