/* 数据获取
 *
 * stats / attempts / sysinfo / logs 的 fetch 封装。
 * 面板的"实时"全部建立在完整响应的轮询之上：tiny_http 只会在响应结束时
 * 刷缓冲，SSE 这类不结束的流在这里永远不会把数据送到浏览器——轮询是
 * 这个后端唯一可靠的推送原语。
 */

/* ── data ── */
/* The live snapshot ships a fixed 200-row window; the pager may ask for
   more, so direct fetches always cover the chosen page size. */
function attemptsLimit() { return Math.max(state.pageSize, 200); }

/* 每秒的轮询只在数据真的变化时才重建 DOM:指标卡/状态条/图表每秒重灌,
   CSS 入场动画就会跟着每秒重播(主页动效反复播放就是这个)。签名取自
   响应本身;animate=true 是刻意的动作(切窗口/首帧/刷新),总是重画。 */
var statsSig = "", attemptsSig = "";

function fetchStats(animate) {
  return fetch("/webui/api/stats?days=" + state.days).then(function (r) { return r.json(); })
    .then(function (s) {
      setLiveChip(true);
      var sig = JSON.stringify(s);
      if (animate !== true && sig === statsSig) return s;
      statsSig = sig;
      renderStats(s, animate === true);
    })
    .catch(function () { setLiveChip(false); /* next poll retries */ });
}
function fetchAttempts() {
  return fetch("/webui/api/attempts?limit=" + attemptsLimit()).then(function (r) { return r.json(); })
    .then(function (list) {
      state.attempts = Array.isArray(list) ? list : [];
      var sig = JSON.stringify(list);
      if (sig === attemptsSig) return;
      attemptsSig = sig;
      renderAttempts(state.attempts);
    })
    .catch(function () { /* next poll retries */ });
}
function fetchSysinfo() {
  fetch("/webui/api/sysinfo").then(function (r) { return r.json(); }).then(function (s) {
    el("chip-mem").textContent = (s.memMb == null ? "—" : s.memMb.toFixed(1) + " MB");
    el("chip-uptime").textContent = fmtUptime(s.uptimeSecs);
    if (s.version) el("ver").textContent = " v" + s.version;
  }).catch(function () { /* transient; next tick retries */ });
}

/* 顶栏的实时 chip:数据轮询成功即点亮(与日志页的跟随 chip 相互独立)。 */
function setLiveChip(on) {
  el("chip-live").classList.toggle("on", on);
  el("chip-live-text").textContent = on ? "实时" : "重连中";
}

/* 日志：since=0 拉全量尾巴（进入日志页 / 切换文件 / 轮转重置后），
   之后带着服务端返回的 len 做增量轮询。 */
function fetchLogFull() {
  return fetch("/webui/api/logs?which=" + encodeURIComponent(logState.which))
    .then(function (r) { return r.json(); })
    .then(function (d) { renderLog(d, true); })
    .catch(function () { setFollowChip(false, "重连中"); });
}
function fetchLogSince(since) {
  return fetch("/webui/api/logs?which=" + encodeURIComponent(logState.which) + "&since=" + since)
    .then(function (r) { return r.json(); })
    .then(function (d) { renderLog(d, false); })
    .catch(function () { setFollowChip(false, "重连中"); });
}
