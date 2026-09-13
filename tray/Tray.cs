// CC Proxy Tray — personal tray manager for commandcode-api-proxy.
//
// 无主窗口：托盘图标 + 右键菜单（启动/停止/日志/开机自启/退出）。
// 代理作为 node 子进程运行，挂入 Job Object——托盘无论怎么死，子进程一并回收，
// 不会留下占用端口的孤儿。崩溃自动重启（3s）。
//
// 单实例 + 接管：互斥锁保证只有一个托盘；新实例启动时通知旧实例优雅退出
// （event 信号 → 旧实例停代理 → 释放锁 → 新实例等待端口释放后接续服务），
// 旧版本（不认识信号的）超时后按进程名兜底结束。
//
// 出站代理：默认读取 Windows 系统代理（注册表 ProxyEnable/ProxyServer），
// 注入 HTTPS_PROXY/HTTP_PROXY + NODE_USE_ENV_PROXY=1 给 node 子进程；
// NO_PROXY 恒含 localhost。环境变量 CC_PROXY=off 可关闭，CC_PROXY=<url> 可覆盖。
//
// Build (Windows 自带 .NET Framework 4.x 编译器，注意 csc 只支持 C# 5 语法):
//   csc /target:winexe /out:CCProxyTray.exe ^
//     /r:System.dll /r:System.Core.dll /r:System.Drawing.dll /r:System.Windows.Forms.dll ^
//     Tray.cs

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.Linq;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows.Forms;
using Microsoft.Win32;

namespace CCProxyTray
{
    static class Program
    {
        /**
         * 内部版本号。每次产出交接用的编译产物都要递增，便于确认「正在运行的
         * 是哪一个」，也让热更新时能一眼判断成败。与打包文件名的版本号一致。
         */
        public const string TrayVersion = "0.4.1-p2";
        // 实例命名空间：默认单实例；设置 CC_TRAY_NS 可跑隔离的测试实例，
        // 使交接协议能在不干扰生产托盘的前提下被真实验证。
        public static readonly string Ns = Environment.GetEnvironmentVariable("CC_TRAY_NS") ?? "";
        static string NsName(string baseName) { return Ns.Length == 0 ? baseName : baseName + "-" + Ns; }

        /**
         * 本实例的服务端口。生产端口 8787 是契约、定死不可改；只有带
         * CC_TRAY_NS 的隔离测试实例才允许用 CC_TRAY_PORT 换端口——换端口
         * 属于非生产行为，防止误用环境变量把生产实例挪走。
         */
        public static int InstancePort()
        {
            if (Ns.Length == 0) return 8787;
            var raw = Environment.GetEnvironmentVariable("CC_TRAY_PORT");
            int p;
            return int.TryParse(raw, out p) && p > 0 ? p : 8787;
        }

        public static readonly string MutexName = NsName("cc-proxy-tray");

        // ── 交接协议（两阶段提交，保证服务不出现真空）─────────────
        //  继任者(新实例)                              现任(旧实例)
        //   置 StandbyEvent ─────────────────────────► 停服务、释放锁、置 Ack
        //   取得互斥锁 ◄───────────────────────────── (Ack 已置)
        //   启动服务并验证端口可服务
        //   成功 → 置 CommitEvent ───────────────────► 确认已接班，退出
        //   失败 → 置 AbortEvent  ───────────────────► 回滚：重新取得锁、重启服务
        // 不变式：现任只有收到 Commit 才退出；否则超时后自行回滚继续服务。
        public static readonly string StandbyEventName = NsName("cc-proxy-tray-standby");
        public static readonly string CommitEventName = NsName("cc-proxy-tray-commit");
        public static readonly string AbortEventName = NsName("cc-proxy-tray-abort");

        const int OWNER_WAIT_MS = 45000;     // 继任者等锁 / 现任等定论
        const int SERVE_VERIFY_MS = 30000;   // 继任者验证自己在提供服务

        static Mutex mutex;
        static EventWaitHandle standbyEvent, commitEvent, abortEvent;

        static void TrayLog(string msg)
        {
            try
            {
                // 与 TrayContext 的日志位置保持一致（含测试命名空间），
                // 否则交接诊断会写到另一处，排查时看不到
                var dir = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "logs");
                if (Ns.Length > 0) dir = Path.Combine(dir, "test-" + Ns);
                Directory.CreateDirectory(dir);
                File.AppendAllText(Path.Combine(dir, "proxy.log"),
                    DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss ") + "[tray] " + msg + Environment.NewLine);
            }
            catch { }
        }

        // ── 所有权线程：Win32 互斥锁有线程亲和性 ──────────────────
        // Acquire 与 Release 必须在同一个线程上完成，否则 ReleaseMutex 会抛
        // 异常并被吞掉——锁从未真正释放，继任者永远拿不到。因此所有锁操作
        // 都投递到一个专用线程串行执行。
        sealed class OwnershipOp
        {
            public string Kind;                 // "acquire" | "release" | "exit"
            public int TimeoutMs;
            public volatile bool Acquired;
            public readonly ManualResetEvent Finished = new ManualResetEvent(false);
        }

