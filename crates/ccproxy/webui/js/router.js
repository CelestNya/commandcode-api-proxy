/* 路由
 *
 * 按 hash 切换三个视图（概览/明细/日志），并负责日志轮询随视图开关。
 */

/* ── routing (sidebar) ── */
function route() {
  var h = location.hash || "#/stats";
  var view = h.indexOf("logs") >= 0 ? "logs"
           : h.indexOf("attempts") >= 0 ? "attempts"
           : h.indexOf("settings") >= 0 ? "settings" : "stats";
  ["stats", "attempts", "logs", "settings"].forEach(function (name) {
    el("view-" + name).classList.toggle("active", name === view);
    el("nav-" + name).classList.toggle("active", name === view);
  });
  syncNavIndicator();
  /* 趋势悬停明细浮层挂在 body 直属层（避开 backdrop-filter 的包含块），
     切视图时不会被面板一起藏住，这里显式收起。 */
  if (typeof hideTrendTip === "function") hideTrendTip();
  /* The log follow poll is only worth running while the screen is visible;
     route() is the one place that knows, so it owns start/stop. */
  if (view === "logs") startLogPolling(); else stopLogPolling();
  /* 设置页不轮询：进入时取一次文档与更新状态，之后只在用户动作时刷新。 */
  if (view === "settings" && typeof onEnterSettings === "function") onEnterSettings();
}
window.addEventListener("hashchange", route);

/* ── 导航滑块 ──
 *
 * 选中色由一块独立滑块承担（layout.css 的 .nav-indicator），切视图时它平移到
 * 新项，而不是两个项各自闪一下底色。
 *
 * 位置用 rect 差值而不是 offsetLeft：导航项的 offsetParent 其实是 .sidebar
 * （position: sticky 也算已定位），不是 .side-nav，用 offset* 会整体差一个
 * 侧栏原点。rect 差值两边都在视口坐标里，侧栏横向滚动时一起移动，仍然对。
 */
var navIndicatorReady = false;

function syncNavIndicator(instant) {
  var nav = document.querySelector(".side-nav");
  var bar = el("nav-indicator");
  if (!nav || !bar) return;
  var active = nav.querySelector(".nav-item.active");
  /* 极窄屏或视图还没排版时取不到尺寸，宁可先藏起来也不放一块 0×0 的色块。
     同时摘掉 nav-slid：留着它，选中项的兜底实底仍是透明的，滑块又没落位，
     界面上就没有任何选中态了。 */
  if (!active || !active.offsetWidth) {
    bar.style.opacity = "0";
    nav.classList.remove("nav-slid");
    return;
  }
  var nr = nav.getBoundingClientRect();
  var ar = active.getBoundingClientRect();
  var noAnim = instant || !navIndicatorReady;
  if (noAnim) bar.classList.add("no-anim");
  bar.style.width = ar.width + "px";
  bar.style.height = ar.height + "px";
  bar.style.transform = "translate3d(" + (ar.left - nr.left) + "px," + (ar.top - nr.top) + "px,0)";
  bar.style.opacity = "1";
  /* 滑块接管了才让选中项自己变透明（CSS 的 .side-nav.nav-slid）。放在同一个
     任务里切，两项同框同色，看不出交接那一帧。 */
  nav.classList.add("nav-slid");
  if (noAnim) {
    /* 读一次布局把上面几个值落定，再恢复过渡；否则首帧会从 0×0 滑过来 */
    void bar.offsetWidth;
    bar.classList.remove("no-anim");
    navIndicatorReady = true;
  }
}

/* 窗口缩放、窄屏换行都会改变项宽，滑块必须重新贴合；缩放期间不做动画，
   否则它会在拖动过程中追着走。 */
var navResizeFrame = 0;
window.addEventListener("resize", function () {
  if (navResizeFrame) return;
  navResizeFrame = requestAnimationFrame(function () {
    navResizeFrame = 0;
    syncNavIndicator(true);
  });
});
/* 字体晚到会改文本宽度（首次渲染用的是回退字体），布局稳定后静默再落一次位 */
if (document.fonts && document.fonts.ready) {
  document.fonts.ready.then(function () { syncNavIndicator(true); });
}
