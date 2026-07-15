/**
 * 文件职责：验证正式命令复制计数的局部更新、串行保存与失败回滚。
 * 主要内容：用 Edge DevTools 注入最小 Tauri 契约，驱动真实前端复制入口并核对 DOM 身份与保存顺序。
 * 重要约束：浏览器配置和运行证据只写入项目 `.local`，不读取个人 APPDATA 或命令仓库。
 */

import fs from "node:fs/promises";
import path from "node:path";
import { spawn } from "node:child_process";
import { pathToFileURL } from "node:url";

/** 项目根目录，由脚本位置稳定反推，避免调用方工作目录影响测试。 */
const repositoryRoot = path.resolve(import.meta.dirname, "..");
/** 正式单文件前端入口。 */
const frontendUrl = pathToFileURL(path.join(repositoryRoot, "frontend", "index.html")).href;
/** 本轮隔离浏览器配置目录；进程号和时间戳避免并行验证互相污染。 */
const browserProfile = path.join(repositoryRoot, ".local", "copy-refresh-regression", `${process.pid}-${Date.now()}`);
/** 随机高位端口降低与本机既有调试会话碰撞的概率。 */
const debuggingPort = 9300 + Math.floor(Math.random() * 500);

/** 等待异步页面状态推进，避免使用固定长时间休眠。 */
function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

/**
 * 查找 Windows 自带 Edge；WebView2 桌面应用的验收机通常也具备该浏览器。
 * @returns {Promise<string>} 可执行文件绝对路径。
 */
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
      // 当前候选不存在时继续检查另一套标准安装目录。
    }
  }
  throw new Error("未找到 Microsoft Edge，无法执行复制无刷新回归测试");
}

/**
 * 轮询 DevTools 页面列表，直到隐藏浏览器完成启动。
 * @returns {Promise<object>} 包含页面 WebSocket 地址的 DevTools 目标。
 */
