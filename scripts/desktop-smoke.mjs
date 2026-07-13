/**
 * 文件职责：通过 WebView2 DevTools 协议验证 CommandShelf S1～S5 的真实桌面入口。
 * 主要内容：连接正在运行的 Tauri 页面，检查仓库、编辑、同步、字段保真和故障恢复并保存截图。
 * 重要约束：脚本只面向隔离测试环境，不创建仓库、不修改正式 APPDATA，也不访问互联网。
 */

import fs from "node:fs/promises";
import { spawn } from "node:child_process";

/** DevTools 端口，由启动测试应用时的 WebView2 参数决定。 */
const port = Number.parseInt(process.argv[2] || "9223", 10);
/** 验证模式：由调用方选择一个已准备好隔离数据的端到端场景。 */
const mode = process.argv[3] || "connect";
/** connect 模式使用的临时本地 Git 仓库根路径。 */
const repositoryPath = process.argv[4] || "";
/** 当前场景截图的输出路径。 */
const screenshotPath = process.argv[5] || "S1桌面验证.png";

/**
 * 等待指定毫秒数，让页面加载或异步 Tauri 命令有机会完成。
 * @param {number} milliseconds 等待时长。
 * @returns {Promise<void>} 计时结束后完成。
 */
function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

/**
 * 在隔离测试仓库中执行无 Shell 的系统 Git，用于核对暂存区并修复测试身份。
 * @param {string} directory 已由桌面后端确认的临时仓库根目录。
 * @param {string[]} gitArguments 固定测试场景所需的 Git 参数数组。
 * @param {number[]} acceptedCodes 被视为预期结果的退出码。
 * @returns {Promise<{code:number, stdout:string, stderr:string}>} 有限测试命令结果。
 */
