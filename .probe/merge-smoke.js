/* merge-smoke: 合并后的面板冒烟 —— 四个视图是否还能正常渲染、有没有 JS 报错。
 *
 * 合并把主线的自绘下拉、控件套件、设置页与本地分支的三图趋势、窗口选择器、
 * 果冻主题拼在一起，文本层面的冲突解决了不等于拼起来能跑：这里逐视图打开，
 * 收集 console 报错 / 未捕获异常，并核对关键节点是否真的画出了东西。
 */
const { spawn } = require("child_process");
const fs = require("fs");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const PORT_CDP = 9361;
const BASE = process.env.PROBE_BASE || "http://127.0.0.1:8892/webui";

const views = [
  { hash: "#/stats", name: "overview" },
  { hash: "#/attempts", name: "attempts" },
  { hash: "#/logs", name: "logs" },
  { hash: "#/settings", name: "settings" },
];

async function main() {
  const chrome = spawn(CHROME, [
    "--headless", "--disable-gpu", "--no-sandbox", "--use-gl=swiftshader",
    `--remote-debugging-port=${PORT_CDP}`, "--window-size=1440,900",
    "--user-data-dir=/tmp/cdp-profile-smoke", "about:blank",
  ], { stdio: "ignore" });
  process.on("exit", () => { try { chrome.kill(); } catch {} });

  let target = null;
  for (let i = 0; i < 40; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT_CDP}/json/list`)).json();
      target = list.find((t) => t.type === "page");
      if (target) break;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
  }
  if (!target) throw new Error("CDP 未就绪");

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (m, p) => new Promise((res, rej) => {
    const mid = ++id;
    ws.onmessage = (e) => {
      const x = JSON.parse(e.data);
      if (x.id === mid) x.error ? rej(new Error(x.error.message)) : res(x.result);
      else if (x.method === "Runtime.exceptionThrown") {
        const d = x.params.exceptionDetails;
        errors.push("EXCEPTION " + (d.exception && d.exception.description || d.text));
      } else if (x.method === "Runtime.consoleAPICalled" && x.params.type === "error") {
        errors.push("console.error " + x.params.args.map((a) => a.value || a.description || "").join(" "));
      } else if (x.method === "Log.entryAdded" && x.params.entry.level === "error") {
        errors.push("LOG " + x.params.entry.text + " " + (x.params.entry.url || ""));
      }
    };
    ws.send(JSON.stringify({ id: mid, method: m, params: p }));
  });
  const errors = [];
  await send("Runtime.enable", {});
  await send("Page.enable", {});
  await send("Log.enable", {});
  const evalJs = async (expr) =>
    (await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true })).result.value;

  let failed = 0;
  for (const v of views) {
    errors.length = 0;
    await send("Page.navigate", { url: `${BASE}/${v.hash}` });
    await new Promise((r) => setTimeout(r, 2500));
    const probe = await evalJs(`(() => {
      const q = (s) => document.querySelector(s);
      const n = (s) => document.querySelectorAll(s).length;
      const visible = [...document.querySelectorAll('.view')].filter(e => e.classList.contains('active')).map(e => e.id);
      return {
        activeView: visible,
        navItems: n('.nav-item'),
        theme: document.documentElement.getAttribute('data-theme'),
        metrics: n('#metric-cards .mcard, #metric-cards > *'),
        trendSvgs: ['trend-tokens','trend-cost','trend-rate'].map(id => { const e = document.getElementById(id); return e ? e.children.length : -1; }),
        donut: (document.getElementById('model-donut')||{children:[]}).children.length,
        modelRows: n('#model-body tr'),
        dropdowns: n('.sd-trigger'),
        selectsHidden: [...document.querySelectorAll('select')].filter(s => getComputedStyle(s).display === 'none').length,
        settingsFields: n('#set-fields .set-field, #set-fields > *'),
        themeCards: n('#theme-grid > *'),
        updTag: (document.getElementById('upd-tag')||{}).textContent || '',
        bodyScrollH: document.body.scrollHeight,
      };
    })()`);
    const { data } = await send("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(`.probe/shots/merge-${v.name}.png`, Buffer.from(data, "base64"));
    const bad = errors.filter((e) => !/favicon|429|net::ERR/.test(e));
    console.log(`\n── ${v.name} ${v.hash}`);
    console.log("   ", JSON.stringify(probe));
    if (bad.length) { failed++; console.log("    ✗ 报错:", bad.slice(0, 5)); } else console.log("    ✓ 无 JS 报错");
  }
  console.log(failed ? `\n${failed} 个视图有问题` : "\n四个视图空态全部正常");

  // ── 概览页带数据：把窗口挪到夹具账本有数据的区间，验证三图与模型表真的画出来 ──
  errors.length = 0;
  await send("Page.navigate", { url: `${BASE}/#/stats` });
  await new Promise((r) => setTimeout(r, 2500));
  const filled = await evalJs(`(async () => {
    document.getElementById('w-from').value = '2026-09-01T00:00';
    document.getElementById('w-to').value = '2026-11-04T00:00';
    document.getElementById('w-apply').click();
    await new Promise(r => setTimeout(r, 2500));
    const n = (s) => document.querySelectorAll(s).length;
    const kids = (id) => { const e = document.getElementById(id); return e ? e.querySelectorAll('*').length : -1; };
    return {
      modelRows: n('#model-body tr'),
      firstModel: (document.querySelector('#model-body tr b, #model-body tr td') || {}).textContent || '',
      tokenNodes: kids('trend-tokens'),
      costNodes: kids('trend-cost'),
      rateNodes: kids('trend-rate'),
      donutSegs: kids('model-donut'),
      windowLabel: (document.getElementById('window-range')||{}).textContent || '',
      foot: (document.getElementById('trend-foot')||{}).textContent || '',
    };
  })()`);
  const shot2 = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync(".probe/shots/merge-overview-data.png", Buffer.from(shot2.data, "base64"));
  console.log("\n── overview 带数据", JSON.stringify(filled));
  if (errors.length) console.log("    ✗ 报错:", errors.slice(0, 5));
  else console.log("    ✓ 无 JS 报错");

  ws.close();
  chrome.kill();
}
main().catch((e) => { console.error(e); process.exit(1); });
