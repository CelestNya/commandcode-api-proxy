/* 日志屏
 *
 * 日志渲染、级别着色、SSE 跟随流与自动滚动。
 */

/* ── log screen ── */
var logState = { which: "proxy", follow: null, autoScroll: true, lastSig: "" };

function logLevel(line) {
  if (line.indexOf("[ERROR]") >= 0) return "lv-error";
  if (line.indexOf("[WARN]") >= 0) return "lv-warn";
  if (line.indexOf("[INFO]") >= 0) return "lv-info";
  if (line.indexOf("[DEBUG]") >= 0) return "lv-debug";
  return "lv-other";
}

function renderLog(d) {
  if (!d) return;
  el("log-path").textContent = d.path || "—";
  el("log-path").setAttribute("title", d.path || "");
  el("log-meta").textContent = d.sizeBytes == null ? "" : fmtBytes(d.sizeBytes);
  var pane = el("log-pane");
  var empty = el("log-empty");
  if (!d.available) {
    pane.style.display = "none"; empty.style.display = "block"; return;
  }
  pane.style.display = "block"; empty.style.display = "none";

  /* Skip the rebuild when nothing changed: this fires on every size change, and
     re-writing the innerHTML would fight the user's manual scrolling. */
  if (d.lines === logState.lastSig) return;
  logState.lastSig = d.lines;

  var html = (d.lines || "").split("\n").map(function (line) {
    /* Split off the leading ISO timestamp so it can be dimmed without touching
       the message; everything after it keeps the level's colour. */
    var m = /^(\d{4}-\d\d-\d\dT[\d:.]+Z)(\s*)(.*)$/.exec(line);
    var head = m ? '<span class="ts">' + esc(m[1]) + "</span>" + esc(m[2]) : "";
    var body = m ? m[3] : line;
    return '<span class="ln ' + logLevel(line) + '">' + head + esc(body) + "</span>";
  }).join("\n");
  pane.innerHTML = html;
  if (logState.autoScroll) pane.scrollTop = pane.scrollHeight;
}

function fmtBytes(n) {
  if (n == null) return "";
  if (n >= 1048576) return (n / 1048576).toFixed(2) + " MB";
  if (n >= 1024) return (n / 1024).toFixed(1) + " KB";
  return n + " B";
}

function fetchLog() {
  fetch("/webui/api/logs?which=" + encodeURIComponent(logState.which))
    .then(function (r) { return r.json(); })
    .then(function (d) { logState.lastSig = ""; renderLog(d); })
    .catch(function () { /* transient; the follow stream retries */ });
}

function closeLogStream() {
  if (logState.follow) { logState.follow.close(); logState.follow = null; }
}

function connectLogStream() {
  closeLogStream();
  var es;
  try { es = new EventSource("/webui/logs/stream?which=" + encodeURIComponent(logState.which)); }
  catch (e) { return; }
  logState.follow = es;
  function setChip(on, text) {
    el("log-follow-chip").classList.toggle("on", on);
    el("log-follow-text").textContent = text;
  }
  es.onopen = function () { setChip(true, "跟随中"); };
  es.onerror = function () {
    setChip(false, "重连中");
    /* EventSource auto-reconnects unless it was closed; a closed stream (the
       server ended it, or we switched logs) is restored after a short pause. */
    if (es.readyState === EventSource.CLOSED) {
      setTimeout(function () {
        if (logState.follow === es && location.hash.indexOf("logs") >= 0) connectLogStream();
      }, 1500);
    }
  };
  /* The endpoint names its event `log`, so onmessage never fires — the handler
     must be attached to the named event. */
  es.addEventListener("log", function (ev) {
    var msg;
    try { msg = JSON.parse(ev.data); } catch (e) { return; }
    renderLog(msg);
  });
}

function connect() {
  var es;
  try { es = new EventSource("/webui/events"); } catch (e) { return; }
  es.onopen = function () { el("chip-live").classList.add("on"); el("chip-live-text").textContent = "实时"; };
  es.onerror = function () {
    el("chip-live").classList.remove("on"); el("chip-live-text").textContent = "重连中";
    if (es.readyState === EventSource.CLOSED) setTimeout(connect, 2000);
  };
  es.onmessage = function (ev) {
    var msg;
    try { msg = JSON.parse(ev.data); } catch (e) { return; }
    if (msg.attempts) {
      state.attempts = Array.isArray(msg.attempts) ? msg.attempts : [];
      renderAttempts(state.attempts);
      /* the snapshot ships a fixed-size window; top it up when the chosen
         page size needs more rows than it carries */
      if (state.attempts.length < attemptsLimit()) fetchAttempts();
    }
    if (msg.stats) {
      renderSummary(msg.stats);
      if (state.days === 14) renderTrend(msg.stats.trend || []);
      else if (Date.now() - state.daysFetchAt > 30000) { state.daysFetchAt = Date.now(); fetchStats(); }
    }
  };
}
