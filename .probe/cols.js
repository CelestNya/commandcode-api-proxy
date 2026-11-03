// 量 attempts 表格每列的实际渲染宽度与内容溢出情况
const { spawn } = require("child_process");
const CHROME = "C:/Users/CelestNya/AppData/Local/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-win64/chrome-headless-shell.exe";
const port = 9413;

const chrome = spawn(CHROME, [
  "--headless", "--disable-gpu", "--no-sandbox",
  `--remote-debugging-port=${port}`,
  "--user-data-dir=/tmp/cdp-cols",
  "about:blank",
], { stdio: "ignore" });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function run() {
  let target = null;
  for (let i = 0; i < 40; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      target = list.find((t) => t.type === "page");
      if (target) break;
    } catch {}
    await sleep(300);
  }
  if (!target) throw new Error("no target");

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const send = (method, params) => new Promise((res, rej) => {
    const mid = ++id;
    const handler = (e) => {
      const msg = JSON.parse(e.data);
      if (msg.id === mid) {
        ws.removeEventListener("message", handler);
        msg.error ? rej(new Error(msg.error.message)) : res(msg.result);
      }
    };
    ws.addEventListener("message", handler);
    ws.send(JSON.stringify({ id: mid, method, params }));
  });

  await send("Page.enable", {});
  await send("Runtime.enable", {});

  const widths = [1440, 1280, 1024];
  for (const w of widths) {
    await send("Emulation.setDeviceMetricsOverride", {
      width: w, height: 900, deviceScaleFactor: 1, mobile: false,
    });
    await send("Page.navigate", { url: "http://127.0.0.1:18924/webui/#/attempts" });
    await sleep(3500);

    const expr = `(function () {
      var t = document.querySelector(".table-wrap table");
      if (!t) return JSON.stringify({ error: "no table" });
      var ths = Array.prototype.slice.call(t.querySelectorAll("thead th"));
      var rows = Array.prototype.slice.call(t.querySelectorAll("tbody tr"));
      var cols = ths.map(function (th, i) {
        var idx = rows.length ? rows[0].children[i] : null;
        var cw = idx ? idx.getBoundingClientRect().width : 0;
        // 真实截断判定：内容盒宽度 = clientWidth - 左右内边距；
        // 文本实际宽度用 Range 量（td.scrollWidth 在表格里不可靠）。
        var clipped = false;
        var inner = 0;
        var textW = 0;
        if (idx) {
          var cs = getComputedStyle(idx);
          inner = idx.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
          var range = document.createRange();
          range.selectNodeContents(idx);
          var rects = range.getClientRects();
          for (var r = 0; r < rects.length; r++) {
            if (rects[r].width > 0) textW = Math.max(textW, rects[r].width);
          }
          clipped = textW > inner + 1;
        }
        return {
          name: th.textContent.trim(),
          thW: Math.round(th.getBoundingClientRect().width),
          cellW: Math.round(cw),
          inner: Math.round(inner),
          textW: Math.round(textW),
          overflow: clipped
        };
      });
      return JSON.stringify({
        tableW: Math.round(t.getBoundingClientRect().width),
        minW: t.style.minWidth,
        rowCount: rows.length,
        cols: cols
      });
    })()`;
    const res = await send("Runtime.evaluate", { expression: expr, returnByValue: true });
    const data = JSON.parse(res.result.value);
    console.log("\n=== viewport " + w + " ===");
    console.log("table width:", data.tableW, "| min-width:", data.minW, "| rows:", data.rowCount);
    for (const c of data.cols) {
      console.log(
        "  " + c.name.padEnd(8) +
        " w=" + String(c.thW).padStart(4) +
        " inner=" + String(c.inner).padStart(4) +
        " text=" + String(c.textW).padStart(4) +
        (c.overflow ? "   <-- 溢出截断" : "")
      );
    }
  }

  ws.close();
  chrome.kill();
}

run().then(() => process.exit(0)).catch((e) => {
  console.error("ERR", e.message);
  try { chrome.kill(); } catch {}
  process.exit(1);
});
