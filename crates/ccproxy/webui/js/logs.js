/* 日志屏
 *
 * 日志渲染、级别着色、增量轮询与自动滚动。
 *
 * 跟随的机制是完整响应的增量轮询（api.js 的 fetchLogSince）：服务端只回
 * `since` 之后的新完整行，这里把它们 append 进面板，而不是整段重建
 * innerHTML——整段重建会丢滚动位置，正是旧版"手动滚动被实时更新打断"的
 * 根源。`reset`（日志轮转）时才整段重画。
 */

/* ── log screen ── */
var logState = { which: "proxy", pollTimer: null, autoScroll: true, len: 0, inFlight: false,
  levels: { DEBUG: true, INFO: true, WARN: true, ERROR: true } };
/* Keep the DOM bounded: the page never pages old logs (users open the file
   themselves), and an unbounded pane degrades into the jank this screen
   exists to avoid. 2000 rendered lines is a comfortable ceiling. */
var LOG_DOM_MAX = 2000;

function logLevel(line) {
  if (line.indexOf("[ERROR]") >= 0) return "lv-error";
  if (line.indexOf("[WARN]") >= 0) return "lv-warn";
  if (line.indexOf("[INFO]") >= 0) return "lv-info";
  if (line.indexOf("[DEBUG]") >= 0) return "lv-debug";
  return "lv-other";
}
function logLevelKey(line) {
  if (line.indexOf("[ERROR]") >= 0) return "ERROR";
  if (line.indexOf("[WARN]") >= 0) return "WARN";
  if (line.indexOf("[INFO]") >= 0) return "INFO";
  if (line.indexOf("[DEBUG]") >= 0) return "DEBUG";
  return "OTHER";
}
function logShown(line) {
  return logState.levels[logLevelKey(line)] !== false;
}

function logLineHtml(line) {
  /* Split off the leading ISO timestamp so it can be dimmed without touching
     the message; everything after it keeps the level's colour. */
  var m = /^(\d{4}-\d\d-\d\dT[\d:.]+Z)(\s*)(.*)$/.exec(line);
  var head = m ? '<span class="ts">' + esc(m[1]) + "</span>" + esc(m[2]) : "";
  var body = m ? m[3] : line;
  return '<span class="ln ' + logLevel(line) + '">' + head + esc(body) + "</span>";
}

/* 行与行之间不能插文本节点：`.ln` 是 block，换行靠它自己。
 *
 * 以前两处渲染都 `join("\n")`，而这里只删元素——每删一行就留下一个孤立的
 * "\n" 文本节点，永远清不掉。`.logpane` 是 `white-space: pre`，每个这样的
 * 文本节点都排成一个空行：实测 2000 行的面板里堆了 37,240 个文本节点，
 * scrollHeight 涨到 39,241 行的高度（真实内容只 2000 行），滚动条长度是假的，
 * 滚动时整块重排 —— 长任务到 11 秒，日志页就是在这里卡死的。
 * 保持"只有元素"这个不变量，下面的上限才真的成立。 */
function trimLogDom(pane) {
  while (pane.childElementCount > LOG_DOM_MAX) {
    pane.removeChild(pane.firstElementChild);
  }
}

function setFollowChip(on, text) {
  el("log-follow-chip").classList.toggle("on", on);
  el("log-follow-text").textContent = text;
}

function scrollLogToBottom() {
  var pane = el("log-pane");
  pane.scrollTop = pane.scrollHeight;
}

function renderLog(d, full) {
  if (!d) return;
  el("log-path").textContent = d.path || "—";
  el("log-path").setAttribute("title", d.path || "");
  el("log-meta").textContent = d.sizeBytes == null ? "" : fmtBytes(d.sizeBytes);
  var pane = el("log-pane");
  var empty = el("log-empty");
  if (!d.available) {
    pane.style.display = "none"; empty.style.display = "block";
    logState.len = 0;
    setFollowChip(false, "不可用");
    return;
  }
  pane.style.display = "block"; empty.style.display = "none";

  if (full || d.reset) {
    /* Full paint: first entry, the file switch, or a rotation that shrank the
       file under the client's offset. Rebuild once, then re-anchor. */
    var lines = (d.lines || "").split("\n").filter(function (l) { return l !== "" && logShown(l); });
    pane.innerHTML = lines.map(logLineHtml).join("");
    logState.len = d.len || 0;
    trimLogDom(pane);
    if (logState.autoScroll) scrollLogToBottom();
    setFollowChip(true, "实时");
    return;
  }

  if (d.len == null || d.len < logState.len) {
    /* Defensive: a shrink the server did not flag should never append. */
    fetchLogFull();
    return;
  }
  if (d.lines) {
    var atBottom = logState.autoScroll;
    var add = d.lines.split("\n").filter(function (l) { return l !== "" && logShown(l); })
      .map(logLineHtml).join("");
    if (add) {
      pane.insertAdjacentHTML("beforeend", add);
      trimLogDom(pane);
    }
    if (atBottom) scrollLogToBottom();
  }
  logState.len = d.len;
  setFollowChip(true, "实时");
}

function fmtBytes(n) {
  if (n == null) return "";
  if (n >= 1048576) return (n / 1048576).toFixed(2) + " MB";
  if (n >= 1024) return (n / 1024).toFixed(1) + " KB";
  return n + " B";
}

/* 轮询循环：只在日志视图可见时运行。上一次请求未返回前不发下一次，
   服务端挂了也不会堆积并发。 */
function pollLog() {
  if (logState.inFlight) return;
  logState.inFlight = true;
  var done = function () { logState.inFlight = false; };
  fetchLogSince(logState.len).then(done, done);
}

function startLogPolling() {
  stopLogPolling();
  setFollowChip(false, "连接中");
  logState.inFlight = false;
  fetchLogFull().then(function () {
    logState.pollTimer = setInterval(pollLog, 700);
  });
}

function stopLogPolling() {
  if (logState.pollTimer) { clearInterval(logState.pollTimer); logState.pollTimer = null; }
}