async function runGit(directory, gitArguments, acceptedCodes = [0]) {
  return new Promise((resolve, reject) => {
    const child = spawn("git", gitArguments, {
      cwd: directory,
      windowsHide: true,
      shell: false,
      env: { ...process.env, GIT_TERMINAL_PROMPT: "0", GCM_INTERACTIVE: "Never" },
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => {
      if (stdout.length < 65536) stdout += chunk.toString().slice(0, 65536 - stdout.length);
    });
    child.stderr.on("data", (chunk) => {
      if (stderr.length < 65536) stderr += chunk.toString().slice(0, 65536 - stderr.length);
    });
    child.on("error", reject);
    child.on("close", (code) => {
      const result = { code: code ?? -1, stdout: stdout.trim(), stderr: stderr.trim() };
      if (acceptedCodes.includes(result.code)) resolve(result);
      else reject(new Error(`git ${gitArguments.join(" ")} 失败：${result.stderr || `退出码 ${result.code}`}`));
    });
  });
}

/**
 * 轮询 WebView2 DevTools 列表，直到出现可自动化的页面目标。
 * @returns {Promise<object>} 包含 WebSocket 地址的页面目标。
 */
async function waitForPageTarget() {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/list`);
      const targets = await response.json();
      const page = targets.find((target) => target.type === "page" && target.webSocketDebuggerUrl);
      if (page) return page;
    } catch {
      // WebView2 进程刚启动时端口尚未监听，继续短暂轮询即可。
    }
    await delay(150);
  }
  throw new Error(`15 秒内未发现端口 ${port} 的 WebView2 页面`);
}

/**
 * 建立轻量 CDP 客户端，只实现本验收需要的请求与响应关联。
 * @param {string} webSocketUrl DevTools 页面目标地址。
 * @returns {Promise<{send: Function, close: Function}>} 可发送 CDP 命令的客户端。
 */
async function connectCdp(webSocketUrl) {
  const socket = new WebSocket(webSocketUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });

  /** 单调递增请求编号，保证并发命令响应不会串位。 */
  let nextRequestId = 1;
  /** 尚未收到响应的 CDP 请求。 */
  const pendingRequests = new Map();
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data));
    if (!message.id || !pendingRequests.has(message.id)) return;
    const pending = pendingRequests.get(message.id);
    pendingRequests.delete(message.id);
    if (message.error) pending.reject(new Error(message.error.message));
    else pending.resolve(message.result);
  });

  /**
   * 发送一个 CDP 请求并等待对应响应。
   * @param {string} method CDP 方法名。
   * @param {object} params 方法参数。
   * @returns {Promise<object>} CDP 返回结果。
   */
  function send(method, params = {}) {
    const id = nextRequestId++;
    return new Promise((resolve, reject) => {
      pendingRequests.set(id, { resolve, reject });
      socket.send(JSON.stringify({ id, method, params }));
    });
  }

  /** 关闭测试连接，不影响应用窗口生命周期。 */
  function close() {
    socket.close();
  }

  return { send, close };
}

/**
 * 在页面主世界执行 JavaScript，并把异常转换为测试失败。
 * @param {Function} send CDP 命令发送函数。
 * @param {string} expression 需要执行的表达式。
 * @returns {Promise<any>} 可序列化的表达式结果。
 */
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

/**
 * 轮询页面表达式，直到返回真值或达到超时。
 * @param {Function} send CDP 命令发送函数。
 * @param {string} expression 判断条件表达式。
 * @param {string} description 超时时显示的场景说明。
 * @returns {Promise<void>} 条件满足后完成。
 */
async function waitForCondition(send, expression, description) {
  const deadline = Date.now() + 12000;
  while (Date.now() < deadline) {
    if (await evaluate(send, expression)) return;
    await delay(150);
  }
  throw new Error(`等待超时：${description}`);
}

/**
 * 收集当前页面最关键的可观察状态，避免每个场景重复拼接 DOM 查询。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<object>} 可归档的界面状态。
 */
async function collectUi(send) {
  return evaluate(
    send,
    `({
      title: document.getElementById('category-title').textContent,
      status: document.getElementById('sync-status-label').textContent,
      statusMessage: document.getElementById('sync-meta').textContent,
      repository: document.getElementById('repository-button').textContent,
      repositoryDisabled: document.getElementById('repository-button').disabled,
      emptyTitle: document.querySelector('#empty-state h2').textContent,
      commandTitles: [...document.querySelectorAll('.command-title')].map((node) => node.textContent),
      addCategoryDisabled: document.getElementById('add-category-button').disabled,
      addCommandDisabled: document.getElementById('add-command-button').disabled,
      pullDisabled: document.getElementById('pull-button').disabled,
      pushDisabled: document.getElementById('push-button').disabled,
      consoleMode: Boolean(window.__TAURI__?.core?.invoke)
    })`,
  );
}

/**
 * 通过真实新增命令抽屉提交一条命令。
 * @param {Function} send CDP 命令发送函数。
 * @param {{title:string, command:string, note:string, parameters:string, output:string}} entry 表单内容。
 * @returns {Promise<void>} 自动保存完成后结束。
 */
async function addCommandThroughDrawer(send, entry) {
  const serialized = JSON.stringify(entry);
  await evaluate(
    send,
    `(() => {
      const entry = ${serialized};
      document.getElementById('add-command-button').click();
      document.getElementById('command-title-input').value = entry.title;
      document.getElementById('command-code-input').value = entry.command;
      document.getElementById('command-note-input').value = entry.note;
      document.getElementById('command-params-input').value = entry.parameters;
      document.getElementById('command-output-input').value = entry.output;
      document.getElementById('command-form').requestSubmit();
      return true;
    })()`,
  );
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '本地有修改' && !document.getElementById('drawer-layer').classList.contains('is-open')",
    `保存命令“${entry.title}”`,
  );
}

/**
 * 保存当前内容区截图。
 * @param {Function} send CDP 命令发送函数。
 * @param {string} outputPath PNG 输出路径。
 * @returns {Promise<void>} PNG 写入指定路径后结束。
 */
async function saveScreenshot(send, outputPath = screenshotPath) {
  // WebView2 在后台窗口更新错误提示时可能延迟合成，先激活页面并等待一帧稳定画面。
  await send("Page.bringToFront");
  await delay(250);
  const screenshot = await send("Page.captureScreenshot", { format: "png", fromSurface: true });
  await fs.writeFile(outputPath, Buffer.from(screenshot.data, "base64"));
}

/**
 * 通过正式仓库对话框连接调用方准备好的隔离 Git 克隆。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<void>} 表单提交完成；具体成功状态由场景继续等待和断言。
 */
async function connectRepositoryThroughDialog(send) {
  if (!repositoryPath) throw new Error(`${mode} 模式必须提供仓库路径`);
  const escapedPath = JSON.stringify(repositoryPath);
  await evaluate(
    send,
    `(() => {
      document.getElementById('repository-button').click();
      const input = document.getElementById('repository-path-input');
      input.value = ${escapedPath};
      input.dispatchEvent(new Event('input', { bubbles: true }));
      document.getElementById('repository-form').requestSubmit();
      return true;
    })()`,
  );
}

/**
 * 验证空数据连接或重启恢复场景。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<{ui:object, backend:object}>} 空数据证据。
 */
async function verifyEmptyRepository(send) {
  if (mode === "connect") {
    await waitForCondition(
      send,
      "document.getElementById('sync-status-label')?.textContent === '未选择仓库'",
      "首次启动进入未配置状态",
    );
    await connectRepositoryThroughDialog(send);
  }

  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '本地有修改' && !document.getElementById('repository-dialog-layer').classList.contains('is-open')",
    mode === "connect" ? "仓库连接和空数据初始化完成" : "重启后恢复仓库和状态",
  );
  const ui = await collectUi(send);
  const backend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");

  if (ui.title !== "还没有分类" || ui.status !== "本地有修改") {
    throw new Error(`桌面空数据视图不符合预期：${JSON.stringify(ui)}`);
  }
  if (ui.addCategoryDisabled || !ui.addCommandDisabled || !ui.pullDisabled || !ui.pushDisabled) {
    throw new Error(`空数据操作状态不符合当前 S2 边界：${JSON.stringify(ui)}`);
  }
  if (backend.document?.schemaVersion !== 1 || backend.document.categories.length !== 0) {
    throw new Error(`后端没有恢复第一版空文档：${JSON.stringify(backend)}`);
  }
  if (mode === "restart" && backend.initializedEmptyDocument) {
    throw new Error("重启恢复不应再次初始化已有 commands.json");
  }
  return { ui, backend };
}

/**
 * 走真实分类、命令、编辑、排序、复制和外部基线保护路径。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<{ui:object, backend:object, copiedValues:Array<string>, baselineProtected:boolean}>} S2 证据。
 */
async function verifyLocalEditing(send) {
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '本地有修改' && document.getElementById('category-title')?.textContent === '还没有分类'",
    "S2 从已连接空文档开始",
  );

  await evaluate(
    send,
    `(() => {
      window.prompt = () => 'Linux';
      document.getElementById('add-category-button').click();
      return true;
    })()`,
  );
  await waitForCondition(
    send,
    "document.getElementById('category-title')?.textContent === 'Linux' && document.getElementById('sync-status-label')?.textContent === '本地有修改'",
    "新增分类原子保存",
  );

  await addCommandThroughDrawer(send, {
    title: "查看当前运行进程",
    command: "ps aux",
    note: "查看系统当前全部进程。",
    parameters: "a = 显示所有终端上的进程\nu = 使用用户格式",
    output: "USER PID %CPU %MEM COMMAND",
  });
  await addCommandThroughDrawer(send, {
    title: "查看磁盘使用量",
    command: "df -h",
    note: "使用易读单位查看各文件系统容量。",
    parameters: "-h = 使用易读容量单位",
    output: "Filesystem Size Used Avail Use% Mounted on",
  });

  await evaluate(
    send,
    `(() => {
      document.querySelector('[data-edit-id]').click();
      document.getElementById('command-title-input').value = '查看全部运行进程';
      document.getElementById('command-note-input').value = '查看系统当前全部进程，并按资源占用判断状态。';
      document.getElementById('command-form').requestSubmit();
      return true;
    })()`,
  );
  await waitForCondition(
    send,
    "document.querySelector('.command-title')?.textContent === '查看全部运行进程' && document.getElementById('sync-status-label')?.textContent === '本地有修改'",
    "编辑命令原位保存",
  );

  await evaluate(
    send,
    `(() => {
      const items = [...document.querySelectorAll('[data-command-id]')];
      const transfer = new DataTransfer();
      items[1].dispatchEvent(new DragEvent('dragstart', { bubbles: true, dataTransfer: transfer }));
      items[0].dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: transfer }));
      items[1].dispatchEvent(new DragEvent('dragend', { bubbles: true, dataTransfer: transfer }));
      return true;
    })()`,
  );
  await waitForCondition(
    send,
    "document.querySelector('.command-title')?.textContent === '查看磁盘使用量' && document.getElementById('sync-status-label')?.textContent === '本地有修改'",
    "拖动排序保存",
  );

  await evaluate(
    send,
    `(() => {
      window.__copiedValues = [];
      copyText = async (text) => { window.__copiedValues.push(text); };
      document.querySelector('[data-copy-id]').click();
      document.querySelector('[data-copy-output-id]').click();
      return true;
    })()`,
  );
  await waitForCondition(send, "window.__copiedValues?.length === 2", "命令与参考输出分别复制");

  const ui = await collectUi(send);
  const backend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  const copiedValues = await evaluate(send, "window.__copiedValues");
  const commands = backend.document?.categories?.[0]?.commands || [];
  if (commands.length !== 2 || commands[0].title !== "查看磁盘使用量" || commands[1].title !== "查看全部运行进程") {
    throw new Error(`后端文档没有保存编辑与排序结果：${JSON.stringify(backend)}`);
  }
  if (copiedValues[0] !== "df -h" || !copiedValues[1].startsWith("Filesystem")) {
    throw new Error(`复制内容不符合当前首条命令：${JSON.stringify(copiedValues)}`);
  }

  await saveScreenshot(send);

  const documentPath = `${backend.repositoryPath}\\commands.json`;
  const originalBytes = await fs.readFile(documentPath);
  await fs.writeFile(documentPath, Buffer.concat([originalBytes, Buffer.from("\n")]));
  await evaluate(
    send,
    `(() => {
      window.prompt = () => '不应保存的分类';
      document.getElementById('add-category-button').click();
      return true;
    })()`,
  );
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '同步失败' && ![...document.querySelectorAll('.category-name')].some((node) => node.textContent === '不应保存的分类')",
    "外部基线变化后回滚界面",
  );
  await fs.writeFile(documentPath, originalBytes);

  return { ui, backend, copiedValues, baselineProtected: true };
}

/**
 * 验证 S2 编辑结果在完全重启后仍保持顺序和内容。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<{ui:object, backend:object}>} 重启持久化证据。
 */
async function verifyLocalRestart(send) {
  await waitForCondition(
    send,
    "document.querySelectorAll('.command-title').length === 2 && document.getElementById('sync-status-label')?.textContent === '本地有修改'",
    "重启恢复两个已保存命令",
  );
  const ui = await collectUi(send);
  const backend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  const commands = backend.document?.categories?.[0]?.commands || [];
  if (commands[0]?.title !== "查看磁盘使用量" || commands[1]?.title !== "查看全部运行进程") {
    throw new Error(`重启后的文档顺序不正确：${JSON.stringify(backend)}`);
  }
  return { ui, backend };
}

/**
 * 验证真实快进拉取，以及本地修改存在时的后端安全停止。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<object>} 拉取前后、失败保护与两张截图证据。
 */
async function verifyPullFlow(send) {
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '已同步' && !document.getElementById('pull-button').disabled",
    "干净数据仓库可以拉取",
  );
  const before = await collectUi(send);
  await evaluate(send, "document.getElementById('pull-button').click(); true");
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '已同步' && document.querySelector('.command-title')?.textContent === '来自电脑 A 的磁盘检查'",
    "远端有效文档完成快进并进入界面",
  );
  const pulledUi = await collectUi(send);
  const pulledBackend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  if (pulledBackend.syncState !== "synced" || pulledBackend.document.categories[0].commands[0].title !== "来自电脑 A 的磁盘检查") {
    throw new Error(`拉取后的后端快照不正确：${JSON.stringify(pulledBackend)}`);
  }
  await saveScreenshot(send, screenshotPath);

  await evaluate(
    send,
    `(() => {
      document.querySelector('[data-edit-id]').click();
      document.getElementById('command-title-input').value = '本地未推送的磁盘命令';
      document.getElementById('command-form').requestSubmit();
      return true;
    })()`,
  );
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '本地有修改' && document.querySelector('.command-title')?.textContent === '本地未推送的磁盘命令'",
    "制造已真实保存的本地修改",
  );
  await evaluate(send, "document.getElementById('pull-button').click(); true");
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '同步失败'",
    "本地修改阻止拉取",
  );
  const protectedUi = await collectUi(send);
  const protectedBackend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  if (protectedBackend.document.categories[0].commands[0].title !== "本地未推送的磁盘命令") {
    throw new Error(`失败拉取改变了本地数据：${JSON.stringify(protectedBackend)}`);
  }
  const protectedScreenshotPath = screenshotPath.replace(/\.png$/i, "-本地保护.png");
  await saveScreenshot(send, protectedScreenshotPath);
  return {
    before,
    pulledUi,
    pulledBackend,
    protectedUi,
    protectedBackend,
    protectedScreenshotPath,
  };
}

/**
 * 验证本地修改通过一次按钮操作完成提交和普通推送。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<object>} 推送前后界面与后端证据。
 */
async function verifyPushFlow(send) {
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '本地有修改' && !document.getElementById('push-button').disabled",
    "本地修改可以推送",
  );
  const before = await collectUi(send);
  await evaluate(send, "document.getElementById('push-button').click(); true");
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '已同步' && document.getElementById('push-button').disabled",
    "本地提交和普通推送完成",
  );
  const pushedUi = await collectUi(send);
  const pushedBackend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  if (pushedBackend.syncState !== "synced" || pushedBackend.document.categories[0].commands[0].title !== "本地未推送的磁盘命令") {
    throw new Error(`推送后的后端状态不正确：${JSON.stringify(pushedBackend)}`);
  }
  return { before, pushedUi, pushedBackend };
}

/**
 * 验证远端领先时，本地修改保留且普通推送在创建提交前停止。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<object>} 远端领先失败保护证据。
 */
async function verifyPushRemoteAhead(send) {
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '已同步'",
    "远端领先测试从本地干净状态开始",
  );
  await evaluate(
    send,
    `(() => {
      document.querySelector('[data-edit-id]').click();
      document.getElementById('command-title-input').value = '电脑 B 未推送的磁盘命令';
      document.getElementById('command-form').requestSubmit();
      return true;
    })()`,
  );
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '本地有修改' && document.querySelector('.command-title')?.textContent === '电脑 B 未推送的磁盘命令'",
    "保存电脑 B 本地修改",
  );
  await evaluate(send, "document.getElementById('push-button').click(); true");
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '同步失败'",
    "远端领先阻止普通推送",
  );
  const protectedUi = await collectUi(send);
  const protectedBackend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  if (protectedBackend.document.categories[0].commands[0].title !== "电脑 B 未推送的磁盘命令") {
    throw new Error(`远端领先失败后本地数据未保留：${JSON.stringify(protectedBackend)}`);
  }
  return { protectedUi, protectedBackend };
}

/**
 * 验证损坏机器配置或失效仓库路径在启动后显示持久错误，而不是伪装成未配置空数据。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<object>} 启动错误的界面与后端证据。
 */
async function verifyStartupError(send) {
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '同步失败'",
    "启动错误进入持久失败状态",
  );
  const ui = await collectUi(send);
  const backend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  const acceptedCodes = new Set(["CONFIG_INVALID", "PATH_NOT_FOUND"]);
  if (!acceptedCodes.has(backend.error?.code)) {
    throw new Error(`启动错误没有保留结构化原因：${JSON.stringify(backend)}`);
  }
  if (ui.repositoryDisabled || !ui.addCategoryDisabled || !ui.addCommandDisabled || !ui.pullDisabled || !ui.pushDisabled) {
    throw new Error(`失效配置仍开放了数据或同步写入口：${JSON.stringify(ui)}`);
  }
  return { ui, backend };
}

/**
 * 验证只改标题时的字段保真、同步中禁改与复制可用，以及 Git 身份修复后的直接重试。
 * @param {Function} send CDP 命令发送函数。
 * @returns {Promise<object>} S5 数据保真、互斥与恢复证据。
 */
async function verifyHardeningFlow(send) {
  if (await evaluate(send, "document.getElementById('sync-status-label')?.textContent === '未选择仓库'")) {
    await connectRepositoryThroughDialog(send);
  }
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '已同步' && document.querySelectorAll('.command-title').length === 1",
    "S5 从包含一条完整命令的干净仓库开始",
  );
  const before = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  const beforeCommand = before.document?.categories?.[0]?.commands?.[0];
  if (!beforeCommand) throw new Error(`S5 测试仓库缺少完整命令：${JSON.stringify(before)}`);
  const editedTitle = beforeCommand.title === "只修改标题后的磁盘命令"
    ? "只修改标题后的磁盘命令（复验）"
    : "只修改标题后的磁盘命令";
  const serializedEditedTitle = JSON.stringify(editedTitle);

  await evaluate(
    send,
    `(() => {
      document.querySelector('[data-edit-id]').click();
      document.getElementById('command-title-input').value = ${serializedEditedTitle};
      document.getElementById('command-form').requestSubmit();
      return true;
    })()`,
  );
  await waitForCondition(
    send,
    `document.getElementById('sync-status-label')?.textContent === '本地有修改' && document.querySelector('.command-title')?.textContent === ${serializedEditedTitle}`,
    "只修改标题并完成本地保存",
  );
  const preserved = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  const preservedCommand = preserved.document.categories[0].commands[0];
  for (const field of ["description", "notes", "usage", "riskNote"]) {
    if (preservedCommand[field] !== beforeCommand[field]) {
      throw new Error(`只改标题破坏了 ${field}：${JSON.stringify({ before: beforeCommand, after: preservedCommand })}`);
    }
  }

  const lockState = await evaluate(
    send,
    `(() => {
      document.querySelector('[data-edit-id]').click();
      window.__s5CopiedValues = [];
      copyText = async (text) => { window.__s5CopiedValues.push(text); };
      syncState.operation = 'pull';
      syncState.error = null;
      renderSync();
      const state = {
        ariaBusy: document.getElementById('sync-panel').getAttribute('aria-busy'),
        status: document.getElementById('sync-status-label').textContent,
        addCategoryDisabled: document.getElementById('add-category-button').disabled,
        addCommandDisabled: document.getElementById('add-command-button').disabled,
        editDisabled: document.querySelector('[data-edit-id]').disabled,
        draggable: document.querySelector('[data-command-id]').draggable,
        titleDisabled: document.getElementById('command-title-input').disabled,
        saveDisabled: document.getElementById('drawer-save').disabled,
        cancelDisabled: document.getElementById('drawer-cancel').disabled,
        copyDisabled: document.querySelector('[data-copy-id]').disabled
      };
      document.querySelector('[data-copy-id]').click();
      document.getElementById('command-title-input').value = '同步期间不应保存';
      document.getElementById('command-form').requestSubmit();
      state.formError = document.getElementById('form-error').textContent;
      return state;
    })()`,
  );
  await waitForCondition(send, "window.__s5CopiedValues?.length === 1", "同步期间仍可复制命令");
  const copiedValues = await evaluate(send, "window.__s5CopiedValues");
  const expectedLockState = lockState.ariaBusy === "true"
    && lockState.status === "正在拉取…"
    && lockState.addCategoryDisabled
    && lockState.addCommandDisabled
    && lockState.editDisabled
    && !lockState.draggable
    && lockState.titleDisabled
    && lockState.saveDisabled
    && !lockState.cancelDisabled
    && !lockState.copyDisabled
    && lockState.formError.includes("等待操作结束");
  if (!expectedLockState || copiedValues[0] !== beforeCommand.command) {
    throw new Error(`同步互斥或只读能力不符合预期：${JSON.stringify({ lockState, copiedValues })}`);
  }
  const lockedBackend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  if (lockedBackend.document.categories[0].commands[0].title !== editedTitle) {
    throw new Error(`同步期间表单绕过了禁改保护：${JSON.stringify(lockedBackend)}`);
  }
  await evaluate(
    send,
    `(() => {
      syncState.operation = null;
      syncState.error = null;
      syncState.statusMessage = '本地修改已保留，等待推送。';
      closeDrawer();
      render();
      return true;
    })()`,
  );

  await runGit(before.repositoryPath, ["config", "user.name", ""]);
  await runGit(before.repositoryPath, ["config", "user.email", ""]);
  await evaluate(send, "document.getElementById('push-button').click(); true");
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '同步失败' && document.getElementById('sync-meta')?.textContent.includes('Git user.name')",
    "缺少 Git 身份时安全停止",
  );
  const identityFailureUi = await collectUi(send);
  const identityFailureBackend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  await runGit(before.repositoryPath, ["diff", "--cached", "--quiet"]);
  if (identityFailureBackend.document.categories[0].commands[0].title !== editedTitle) {
    throw new Error(`身份失败后本地数据发生变化：${JSON.stringify(identityFailureBackend)}`);
  }
  const failureScreenshotPath = screenshotPath.replace(/\.png$/i, "-身份失败.png");
  await saveScreenshot(send, failureScreenshotPath);

  await runGit(before.repositoryPath, ["config", "user.name", "CommandShelf Test"]);
  await runGit(before.repositoryPath, ["config", "user.email", "commandshelf-test@example.invalid"]);
  await evaluate(send, "document.getElementById('push-button').click(); true");
  await waitForCondition(
    send,
    "document.getElementById('sync-status-label')?.textContent === '已同步' && document.getElementById('push-button').disabled",
    "修复 Git 身份后直接重试成功",
  );
  const retriedUi = await collectUi(send);
  const retriedBackend = await evaluate(send, "window.__TAURI__.core.invoke('load_app')");
  if (retriedBackend.syncState !== "synced" || retriedBackend.document.categories[0].commands[0].title !== editedTitle) {
    throw new Error(`身份修复后的推送结果不正确：${JSON.stringify(retriedBackend)}`);
  }

  return {
    before,
    editedTitle,
    preserved,
    lockState,
    copiedValues,
    identityFailureUi,
    identityFailureBackend,
    failureScreenshotPath,
    retriedUi,
    retriedBackend,
  };
}

/** 执行指定模式的桌面验收并打印可归档证据。 */
async function main() {
  if (!Number.isFinite(port)) throw new Error("DevTools 端口必须是数字");
  const knownModes = new Set([
    "connect",
    "restart",
    "local-edit",
    "local-restart",
    "pull-flow",
    "push-flow",
    "push-remote-ahead",
    "startup-error",
    "hardening-flow",
  ]);
  if (!knownModes.has(mode)) throw new Error(`未知验证模式：${mode}`);
  if (new Set(["connect", "hardening-flow"]).has(mode) && !repositoryPath) {
    throw new Error(`${mode} 模式必须提供仓库路径`);
  }

  const target = await waitForPageTarget();
  const client = await connectCdp(target.webSocketDebuggerUrl);
  try {
    await client.send("Runtime.enable");
    await client.send("Page.enable");
    await waitForCondition(
      client.send,
      "document.readyState === 'complete' && Boolean(window.__TAURI__?.core?.invoke)",
      "Tauri 页面和全局接口就绪",
    );

    const evidence = mode === "connect" || mode === "restart"
      ? await verifyEmptyRepository(client.send)
      : mode === "local-edit"
        ? await verifyLocalEditing(client.send)
        : mode === "local-restart"
          ? await verifyLocalRestart(client.send)
          : mode === "pull-flow"
            ? await verifyPullFlow(client.send)
            : mode === "push-flow"
              ? await verifyPushFlow(client.send)
              : mode === "push-remote-ahead"
                ? await verifyPushRemoteAhead(client.send)
                : mode === "startup-error"
                  ? await verifyStartupError(client.send)
                  : await verifyHardeningFlow(client.send);
    if (!new Set(["local-edit", "pull-flow"]).has(mode)) await saveScreenshot(client.send);
    process.stdout.write(`${JSON.stringify({ mode, targetUrl: target.url, ...evidence, screenshotPath }, null, 2)}\n`);
  } finally {
    client.close();
  }
}

await main();