        static readonly Queue<OwnershipOp> ownershipQueue = new Queue<OwnershipOp>();
        static readonly object ownershipLock = new object();
        static Thread ownershipThread;
        static readonly ManualResetEvent bootstrapDone = new ManualResetEvent(false);
        static volatile bool bootstrapCreatedNew;

        /// <summary>
        /// 所有权线程：互斥锁从创建到释放全部在这一个线程上完成。
        /// 首个动作即以 initiallyOwned:true 创建命名互斥锁——createdNew=false
        /// 表示已有实例持有它（本线程未取得所有权），据此判定继任者角色。
        /// </summary>
        static void StartOwnershipThread()
        {
            ownershipThread = new Thread(delegate()
            {
                bool created;
                mutex = new Mutex(true, MutexName, out created);
                bootstrapCreatedNew = created;
                bootstrapDone.Set();

                while (true)
                {
                    OwnershipOp op = null;
                    lock (ownershipLock)
                    {
                        if (ownershipQueue.Count > 0) op = ownershipQueue.Dequeue();
                    }
                    if (op == null) { Thread.Sleep(20); continue; }
                    if (op.Kind == "exit") { op.Finished.Set(); return; }
                    if (op.Kind == "acquire") op.Acquired = TryAcquireOnThisThread(op.TimeoutMs);
                    else if (op.Kind == "release") { try { mutex.ReleaseMutex(); } catch { } }
                    op.Finished.Set();
                }
            });
            ownershipThread.IsBackground = true;
            ownershipThread.Start();
            bootstrapDone.WaitOne(10000);
        }

        static bool TryAcquireOnThisThread(int timeoutMs)
        {
            var sw = Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < timeoutMs)
            {
                try { if (mutex.WaitOne(0)) return true; }
                catch (AbandonedMutexException) { return true; }
                Thread.Sleep(200);
            }
            return false;
        }

        /** 取得锁（在所有权线程上执行，保证与释放同线程）。 */
        static bool TryAcquire(int timeoutMs)
        {
            var op = new OwnershipOp { Kind = "acquire", TimeoutMs = timeoutMs };
            lock (ownershipLock) ownershipQueue.Enqueue(op);
            op.Finished.WaitOne(timeoutMs + 5000);
            return op.Acquired;
        }

        /** 释放锁（在所有权线程上执行）。 */
        static void ReleaseOwnership()
        {
            var op = new OwnershipOp { Kind = "release" };
            lock (ownershipLock) ownershipQueue.Enqueue(op);
            op.Finished.WaitOne(5000);
        }

        /** 端口是否真的在提供服务——接班是否成立的唯一判据。 */
        static bool PortServing(int port)
        {
            try
            {
                using (var c = new TcpClient())
                {
                    var ar = c.BeginConnect(System.Net.IPAddress.Loopback, port, null, null);
                    if (!ar.AsyncWaitHandle.WaitOne(1000)) return false;
                    c.EndConnect(ar);
                    return true;
                }
            }
            catch { return false; }
        }

        /** 只读探测是否有另一个托盘在运行；绝不干扰对方（自检安全用）。 */
        static bool AnotherInstanceRunning()
        {
            try
            {
                bool created;
                using (var probe = new Mutex(true, MutexName, out created))
                {
                    if (!created) return true;
                    try { probe.ReleaseMutex(); } catch { }
                    return false;
                }
            }
            catch { return false; }
        }

        [DllImport("kernel32.dll")]
        static extern bool AttachConsole(int dwProcessId);
        const int ATTACH_PARENT_PROCESS = -1;

        // Job Object: 托盘进程死亡（含硬杀）时回收子进程
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern IntPtr CreateJobObject(IntPtr lpJobAttributes, string lpName);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool SetInformationJobObject(IntPtr hJob, int infoType, IntPtr lpInfo, int length);
        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool AssignProcessToJobObject(IntPtr hJob, IntPtr hProcess);

        const int JobObjectExtendedLimitInformation = 9;
        const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000;

        [StructLayout(LayoutKind.Sequential)]
        struct IO_COUNTERS
        {
            public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
            public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
        }
        [StructLayout(LayoutKind.Sequential)]
        struct JOBOBJECT_BASIC_LIMIT_INFORMATION
        {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public ulong MinimumWorkingSetSize;
            public ulong MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public IntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }
        [StructLayout(LayoutKind.Sequential)]
        struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
        {
            public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
            public IO_COUNTERS IoInfo;
            public ulong ProcessMemoryLimit;
            public ulong JobMemoryLimit;
            public ulong PeakProcessMemoryUsed;
            public ulong PeakJobMemoryUsed;
        }

        static IntPtr jobHandle;

