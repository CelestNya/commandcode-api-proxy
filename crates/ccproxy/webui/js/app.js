/* 装配
 *
 * DOMContentLoaded 时接上所有事件、首次取数，并按视图开关各自的轮询。
 *
 * "实时"是视图级的完整响应轮询（概览 1s、明细 2s、日志 0.7s），且只在
 * 对应视图可见、页面处于前台时运行——后台标签页一个请求都不发。
 */

/* ── wiring ── */
function currentView() {
  var h = location.hash || "#/stats";
  return h.indexOf("logs") >= 0 ? "logs"
       : h.indexOf("attempts") >= 0 ? "attempts" : "stats";
}

/* 概览/明细的轮询走同一定时器：一个 tick 按当前视图取对应的数。 */
var statsTimer = null;
function startDataPolling() {
  if (statsTimer) return;
  statsTimer = setInterval(function () {
    if (document.hidden) return;
    var view = currentView();
    if (view === "stats") fetchStats(false);
    else if (view === "attempts") fetchAttempts();
  }, 1000);
}

document.addEventListener("DOMContentLoaded", function () {
  route();
  el("days-7").addEventListener("click", function () { setDays(7); });
  el("days-14").addEventListener("click", function () { setDays(14); });
  el("days-30").addEventListener("click", function () { setDays(30); });
  function setDays(n) {
    state.days = n;
    [7, 14, 30].forEach(function (d) { el("days-" + d).classList.toggle("active", d === n); });
    // A deliberate window change is worth the draw-in; the live poll's
    // once-a-second refresh is not (see renderTrend).
    fetchStats(true);
  }
  ["f-model", "f-stream", "f-status"].forEach(function (id) {
    el(id).addEventListener("change", function () {
      state.filters[id.replace("f-", "")] = el(id).value;
      state.page = 1;
      renderAttempts(state.attempts);
    });
  });
  el("btn-reset").addEventListener("click", function () {
    state.filters = { model: "", stream: "", status: "" };
    el("f-model").value = ""; el("f-stream").value = ""; el("f-status").value = "";
    state.page = 1;
    renderAttempts(state.attempts);
  });
  el("btn-refresh").addEventListener("click", function () { fetchAttempts(); fetchStats(); });

  /* Log screen: switching file restarts the follow loop; a manual scroll away
     from the bottom suspends auto-scroll, scrolling back re-arms it. */
  el("log-which").addEventListener("change", function () {
    logState.which = el("log-which").value;
    startLogPolling();
  });
  el("log-refresh").addEventListener("click", function () { startLogPolling(); });
  el("log-autoscroll").addEventListener("click", function () {
    logState.autoScroll = !logState.autoScroll;
    el("log-autoscroll").classList.toggle("active", logState.autoScroll);
    if (logState.autoScroll) scrollLogToBottom();
  });
  el("log-pane").addEventListener("scroll", function () {
    var pane = el("log-pane");
    var atBottom = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 24;
    /* Scrolling up suspends auto-scroll; scrolling back to the bottom re-arms
       it. While auto-scrolling we move scrollTop ourselves, and that event
       arrives already at the bottom, so it never flips the toggle off. */
    if (logState.autoScroll === atBottom) return;
    logState.autoScroll = atBottom;
    el("log-autoscroll").classList.toggle("active", atBottom);
  });

  /* Astro-style static first: the page itself carries no data (which is what
     makes its ETag stable), so hydrate from the JSON APIs in parallel — the
     round-trip is loopback-scale. */
  fetchStats(true);
  fetchAttempts();
  fetchSysinfo();
  if (currentView() === "logs") startLogPolling();
  setInterval(fetchSysinfo, 5000);
  startDataPolling();

  document.addEventListener("visibilitychange", function () {
    if (document.hidden) { stopLogPolling(); }
    else if (currentView() === "logs") { startLogPolling(); }
  });
});
