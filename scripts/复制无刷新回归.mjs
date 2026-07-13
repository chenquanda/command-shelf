/**
 * 文件职责：验证正式命令复制并保存计数时不会重建命令列表。
 * 主要内容：用 Edge DevTools 注入最小 Tauri 契约，驱动真实前端复制入口并核对 DOM 身份。
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
        window.__savedDocument = structuredClone(args.document);
        await new Promise((resolve) => setTimeout(resolve, 80));
        return {
          document: structuredClone(args.document),
          repositoryPath: "F:\\\\isolated-command-shelf-data",
          syncState: "dirty",
          statusMessage: "复制次数已保存。",
          documentHash: "hash-after-copy",
          error: null
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

    await evaluate(client.send, `(() => {
      window.__cardBeforeCopy = document.querySelector('[data-command-id]');
      window.__copyButtonBeforeCopy = document.querySelector('[data-copy-id]');
      copyText = async () => {};
      window.__copyButtonBeforeCopy.click();
      return true;
    })()`);
    await waitForCondition(
      client.send,
      "document.querySelector('[data-copy-count-id]')?.textContent === '复制 1 次' && !syncState.operation && window.__savedDocument?.categories?.[0]?.commands?.[0]?.copyCount === 1",
      "复制次数保存完成",
    );

    const evidence = await evaluate(client.send, `({
      cardStillConnected: window.__cardBeforeCopy.isConnected,
      sameCard: window.__cardBeforeCopy === document.querySelector('[data-command-id]'),
      sameCopyButton: window.__copyButtonBeforeCopy === document.querySelector('[data-copy-id]'),
      countText: document.querySelector('[data-copy-count-id]').textContent,
      savedCount: window.__savedDocument.categories[0].commands[0].copyCount,
      syncLabel: document.getElementById('sync-status-label').textContent
    })`);
    if (!evidence.cardStillConnected || !evidence.sameCard || !evidence.sameCopyButton) {
      throw new Error(`复制后命令卡片被整体替换：${JSON.stringify(evidence)}`);
    }
    process.stdout.write(`${JSON.stringify({ status: "passed", ...evidence })}\n`);
  } finally {
    client?.close();
    edge.kill();
    await delay(300);
    await fs.rm(browserProfile, { recursive: true, force: true });
  }
}

await main();
