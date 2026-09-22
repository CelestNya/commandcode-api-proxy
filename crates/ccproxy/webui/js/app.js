/* 装配
 *
 * DOMContentLoaded 时接上所有事件、首次取数并建立实时连接。
 */

/* ── wiring ── */
document.addEventListener("DOMContentLoaded", function () {
  route();
  el("days-7").addEventListener("click", function () { setDays(7); });
  el("days-14").addEventListener("click", function () { setDays(14); });
  el("days-30").addEventListener("click", function () { setDays(30); });
  function setDays(n) {
    state.days = n;
    [7, 14, 30].forEach(function (d) { el("days-" + d).classList.toggle("active", d === n); });
    // A deliberate window change is worth the draw-in; the live stream's
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

  /* Log screen: switching file re-points the follow stream; a manual scroll
     away from the bottom suspends auto-scroll, scrolling back re-arms it. */
  el("log-which").addEventListener("change", function () {
    logState.which = el("log-which").value;
    logState.lastSig = "";
    fetchLog();
    connectLogStream();
  });
  el("log-refresh").addEventListener("click", function () { fetchLog(); });
  el("log-autoscroll").addEventListener("click", function () {
    logState.autoScroll = !logState.autoScroll;
    el("log-autoscroll").classList.toggle("active", logState.autoScroll);
    if (logState.autoScroll) {
      var pane = el("log-pane");
      pane.scrollTop = pane.scrollHeight;
    }
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

  /* Astro-style static first: the snapshot shipped inside the HTML wins,
     no network round-trip before first paint; fetch only when absent. */
  var INIT = window.__INIT__;
  if (INIT && INIT.stats) { renderStats(INIT.stats, true); }
  else { fetchStats(true); }
  if (INIT && INIT.attempts) { state.attempts = INIT.attempts; renderAttempts(INIT.attempts); }
  else { fetchAttempts(); }
  fetchSysinfo();
  setInterval(fetchSysinfo, 5000);
  connect();
});
