/* 数据获取
 *
 * stats / attempts / sysinfo 的 fetch 封装。
 */

/* ── data ── */
/* The live snapshot ships a fixed 200-row window; the pager may ask for
   more, so direct fetches always cover the chosen page size. */
function attemptsLimit() { return Math.max(state.pageSize, 200); }
function fetchStats(animate) {
  return fetch("/webui/api/stats?days=" + state.days).then(function (r) { return r.json(); })
    .then(function (s) { renderStats(s, animate === true); })
    .catch(function () { /* next poll retries */ });
}
function fetchAttempts() {
  return fetch("/webui/api/attempts?limit=" + attemptsLimit()).then(function (r) { return r.json(); })
    .then(function (list) {
      state.attempts = Array.isArray(list) ? list : [];
      renderAttempts(state.attempts);
    })
    .catch(function () { /* next poll retries */ });
}
function fetchSysinfo() {
  fetch("/webui/api/sysinfo").then(function (r) { return r.json(); }).then(function (s) {
    el("chip-mem").textContent = (s.memMb == null ? "—" : s.memMb.toFixed(1) + " MB");
    el("chip-uptime").textContent = fmtUptime(s.uptimeSecs);
  }).catch(function () { /* transient; next tick retries */ });
}