async function waitForPageTarget() {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${debuggingPort}/json/list`);
      const targets = await response.json();
      const page = targets.find((target) => target.type === "page" && target.webSocketDebuggerUrl);
      if (page) return page;
    } catch {
      // Edge 尚未监听端口时继续短暂轮询。
    }
    await delay(100);
  }
  throw new Error("15 秒内未发现复制回归测试页面");
}

/**
 * 建立本测试需要的轻量 CDP 客户端，并关联并发请求与响应。
 * @param {string} webSocketUrl DevTools 页面连接地址。
 * @returns {Promise<{send:Function, close:Function}>} CDP 请求接口和关闭函数。
 */
async function connectCdp(webSocketUrl) {
  const socket = new WebSocket(webSocketUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
  let nextRequestId = 1;
  const pendingRequests = new Map();
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data));
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
    close() {
      socket.close();
    },
  };
}

/**
 * 在页面主世界执行表达式，并把浏览器异常转换为明确的测试失败。
 * @param {Function} send CDP 命令发送函数。
 * @param {string} expression 待执行 JavaScript。
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
 * 等待页面条件成立；超时时携带业务场景，方便定位卡在哪个状态。
 * @param {Function} send CDP 命令发送函数。
 * @param {string} expression 返回真值的页面表达式。
 * @param {string} description 场景说明。
 * @returns {Promise<void>} 条件成立后完成。
 */
async function waitForCondition(send, expression, description) {
  const deadline = Date.now() + 12000;
  while (Date.now() < deadline) {
    if (await evaluate(send, expression)) return;
    await delay(100);
  }
  throw new Error(`等待超时：${description}`);
}

/**
 * 生成页面加载前注入的最小 Tauri 后端替身。
 * 替身保留完整保存参数，并返回与 Rust `AppSnapshot` 一致的快照，使测试进入真实桌面分支。
 * @returns {string} 可由 DevTools 在每次新文档创建前执行的脚本。
 */
function createTauriStub() {
  const document = {
    schemaVersion: 1,
    categories: [{
      id: "regression-category",
      name: "回归分类",
      description: "验证复制保存不刷新。",
      icon: "terminal",
      commands: [{
        id: "regression-command",
        title: "回归命令",
        command: "echo regression",
        description: "验证复制计数保存。",
        usage: "echo regression",
        parameters: [],
        outputExample: "regression",
        riskNote: "",
        notes: "",
        copyCount: 0,
      }],
    }],
  };
  return `(() => {
    const initialDocument = ${JSON.stringify(document)};
    window.__savedDocument = null;
    window.__saveRequests = [];
    window.__activeSaveCalls = 0;
    window.__maxActiveSaveCalls = 0;
    window.__savedRevision = 0;
    window.__failNextSave = false;
    window.__operationOrder = [];
    window.__pushDuringSave = false;
    window.__TAURI__ = { core: { invoke: async (command, args = {}) => {
      if (command === "load_app") {
        return {
          document: structuredClone(initialDocument),
          repositoryPath: "F:\\\\isolated-command-shelf-data",
          syncState: "synced",
          statusMessage: "隔离回归数据已加载。",
          documentHash: "hash-before-copy",
          error: null
        };
      }
      if (command === "save_document") {
        const request = {
          expectedHash: args.expectedHash,
          document: structuredClone(args.document),
          completed: false,
          failed: false
        };
        window.__saveRequests.push(request);
        window.__operationOrder.push("save:start");
        window.__activeSaveCalls += 1;
        window.__maxActiveSaveCalls = Math.max(window.__maxActiveSaveCalls, window.__activeSaveCalls);
        try {
          await new Promise((resolve) => setTimeout(resolve, 80));
          if (window.__failNextSave) {
            window.__failNextSave = false;
            request.failed = true;
            window.__operationOrder.push("save:failed");
            throw { message: "测试注入的保存失败。", action: "请重试复制。" };
          }
          window.__savedRevision += 1;
          window.__savedDocument = structuredClone(args.document);
          request.completed = true;
          window.__operationOrder.push("save:complete");
          return {
            document: structuredClone(args.document),
            repositoryPath: "F:\\\\isolated-command-shelf-data",
            syncState: "dirty",
            statusMessage: "复制次数已保存。",
            documentHash: "hash-after-save-" + window.__savedRevision,
            error: null
          };
        } finally {
          window.__activeSaveCalls -= 1;
        }
      }
      if (command === "start_push_repository") {
        window.__pushDuringSave = window.__activeSaveCalls > 0;
        window.__operationOrder.push("push:start");
        return {
          status: "completed",
          snapshot: {
            document: structuredClone(window.__savedDocument),
            repositoryPath: "F:\\\\isolated-command-shelf-data",
            syncState: "synced",
            statusMessage: "测试推送已完成。",
            documentHash: "hash-after-save-" + window.__savedRevision,
            error: null
          }
        };
      }
      throw new Error("测试替身不支持命令：" + command);
    } } };
  })();`;
}

/** 启动隐藏浏览器、执行复制路径并确保所有临时资源得到清理。 */
async function main() {
  await fs.mkdir(browserProfile, { recursive: true });
  const edgePath = await findEdge();
  const edge = spawn(edgePath, [
    "--headless=new",
    "--disable-gpu",
    `--remote-debugging-port=${debuggingPort}`,
    `--user-data-dir=${browserProfile}`,
    "about:blank",
  ], { windowsHide: true, shell: false, stdio: "ignore" });

  let client;
  try {
    const target = await waitForPageTarget();
    client = await connectCdp(target.webSocketDebuggerUrl);
    await client.send("Runtime.enable");
    await client.send("Page.enable");
    await client.send("Page.addScriptToEvaluateOnNewDocument", { source: createTauriStub() });
    await client.send("Page.navigate", { url: frontendUrl });
    await waitForCondition(
      client.send,
      "document.readyState === 'complete' && document.querySelectorAll('[data-command-id]').length === 1 && !syncState.operation",
      "正式命令加载完成",
    );

    const unchangedRenderMutations = await evaluate(client.send, `(async () => {
      const mutations = [];
      const observer = new MutationObserver((records) => {
        records.forEach((record) => {
          mutations.push({
            type: record.type,
            target: record.target.id || record.target.getAttribute?.('data-command-id') || record.target.tagName,
            attribute: record.attributeName || null
          });
        });
      });
      observer.observe(document.body, {
        subtree: true,
        attributes: true,
        childList: true,
        characterData: true
      });
      renderSync();
      await Promise.resolve();
      observer.disconnect();
      return mutations;
    })()`);
    if (unchangedRenderMutations.length > 0) {
      throw new Error(`同步状态没有变化时仍重写了 DOM：${JSON.stringify(unchangedRenderMutations)}`);
    }

    await evaluate(client.send, `(() => {
      window.__cardBeforeCopy = document.querySelector('[data-command-id]');
      window.__copyButtonBeforeCopy = document.querySelector('[data-copy-id]');
      window.__copyMutations = [];
      window.__describeCopyMutation = (record) => {
        const element = record.target.nodeType === Node.TEXT_NODE
          ? record.target.parentElement
          : record.target;
        return {
          type: record.type,
          id: element?.id || null,
          copyCountId: element?.closest?.('[data-copy-count-id]')?.dataset.copyCountId || null,
          attribute: record.attributeName || null
        };
      };
      window.__copyMutationObserver = new MutationObserver((records) => {
        records.forEach((record) => window.__copyMutations.push(window.__describeCopyMutation(record)));
      });
      window.__copyMutationObserver.observe(document.body, {
        subtree: true,
        attributes: true,
        childList: true,
        characterData: true
      });
      copyText = async () => {};
      window.__copyButtonBeforeCopy.click();
      return true;
    })()`);
    await waitForCondition(
      client.send,
      "document.querySelector('[data-copy-count-id]')?.textContent === '复制 1 次' && !syncState.operation && window.__savedDocument?.categories?.[0]?.commands?.[0]?.copyCount === 1",
      "复制次数保存完成",
    );

    const evidence = await evaluate(client.send, `(() => {
      window.__copyMutationObserver.takeRecords()
        .forEach((record) => window.__copyMutations.push(window.__describeCopyMutation(record)));
      window.__copyMutationObserver.disconnect();
      return {
        cardStillConnected: window.__cardBeforeCopy.isConnected,
        sameCard: window.__cardBeforeCopy === document.querySelector('[data-command-id]'),
        sameCopyButton: window.__copyButtonBeforeCopy === document.querySelector('[data-copy-id]'),
        countText: document.querySelector('[data-copy-count-id]').textContent,
        savedCount: window.__savedDocument.categories[0].commands[0].copyCount,
        syncLabel: document.getElementById('sync-status-label').textContent,
        copyButtonLabel: window.__copyButtonBeforeCopy.querySelector('span').textContent,
        copyButtonIcon: window.__copyButtonBeforeCopy.querySelector('use').getAttribute('href'),
        copyButtonClassChanged: window.__copyButtonBeforeCopy.classList.contains('is-copied'),
        toastVisible: document.getElementById('toast').classList.contains('is-visible'),
        mutations: window.__copyMutations
      };
    })()`);
    if (!evidence.cardStillConnected || !evidence.sameCard || !evidence.sameCopyButton) {
      throw new Error(`复制后命令卡片被整体替换：${JSON.stringify(evidence)}`);
    }
    const allowedSyncMutationIds = new Set(["sync-status", "sync-status-label", "sync-meta", "push-button"]);
    const unexpectedMutations = evidence.mutations.filter((mutation) => (
      mutation.copyCountId !== "regression-command" && !allowedSyncMutationIds.has(mutation.id)
    ));
    if (evidence.copyButtonLabel !== "复制" || evidence.copyButtonIcon !== "#icon-copy" || evidence.copyButtonClassChanged || evidence.toastVisible || unexpectedMutations.length > 0) {
      throw new Error(`正常复制改动了计数和同步状态以外的界面：${JSON.stringify({ evidence, unexpectedMutations })}`);
    }

    /* 连续复制与新增分类同时发生时，新增必须等待全部复制计数串行落盘。 */
    await evaluate(client.send, `(() => {
      window.__saveRequests = [];
      window.__maxActiveSaveCalls = 0;
      window.prompt = () => '串行保存分类';
      const copyButton = document.querySelector('[data-copy-id]');
      window.__copyButtonDuringQueue = copyButton;
      copyButton.click();
      document.getElementById('add-category-button').click();
      return true;
    })()`);
    await waitForCondition(
      client.send,
      "window.__activeSaveCalls > 0",
      "连续复制进入异步保存",
    );
    const inFlightEvidence = await evaluate(client.send, `({
      operation: syncState.operation,
      listReadonly: document.getElementById('command-list').dataset.readonly,
      copyConnected: window.__copyButtonDuringQueue.isConnected,
      copyDisabled: window.__copyButtonDuringQueue.disabled,
      addCategoryDisabled: document.getElementById('add-category-button').disabled
    })`);
    if (inFlightEvidence.operation || inFlightEvidence.listReadonly === "true" || !inFlightEvidence.copyConnected || inFlightEvidence.copyDisabled || inFlightEvidence.addCategoryDisabled) {
      throw new Error(`复制计数保存期间错误锁定了界面：${JSON.stringify(inFlightEvidence)}`);
    }
    await evaluate(client.send, `(() => {
      window.__copyButtonDuringQueue.click();
      window.__copyButtonDuringQueue.click();
      return true;
    })()`);
    await waitForCondition(
      client.send,
      "window.__activeSaveCalls === 0 && window.__savedDocument?.categories?.length === 2 && window.__savedDocument.categories[0].commands[0].copyCount === 4",
      "连续复制与后续分类保存全部完成",
    );

    const serialEvidence = await evaluate(client.send, `({
      maxActiveSaveCalls: window.__maxActiveSaveCalls,
      requests: window.__saveRequests.map((request) => ({
        expectedHash: request.expectedHash,
        copyCount: request.document.categories[0].commands[0].copyCount,
        categoryCount: request.document.categories.length,
        completed: request.completed,
        failed: request.failed
      })),
      savedCount: window.__savedDocument.categories[0].commands[0].copyCount
    })`);
    const requestsAreSerial = serialEvidence.requests.length === 3
      && serialEvidence.requests[0].expectedHash === "hash-after-save-1"
      && serialEvidence.requests[0].copyCount === 2
      && serialEvidence.requests[0].categoryCount === 1
      && serialEvidence.requests[1].expectedHash === "hash-after-save-2"
      && serialEvidence.requests[1].copyCount === 4
      && serialEvidence.requests[1].categoryCount === 1
      && serialEvidence.requests[2].expectedHash === "hash-after-save-3"
      && serialEvidence.requests[2].copyCount === 4
      && serialEvidence.requests[2].categoryCount === 2
      && serialEvidence.requests.every((request) => request.completed && !request.failed);
    if (serialEvidence.maxActiveSaveCalls !== 1 || !requestsAreSerial) {
      throw new Error(`复制与后续修改未按文档哈希串行保存：${JSON.stringify(serialEvidence)}`);
    }

    /* Git 推送复用同一顺序屏障，必须在剪贴板准备与计数写盘完成后才能启动。 */
    await evaluate(client.send, `(() => {
      document.querySelector('[data-category-id="regression-category"]').click();
      window.__operationOrder = [];
      window.__pushDuringSave = false;
      document.querySelector('[data-copy-id]').click();
      window.__pushPromise = runSyncOperation('push');
      return true;
    })()`);
    await waitForCondition(
      client.send,
      "window.__operationOrder.includes('push:start') && window.__activeSaveCalls === 0 && !syncState.operation && window.__savedDocument.categories[0].commands[0].copyCount === 5",
      "复制计数保存后启动 Git 推送",
    );
    const pushEvidence = await evaluate(client.send, `({
      operationOrder: window.__operationOrder,
      pushDuringSave: window.__pushDuringSave,
      savedCount: window.__savedDocument.categories[0].commands[0].copyCount,
      hasLocalChanges: syncState.hasLocalChanges
    })`);
    const saveCompletedAt = pushEvidence.operationOrder.indexOf("save:complete");
    const pushStartedAt = pushEvidence.operationOrder.indexOf("push:start");
    if (pushEvidence.pushDuringSave || saveCompletedAt < 0 || pushStartedAt <= saveCompletedAt || pushEvidence.savedCount !== 5 || pushEvidence.hasLocalChanges) {
      throw new Error(`Git 推送越过了复制计数保存屏障：${JSON.stringify(pushEvidence)}`);
    }

    /* 同批复制保存失败时，只撤销尚未落盘的增量，不破坏此前成功保存的累计值。 */
    await evaluate(client.send, `(() => {
      window.__failNextSave = true;
      const copyButton = document.querySelector('[data-copy-id]');
      copyButton.click();
      copyButton.click();
      return true;
    })()`);
    await waitForCondition(
      client.send,
      "window.__activeSaveCalls === 0 && document.querySelector('[data-copy-count-id]')?.textContent === '复制 5 次' && Boolean(syncState.error)",
      "失败批次回滚到最后成功计数",
    );
    const failureEvidence = await evaluate(client.send, `({
      visibleCount: document.querySelector('[data-copy-count-id]').textContent,
      savedCount: window.__savedDocument.categories[0].commands[0].copyCount,
      failedRequest: window.__saveRequests.at(-1).failed,
      maxActiveSaveCalls: window.__maxActiveSaveCalls
    })`);
    if (failureEvidence.visibleCount !== "复制 5 次" || failureEvidence.savedCount !== 5 || !failureEvidence.failedRequest || failureEvidence.maxActiveSaveCalls !== 1) {
      throw new Error(`复制保存失败回滚不完整：${JSON.stringify(failureEvidence)}`);
    }

    process.stdout.write(`${JSON.stringify({ status: "passed", ...evidence, inFlightEvidence, serialEvidence, pushEvidence, failureEvidence })}\n`);
  } finally {
    client?.close();
    edge.kill();
    await delay(300);
    await fs.rm(browserProfile, { recursive: true, force: true });
  }
}

await main();