        static void CreateJob()
        {
            jobHandle = CreateJobObject(IntPtr.Zero, null);
            if (jobHandle == IntPtr.Zero) return;
            var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            var size = Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION));
            var ptr = Marshal.AllocHGlobal(size);
            try
            {
                Marshal.StructureToPtr(info, ptr, false);
                SetInformationJobObject(jobHandle, JobObjectExtendedLimitInformation, ptr, size);
            }
            finally { Marshal.FreeHGlobal(ptr); }
        }

        [STAThread]
        static void Main()
        {
            var rawArgs = Environment.GetCommandLineArgs();
            bool selfCheck = rawArgs.Length > 1 && rawArgs[1] == "--selfcheck";
            if (selfCheck) AttachConsole(ATTACH_PARENT_PROCESS);

            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            CreateJob();

            // 锁的创建/获取/释放全部在专用所有权线程上完成（Win32 互斥锁有
            // 线程亲和性，跨线程释放会静默失败，导致继任者永远拿不到锁）。
            standbyEvent = new EventWaitHandle(false, EventResetMode.ManualReset, StandbyEventName);
            commitEvent = new EventWaitHandle(false, EventResetMode.ManualReset, CommitEventName);
            abortEvent = new EventWaitHandle(false, EventResetMode.ManualReset, AbortEventName);
            StartOwnershipThread();
            bool createdNew = bootstrapCreatedNew;

            bool isSuccessor = !createdNew;   // 已有实例 = 我是继任者

            // ── 自检：绝不干扰正在运行的实例 ──────────────────────
            if (selfCheck)
            {
                if (isSuccessor)
                {
                    Console.WriteLine("SELFCHECK_SKIP 检测到运行中的托盘实例；自检拒绝接管，未做任何改动");
                    Environment.Exit(0);
                }
                var probe = new TrayContext(false, true);
                try { probe.SelfCheck(); Environment.Exit(0); }
                catch (Exception ex) { Console.WriteLine("SELFCHECK_FAIL " + ex); Environment.Exit(2); }
            }

            if (isSuccessor)
            {
                // ═══ 阶段 1：请求现任让位 ═══
                TrayLog("交接：请求现任让位");
                standbyEvent.Set();
                commitEvent.Reset();
                abortEvent.Reset();
                if (!TryAcquire(OWNER_WAIT_MS))
                {
                    // 现任没让位（可能是不认识协议的旧版本）→ 再等一轮后放弃，
                    // 绝不动武杀进程：宁可这次更新失败，也不能留下服务真空。
                    TrayLog("交接失败：现任未在 " + OWNER_WAIT_MS + "ms 内让位，本次更新中止");
                    MessageBox.Show(
                        "已有实例未能让位，本次启动已取消。\n请在托盘菜单中选择「退出」后重试。",
                        "CC Proxy Tray", MessageBoxButtons.OK, MessageBoxIcon.Warning);
                    standbyEvent.Reset();
                    Environment.Exit(1);
                }
                TrayLog("交接：已取得所有权，启动服务");
            }
            else
            {
                // 我是首任实例
                standbyEvent.Reset();
            }

            // 进程内私有的退出信号（必须匿名！）。
            // 若用命名事件，上一任实例「让自己退出」的信号会通过同名内核对象
            // 泄漏给继任者，把刚接班的实例一起关掉——交接测试实测到这个缺陷。
            var shutdownEvent = new EventWaitHandle(false, EventResetMode.ManualReset);

            // 先建上下文（含首任/继任者角色），再启动让位监视线程
            var ctx = new TrayContext(isSuccessor, false);

            // 让位监视：收到 Standby → 暂停服务(进程存活)、释放锁 → 等继任者定论
            var handover = new Thread(delegate() { OwnerHandoverLoop(isSuccessor, ctx, shutdownEvent); });
            handover.IsBackground = true;
            handover.Start();

            var watcher = new Thread(delegate() { shutdownEvent.WaitOne(); ctx.RequestQuit(); });
            watcher.IsBackground = true;
            watcher.Start();
            Application.Run(ctx);

            ReleaseOwnership();
            mutex.Dispose();
            shutdownEvent.Dispose();
        }

        /// <summary>
        /// 让位监视线程。职责分离：
        ///  - 首任实例(owner)：收到 Standby → 暂停服务(进程存活) → 释放锁 →
        ///    等 Commit/Abort：Commit 则退出，Abort 则重启服务继续当班。
        ///  - 继任者(successor)：启动后轮询端口确认自己在服务 → 广播 Commit；
        ///    超时未服务则广播 Abort 并自行退出，把服务还给现任。
        /// </summary>
        static void OwnerHandoverLoop(bool isSuccessor, TrayContext ctx, EventWaitHandle shutdownEvent)
        {
            if (isSuccessor)
            {
                var sw = Stopwatch.StartNew();
                while (sw.ElapsedMilliseconds < SERVE_VERIFY_MS)
                {
                    if (PortServing(InstancePort()))
                    {
                        TrayLog("交接成功：已在 :" + InstancePort() + " 提供服务，通知现任退出");
                        commitEvent.Set();
                        return;
                    }
                    Thread.Sleep(500);
                }
                TrayLog("交接失败：未能在期限内提供服务，通知现任回滚");
                abortEvent.Set();
                return;
            }

            // 在职实例：等待让位请求
            while (true)
            {
                if (!standbyEvent.WaitOne(1000)) continue;
                if (commitEvent.WaitOne(0)) { shutdownEvent.Set(); return; }

                TrayLog("交接：收到让位请求，暂停服务并释放所有权");
                if (!ctx.PauseForHandover(15000))
                    TrayLog("交接警告：暂停服务超时，仍继续让出所有权");
                ReleaseOwnership();

                // 让位后等继任者定论；这是不变式的核心：没有 Commit 就不退场
                var sw = Stopwatch.StartNew();
                while (sw.ElapsedMilliseconds < OWNER_WAIT_MS)
                {
                    if (commitEvent.WaitOne(50))
                    {
                        TrayLog("交接完成：继任者已接班，本实例退出");
                        shutdownEvent.Set();
                        return;
                    }
                    if (abortEvent.WaitOne(50))
                    {
                        TrayLog("交接回滚：继任者失败，重新取得所有权并恢复服务");
                        if (!TryAcquire(15000))
                            TrayLog("交接回滚：未能重新取得所有权（另一实例已接管）");
                        commitEvent.Reset();
                        standbyEvent.Reset();
                        ctx.ResumeAfterHandover(20000);
                        break; // 回到外层循环，继续当班
                    }
                }
                if (commitEvent.WaitOne(0)) { shutdownEvent.Set(); return; }
                if (abortEvent.WaitOne(0)) continue; // 已回滚，继续监听让位请求

                // 超时无人定论：不让自己停在“已释放锁但仍在运行”的悬空状态
                TrayLog("交接超时：未收到继任者定论，重新取得所有权并恢复服务");
                if (TryAcquire(15000)) { commitEvent.Reset(); standbyEvent.Reset(); }
                ctx.ResumeAfterHandover(20000);
            }
        }

        public static void AssignToJob(Process p)
        {
            if (jobHandle != IntPtr.Zero)
            {
                try { AssignProcessToJobObject(jobHandle, p.Handle); } catch { }
            }
        }
    }

    class TrayContext : ApplicationContext
    {
        const string LogFileName = "proxy.log";
        const long LogMaxBytes = 2 * 1024 * 1024;

        readonly NotifyIcon icon;
        readonly string projectRoot;
        readonly System.Windows.Forms.Timer quitPoll;
        Process proxy;
        bool quitting;          // 用户要求退出 —— 不自动重启
        bool manualStop;        // 用户要求停止 —— 不自动重启
        volatile bool quitRequested;
        readonly object gate = new object();

        public TrayContext(bool tookOver, bool selfCheckOnly = false)
        {
            projectRoot = ResolveProjectRoot();

            // 先建菜单再建图标：RefreshMenu 会触碰 NotifyIcon
            var strip = BuildMenu();
            icon = new NotifyIcon
            {
                Icon = MakeIcon(Color.Gray),
                Text = "CC Proxy — stopped",
                Visible = true,
                ContextMenuStrip = strip,
            };
            icon.DoubleClick += delegate { OpenLog(); };

            // 接管信号经非 UI 线程置位，由 UI Timer 轮询执行退出（线程安全）
            quitPoll = new System.Windows.Forms.Timer();
            quitPoll.Interval = 300;
            quitPoll.Tick += delegate
            {
                // 交接请求统一在 UI 线程执行，避免跨线程操作控件
                if (handoverPause)
                {
                    handoverPause = false;
                    try { DoPauseForHandover(); } catch { }
                    pauseDone.Set();
                }
                if (handoverResume)
                {
                    handoverResume = false;
                    try { DoResumeAfterHandover(); } catch { }
                    resumeDone.Set();
                }
                if (quitRequested) Quit();
            };
            quitPoll.Start();

            Directory.CreateDirectory(LogDir());
            // 自检模式不启动代理：否则会抢 8787 端口、干扰正在运行的实例
            if (selfCheckOnly)
            {
                RefreshMenu();
                return;
            }
            if (tookOver)
            {
                WaitForPortFree(Port(), 8000);
                AppendLog("[tray] 接管：旧实例已退出，端口已释放");
            }
            LogEgressDecision();
            StartProxy(); // 托盘起来 = 代理起来
            RefreshMenu();
        }

        public void RequestQuit() { quitRequested = true; }

        // ── 交接：暂停/恢复服务（进程保持存活，以便失败时回滚）──────
        // 所有 UI 控件访问都经 quitPoll（UI 线程）执行，避免跨线程设置属性。
        volatile bool handoverPause;
        volatile bool handoverResume;
        readonly ManualResetEvent pauseDone = new ManualResetEvent(false);
        readonly ManualResetEvent resumeDone = new ManualResetEvent(false);

        /// <summary>停掉代理子进程并释放端口，但托盘进程继续存活以便回滚。</summary>
        public bool PauseForHandover(int timeoutMs)
        {
            pauseDone.Reset();
            handoverPause = true;
            return pauseDone.WaitOne(timeoutMs);
        }

        /// <summary>交接失败后重新提供服务。</summary>
        public bool ResumeAfterHandover(int timeoutMs)
        {
            resumeDone.Reset();
            handoverResume = true;
            return resumeDone.WaitOne(timeoutMs);
        }

        void DoPauseForHandover()
        {
            lock (gate)
            {
                manualStop = true;   // 阻止 OnProxyExited 自动重启
                var p = proxy;
                if (p != null && !p.HasExited)
                {
                    try { p.Kill(); p.WaitForExit(5000); } catch { }
                }
                // 确认端口已释放，继任者才能立即绑定
                WaitForPortFree(Port(), 10000);
            }
            AppendLog("[tray] 交接：服务已暂停，端口已释放");
            RefreshMenu();
        }

        void DoResumeAfterHandover()
        {
            lock (gate) { manualStop = false; }
            AppendLog("[tray] 交接回滚：重新启动服务");
            StartProxy();
            RefreshMenu();
        }

        /// <summary>自检：真实执行「构建菜单 + 展开刷新」这段代码路径，
        /// 把结果写到 stdout，用于验证右键菜单不会崩。</summary>
        public void SelfCheck()
        {
            var strip = BuildMenu();
            RefreshCacheMenu();
            Console.WriteLine("menu.items=" + strip.Items.Count);
            Console.WriteLine("menu.headline=" + menuCache.Text);
            Console.WriteLine("menu.detail=" + menuCacheDetail.Text);
            var onOpening = typeof(ToolStripDropDown).GetMethod("OnOpening",
                System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic);
            onOpening.Invoke(strip, new object[] { new System.ComponentModel.CancelEventArgs() });
            Console.WriteLine("menu.after_opening=" + menuCache.Text);
            Console.WriteLine("SELFCHECK_OK");
        }

        ContextMenuStrip BuildMenu()
        {
            var strip = new ContextMenuStrip();
            // 顶部两条为只读统计：菜单展开时由 RefreshCacheMenu() 填充
            menuCache = new ToolStripMenuItem("24h 缓存率：—");
            menuCache.Enabled = false;
            strip.Items.Add(menuCache);
            menuCacheDetail = new ToolStripMenuItem(" ");
            menuCacheDetail.Enabled = false;
            strip.Items.Add(menuCacheDetail);
            strip.Items.Add(new ToolStripSeparator());
            menuStart = Add(strip, "启动代理", delegate { StartProxy(); });
            menuStop = Add(strip, "停止代理", delegate { StopProxy(); });
            strip.Items.Add(new ToolStripSeparator());
            menuLog = Add(strip, "打开日志", delegate { OpenLog(); });
            menuAutostart = Add(strip, "开机自启", delegate { ToggleAutostart(); });
            strip.Items.Add(new ToolStripSeparator());
            menuQuit = Add(strip, "退出（同时停止代理）", delegate { Quit(); });
            strip.Items.Add(new ToolStripSeparator());
            menuVersion = new ToolStripMenuItem("版本：" + Program.TrayVersion);
            menuVersion.Enabled = false;
            strip.Items.Add(menuVersion);
            // 菜单展开时刷新统计。这里再包一层 try/catch：任何异常从
            // ToolStripDropDown.Opening 逃逸都会让右键菜单直接弹崩溃对话框。
            strip.Opening += delegate
            {
                try { RefreshCacheMenu(); } catch { }
            };
            return strip;
        }

        ToolStripMenuItem Add(ContextMenuStrip strip, string text, EventHandler onClick)
        {
            var item = new ToolStripMenuItem(text);
            item.Click += onClick;
            strip.Items.Add(item);
            return item;
        }

        ToolStripMenuItem menuStart, menuStop, menuLog, menuAutostart, menuQuit;
        ToolStripMenuItem menuCache, menuCacheDetail;
        ToolStripMenuItem menuVersion;

        // 最近 24h 的缓存率：读 node 侧落盘的 logs\usage.jsonl 聚合。
        // 每次展开菜单时刷新。任何失败都必须降级为「无数据」——异常一旦
        // 从 ToolStripDropDown.Opening 逃逸，右键菜单就直接崩溃弹框。
        void RefreshCacheMenu()
        {
            if (menuCache == null || menuCacheDetail == null) return;
            try
            {
                var path = Path.Combine(LogDir(), "usage.jsonl");
                if (!File.Exists(path))
                {
                    ShowCache("24h 缓存率：无数据", " ");
                    return;
                }
                var cutoff = DateTime.UtcNow.AddHours(-24);
                double prompt = 0, cached = 0, comp = 0;
                int n = 0;
                var rx = new System.Text.RegularExpressions.Regex(
                    "\"ts\":\"([^\"]+)\".*\"promptTokens\":(\\d+),\"cachedTokens\":(\\d+),\"completionTokens\":(\\d+)");
                foreach (var line in File.ReadLines(path))
                {
                    var m = rx.Match(line);
                    if (!m.Success) continue;
                    DateTime ts;
                    if (!DateTime.TryParse(m.Groups[1].Value, null,
                        System.Globalization.DateTimeStyles.RoundtripKind, out ts)) continue;
                    if (ts.ToUniversalTime() < cutoff) continue;
                    prompt += double.Parse(m.Groups[2].Value);
                    cached += double.Parse(m.Groups[3].Value);
                    comp += double.Parse(m.Groups[4].Value);
                    n += 1;
                }
                if (n == 0)
                {
                    ShowCache("24h 缓存率：无数据", " ");
                    return;
                }
                var rate = prompt > 0 ? Math.Round(cached / prompt * 1000) / 10 : 0;
                ShowCache("24h 缓存率：" + rate + "%（" + n + " 次请求）",
                          "缓存 " + FmtTokens(cached) + " / " + FmtTokens(prompt)
                          + " tokens · 输出 " + FmtTokens(comp));
            }
            catch
            {
                // 兜底也必须自身安全：绝不在这里再次解引用可能为 null 的项
                try { ShowCache("24h 缓存率：读取失败", " "); } catch { }
            }
        }

        void ShowCache(string headline, string detail)
        {
            if (menuCache != null) menuCache.Text = headline;
            if (menuCacheDetail != null) menuCacheDetail.Text = detail;
        }

        static string FmtTokens(double v)
        {
            if (v >= 1000000) return Math.Round(v / 1000000, 1) + "M";
            if (v >= 1000) return Math.Round(v / 1000, 1) + "k";
            return Math.Round(v).ToString();
        }

        void RefreshMenu()
        {
            bool running = proxy != null && !proxy.HasExited;
            menuStart.Enabled = !running;
            menuStop.Enabled = running;
            menuAutostart.Checked = AutostartEnabled();
            if (menuVersion != null) menuVersion.Text = "版本：" + Program.TrayVersion;
            icon.Icon = MakeIcon(running ? Color.FromArgb(46, 204, 113) : Color.Gray);
            // 悬停提示带上版本号：热更新后瞄一眼即可确认换成了哪个版本
            icon.Text = (running ? "CC Proxy — running (:" + Port() + ")" : "CC Proxy — stopped")
                        + "  v" + Program.TrayVersion;
        }

        // ── 路径 ─────────────────────────────────────────────

        string ExeDir() { return AppDomain.CurrentDomain.BaseDirectory; }

        // exe 可能在子目录（tray\）；dist 在 <root>\dist。探测 exe 目录与其父目录。
        string ResolveProjectRoot()
        {
            var candidates = new[] { ExeDir(), Path.Combine(ExeDir(), "..") };
            foreach (var c in candidates)
            {
                try
                {
                    if (File.Exists(Path.Combine(c, "dist", "proxy.js")))
                        return Path.GetFullPath(c);
                }
                catch { }
            }
            return ExeDir();
        }

        string ProxyJs() { return Path.Combine(projectRoot, "dist", "proxy.js"); }
        // 测试实例（CC_TRAY_NS）用独立日志目录，避免污染生产日志
        string LogDir()
        {
            var baseDir = Path.Combine(projectRoot, "logs");
            return Program.Ns.Length == 0 ? baseDir : Path.Combine(baseDir, "test-" + Program.Ns);
        }
        string LogPath() { return Path.Combine(LogDir(), LogFileName); }
        int Port() { return Program.InstancePort(); }

        // ── node 定位：包内 node\ 优先，其次 PATH，再退到常见安装位 ──

        string FindNode()
        {
            var found = new List<string>();
            var roots = new[]
            {
                Path.Combine(ExeDir(), "node"),
                null,
                @"C:\Program Files\nodejs",
                @"C:\Program Files (x86)\nodejs",
            };
            foreach (var root in roots)
            {
                try
                {
                    var candidate = root == null ? "node.exe" : Path.Combine(root, "node.exe");
                    if (File.Exists(candidate)) found.Add(candidate);
                }
                catch { }
            }
            foreach (var dir in (Environment.GetEnvironmentVariable("PATH") ?? "").Split(';'))
            {
                if (dir.Trim().Length == 0) continue;
                try
                {
                    var p = Path.Combine(dir.Trim(), "node.exe");
                    if (File.Exists(p) && !found.Contains(p)) found.Add(p);
                }
                catch { }
            }
            return found.FirstOrDefault();
        }

        // ── 出站代理：默认跟随 Windows 系统代理 ─────────────

        string ResolveProxyUrl()
        {
            var ovr = Environment.GetEnvironmentVariable("CC_PROXY");
            if (ovr == "off") return null;
            if (!string.IsNullOrWhiteSpace(ovr)) return NormalizeProxy(ovr);
            try
            {
                using (var key = Registry.CurrentUser.OpenSubKey(
                    @"Software\Microsoft\Windows\CurrentVersion\Internet Settings"))
                {
                    if (key == null) return null;
                    var en = key.GetValue("ProxyEnable") as int?;
                    if (en == null || en.Value != 1) return null;
                    var server = key.GetValue("ProxyServer") as string;
                    if (string.IsNullOrWhiteSpace(server)) return null;
                    if (server.IndexOf(';') >= 0)
                    {
                        // 形如 "http=...;https=...;ftp=..." —— https 优先
                        string best = null;
                        foreach (var part in server.Split(';'))
                        {
                            var kv = part.Split('=');
                            if (kv.Length == 2 && (kv[0] == "https" || kv[0] == "http"))
                            {
                                best = kv[0] + "://" + kv[1];
                                if (kv[0] == "https") break;
                            }
                        }
                        return best;
                    }
                    if (server.IndexOf('=') < 0) return "http://" + server;
                    return null;
                }
            }
            catch { return null; }
        }

        string NormalizeProxy(string url)
        {
            url = url.Trim();
            if (url.IndexOf("://") < 0) return "http://" + url;
            return url;
        }

        string BuildNoProxy()
        {
            var list = new List<string> { "localhost", "127.0.0.1" };
            try
            {
                using (var key = Registry.CurrentUser.OpenSubKey(
                    @"Software\Microsoft\Windows\CurrentVersion\Internet Settings"))
                {
                    var ovr = key != null ? key.GetValue("ProxyOverride") as string : null;
                    if (!string.IsNullOrWhiteSpace(ovr))
                    {
                        foreach (var part in ovr.Split(';'))
                            if (part.Trim().Length > 0 && part.Trim() != "<local>")
                                list.Add(part.Trim());
                    }
                }
            }
            catch { }
            return string.Join(",", list.Distinct().ToArray());
        }

        void LogEgressDecision()
        {
            var url = ResolveProxyUrl();
            AppendLog("[tray] 出站代理: " + (url ?? "直连（系统代理未开或 CC_PROXY=off）")
                      + "  [NODE_USE_ENV_PROXY=1, NO_PROXY=" + BuildNoProxy() + "]");
        }

        void ApplyEgressEnv(ProcessStartInfo psi)
        {
            psi.EnvironmentVariables["NODE_USE_ENV_PROXY"] = "1";
            var url = ResolveProxyUrl();
            if (url != null)
            {
                psi.EnvironmentVariables["HTTPS_PROXY"] = url;
                psi.EnvironmentVariables["HTTP_PROXY"] = url;
            }
            psi.EnvironmentVariables["NO_PROXY"] = BuildNoProxy();
            // 测试实例用独立端口，避免与生产实例抢端口
            if (Program.Ns.Length > 0)
                psi.EnvironmentVariables["PORT"] = Port().ToString();
        }

        // ── 代理生命周期 ─────────────────────────────────────

        /** 找出占用指定端口的进程 PID（仅 IPv4/IPv6 的 LISTENING 行）。 */
        int? PortOwnerPid(int port)
        {
            try
            {
                var outText = "";
                var psi = new ProcessStartInfo("netstat", "-ano")
                {
                    UseShellExecute = false,
                    RedirectStandardOutput = true,
                    CreateNoWindow = true,
                };
                var np = Process.Start(psi);
                outText = np.StandardOutput.ReadToEnd();
                np.WaitForExit(5000);
                foreach (var raw in outText.Split('\n'))
                {
                    var line = raw.Trim();
                    if (!line.StartsWith("TCP")) continue;
                    if (line.IndexOf("LISTENING", StringComparison.OrdinalIgnoreCase) < 0) continue;
                    var parts = line.Split(new[] { ' ' }, StringSplitOptions.RemoveEmptyEntries);
                    if (parts.Length < 5) continue;
                    var local = parts[1];
                    var colon = local.LastIndexOf(':');
                    if (colon < 0) continue;
                    int p;
                    if (!int.TryParse(local.Substring(colon + 1), out p) || p != port) continue;
                    int pid;
                    if (int.TryParse(parts[parts.Length - 1], out pid)) return pid;
                }
            }
            catch { }
            return null;
        }

        /** 该 PID 是否是我们自己的进程（本托盘，或它启动的 node）。 */
        bool IsSelfOwned(int pid)
        {
            try
            {
                if (proxy != null && !proxy.HasExited && proxy.Id == pid) return true;
                if (pid == Process.GetCurrentProcess().Id) return true;
                var p = Process.GetProcessById(pid);
                if (string.Equals(p.ProcessName, "CCProxyTray", StringComparison.OrdinalIgnoreCase))
                    return true;
                if (string.Equals(p.ProcessName, "node", StringComparison.OrdinalIgnoreCase))
                {
                    // 只认「运行我们 dist\proxy.js」的 node，别误杀别人的 node
                    var cmd = GetCommandLineOf(pid);
                    return cmd != null && cmd.IndexOf("proxy.js", StringComparison.OrdinalIgnoreCase) >= 0;
                }
            }
            catch { }
            return false;
        }

        static string GetCommandLineOf(int pid)
        {
            try
            {
                var psi = new ProcessStartInfo("powershell",
                    "-NoProfile -Command \"(Get-CimInstance Win32_Process -Filter \\\"ProcessId=" + pid +
                    "\\\").CommandLine\"")
                {
                    UseShellExecute = false,
                    RedirectStandardOutput = true,
                    CreateNoWindow = true,
                };
                var p = Process.Start(psi);
                var s = p.StandardOutput.ReadToEnd();
                p.WaitForExit(5000);
                return s;
            }
            catch { return null; }
        }

        /** 纯等待：轮询直到端口无人监听（交接让位时用，不判断占用者身份）。 */
        void WaitForPortFree(int port, int timeoutMs)
        {
            var sw = Stopwatch.StartNew();
            while (sw.ElapsedMilliseconds < timeoutMs)
            {
                if (PortOwnerPid(port) == null) return;
                Thread.Sleep(250);
            }
            AppendLog("[tray] 等待端口 " + port + " 释放超时");
        }

        /**
         * 端口守卫。端口是生产契约，必须定死，因此：
         *   空闲            → 放行
         *   被我方进程占用   → 结束它（旧版本残留 / 交接未清的尾巴），放行
         *   被外部软件占用   → 拒绝启动并明确告知，绝不抢别人的端口
         */
        bool EnsurePortAvailable(int port)
        {
            var pid = PortOwnerPid(port);
            if (pid == null) return true;

            if (IsSelfOwned(pid.Value))
            {
                AppendLog("[tray] 端口 " + port + " 被我方进程(PID " + pid.Value + ")占用，结束它后启动");
                try { Process.GetProcessById(pid.Value).Kill(); } catch { }
                var sw = Stopwatch.StartNew();
                while (sw.ElapsedMilliseconds < 10000)
                {
                    if (PortOwnerPid(port) == null) return true;
                    Thread.Sleep(250);
                }
                AppendLog("[tray] 结束我方旧进程后端口仍未释放");
                return false;
            }

            AppendLog("[tray] 端口 " + port + " 已被其他程序占用(PID " + pid.Value + ")，拒绝启动");
            MessageBox.Show(
                "端口 " + port + " 已被其他程序占用（PID " + pid.Value + "）。\n\n" +
                "本代理需要该固定端口，不会抢占其他程序的端口。\n" +
                "请先释放该端口后重试。",
                "CC Proxy Tray", MessageBoxButtons.OK, MessageBoxIcon.Error);
            return false;
        }

        void StartProxy()
        {
            lock (gate)
            {
                if (proxy != null && !proxy.HasExited) return;
                var node = FindNode();
                if (node == null || !File.Exists(ProxyJs()))
                {
                    MessageBox.Show(
                        "找不到 node.exe 或 dist\\proxy.js。\n请确认托盘 exe 位于 CCProxy 包内且已构建（pnpm build）。",
                        "CC Proxy Tray", MessageBoxButtons.OK, MessageBoxIcon.Error);
                    return;
                }
                if (!EnsurePortAvailable(Port())) return;
                manualStop = false;

                var psi = new ProcessStartInfo
                {
                    FileName = node,
                    Arguments = "\"" + ProxyJs() + "\"",
                    WorkingDirectory = projectRoot,
                    UseShellExecute = false,
                    CreateNoWindow = true,
                    RedirectStandardOutput = true,
                    RedirectStandardError = true,
                };
                ApplyEgressEnv(psi);
                var p = new Process { StartInfo = psi, EnableRaisingEvents = true };
                // node 输出是 UTF-8；不显式指定会被按系统代码页（GBK）解码成乱码
                psi.StandardOutputEncoding = System.Text.Encoding.UTF8;
                psi.StandardErrorEncoding = System.Text.Encoding.UTF8;
                p.OutputDataReceived += delegate(object s, DataReceivedEventArgs e) { AppendLog(e.Data); };
                p.ErrorDataReceived += delegate(object s, DataReceivedEventArgs e) { AppendLog(e.Data); };
                p.Exited += delegate { OnProxyExited(); };
                p.Start();
                p.BeginOutputReadLine();
                p.BeginErrorReadLine();
                Program.AssignToJob(p); // 托盘死则子进程死，无孤儿占端口
                proxy = p;
            }
            RefreshMenu();
        }

        void StopProxy()
        {
            lock (gate)
            {
                manualStop = true;
                if (proxy != null && !proxy.HasExited)
                {
                    try { proxy.Kill(); } catch { }
                }
            }
            RefreshMenu();
        }

        void OnProxyExited()
        {
            if (quitting || manualStop) { RefreshMenu(); return; }
            AppendLog("[tray] 代理意外退出 —— 3 秒后自动重启");
            System.Threading.Timer restart = null;
            restart = new System.Threading.Timer(delegate
            {
                try { restart.Dispose(); } catch { }
                StartProxy();
            });
            restart.Change(3000, Timeout.Infinite);
        }

        void Quit()
        {
            quitting = true;
            StopProxy();
            icon.Visible = false;
            Application.Exit();
        }

        // ── 日志 / 自启 ─────────────────────────────────────

        void AppendLog(string line)
        {
            if (line == null) return;
            try
            {
                lock (gate)
                {
                    var path = LogPath();
                    if (File.Exists(path) && new FileInfo(path).Length > LogMaxBytes)
                        File.WriteAllText(path, ""); // 个人用：简单截断轮转
                    File.AppendAllText(path, DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss ") + line + Environment.NewLine);
                }
            }
            catch { }
        }

        void OpenLog()
        {
            var path = LogPath();
            if (!File.Exists(path)) File.WriteAllText(path, "");
            Process.Start(new ProcessStartInfo("notepad.exe", "\"" + path + "\"") { UseShellExecute = true });
        }

        const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
        const string RunValue = "CC Proxy Tray";

        bool AutostartEnabled()
        {
            using (var key = Registry.CurrentUser.OpenSubKey(RunKey))
            {
                if (key == null) return false;
                return key.GetValue(RunValue) != null;
            }
        }

        void ToggleAutostart()
        {
            using (var key = Registry.CurrentUser.CreateSubKey(RunKey))
            {
                if (AutostartEnabled()) key.DeleteValue(RunValue, false);
                else key.SetValue(RunValue, "\"" + Application.ExecutablePath + "\"");
            }
            RefreshMenu();
        }

        // ── 托盘图标：彩色圆点，无需 .ico 文件 ───────────────

        Icon MakeIcon(Color color)
        {
            using (var bmp = new Bitmap(16, 16))
            using (var g = Graphics.FromImage(bmp))
            {
                g.SmoothingMode = System.Drawing.Drawing2D.SmoothingMode.AntiAlias;
                using (var brush = new SolidBrush(color))
                    g.FillEllipse(brush, 2, 2, 12, 12);
                using (var pen = new Pen(Color.FromArgb(60, 60, 60)))
                    g.DrawEllipse(pen, 2, 2, 12, 12);
                return Icon.FromHandle(bmp.GetHicon());
            }
        }
    }
}
