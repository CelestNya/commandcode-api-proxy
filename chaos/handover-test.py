# -*- coding: utf-8 -*-
"""交接协议验证：用隔离命名空间(CC_TRAY_NS) + 独立端口(CC_TRAY_PORT)真实启动
两个托盘实例，验证「新实例接管、旧实例让位」的两阶段提交与回滚。

绝不触碰生产实例（默认命名空间 / 8787）。

用法：python chaos/handover-test.py
"""
import os
import subprocess
import sys
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.environ.get("CC_TEST_EXE") or os.path.join(ROOT, "release", "CCProxy", "CCProxyTray.exe")
NS = "handovertest"
PORT = 8899
LOG = os.path.join(os.path.dirname(EXE), "logs", "test-" + NS, "proxy.log")

opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def env():
    e = dict(os.environ)
    e["CC_TRAY_NS"] = NS
    e["CC_TRAY_PORT"] = str(PORT)
    e["CC_API_BASE"] = "http://127.0.0.1:1"   # 上游不可达，无所谓：只验证进程/端口交接
    return e


def start():
    return subprocess.Popen([EXE], cwd=os.path.dirname(EXE), env=env(),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def serving(timeout=25.0):
    """端口是否在服务（TCP 可连接）。"""
    end = time.time() + timeout
    while time.time() < end:
        try:
            import socket
            s = socket.create_connection(("127.0.0.1", PORT), timeout=1)
            s.close()
            return True
        except Exception:
            time.sleep(0.4)
    return False


def count(proc_name="CCProxyTray"):
    out = subprocess.run(
        ["powershell", "-NoProfile", "-Command",
         f"@(Get-Process {proc_name} -ErrorAction SilentlyContinue).Count"],
        capture_output=True).stdout.decode("utf-8", "replace").strip()
    try:
        return int(out)
    except Exception:
        return -1


def tray_procs():
    out = subprocess.run(
        ["powershell", "-NoProfile", "-Command",
         "Get-CimInstance Win32_Process -Filter \"Name='CCProxyTray.exe'\" | "
         "Select-Object -ExpandProperty ProcessId"],
        capture_output=True).stdout.decode("utf-8", "replace")
    return [int(x) for x in out.split() if x.strip().isdigit()]


def log_tail(n=12):
    try:
        with open(LOG, encoding="utf-8", errors="replace") as f:
            return [l.rstrip() for l in f.readlines()[-n:]]
    except Exception:
        return ["(无日志)"]


def cleanup(procs):
    for pid in procs:
        subprocess.run(["taskkill", "/F", "/PID", str(pid)], capture_output=True)
    time.sleep(1)


def main():
    try:
        os.remove(LOG)
    except Exception:
        pass
    baseline = set(tray_procs())
    print(f"生产托盘 PID（本次绝不触碰）: {sorted(baseline)}", flush=True)
    if not os.path.exists(EXE):
        print("找不到 exe:", EXE)
        return 1

    print(f"\n=== 场景1：首任实例启动并服务 :{PORT} ===", flush=True)
    p1 = start()
    ok1 = serving()
    print(f"A 启动 pid={p1.pid} | 端口服务={'✓' if ok1 else '✗'}", flush=True)

    print("\n=== 场景2：第二个实例接管（旧实例应让位且不出现服务真空）===", flush=True)
    p2 = start()
    ok2 = serving(30)
    # 交接是异步的：旧实例暂停服务→释放锁→新实例取锁→启动 node→验证服务。
    # 轮询等待旧实例真正退出，而不是固定 sleep（否则必然误判）。
    deadline = time.time() + 60
    vacuum = False
    while time.time() < deadline:
        if p1.poll() is not None:
            break
        if not serving(timeout=0.5):
            vacuum = True
        time.sleep(0.5)
    p1_exited = p1.poll() is not None
    ok2 = serving(30)
    time.sleep(2)
    procs = set(tray_procs()) - baseline
    alive2 = p2.poll() is None
    print(f"B 启动 pid={p2.pid} | 端口服务={'✓' if ok2 else '✗'}", flush=True)
    print(f"接管后：A存活={not p1_exited} B存活={alive2} 测试实例数={len(procs)}（期望 1）", flush=True)
    print(f"交接窗口内是否出现过服务真空={vacuum}", flush=True)
    verdict2 = ok2 and alive2 and p1_exited and len(procs) == 1
    print(f"场景2 判定: {'✓ 接管正确（新实例服务、旧实例退出）' if verdict2 else '✗ 交接异常'}", flush=True)

    # ── 场景 2b：再接一次班 ───────────────────────────────────────────────
    # 关键回归：B 是以「继任者」身份启动的。接班成功后它必须转成在职实例继续
    # 监听让位事件，否则下一次热更新无人应答。该缺陷只在**连续两次交接**时
    # 才暴露（2026-09-15 生产实测：接班的实例从此不再响应让位请求，导致热更新
    # 只成功得了一次）。
    print("\n=== 场景2b：第三次启动（B 已是现任，必须能再次让位）===", flush=True)
    p3 = start()
    ok3 = serving(30)
    deadline = time.time() + 60
    while time.time() < deadline:
        if p2.poll() is not None:
            break
        time.sleep(0.5)
    p2_exited = p2.poll() is not None
    ok3 = serving(30)
    time.sleep(2)
    procs3 = set(tray_procs()) - baseline
    alive3 = p3.poll() is None
    print(f"C 启动 pid={p3.pid} | 端口服务={'✓' if ok3 else '✗'}", flush=True)
    print(f"接管后：B存活={not p2_exited} C存活={alive3} 测试实例数={len(procs3)}（期望 1）", flush=True)
    verdict2b = ok3 and alive3 and p2_exited and len(procs3) == 1
    print(
        f"场景2b 判定: {'✓ 第二次交接成功（接班的实例仍能再让位）' if verdict2b else '✗ 接班的实例无法再让位'}",
        flush=True,
    )

    print("\n=== 场景3：生产实例未受影响 ===", flush=True)
    after = set(tray_procs())
    untouched = baseline.issubset(after)
    print(f"生产托盘仍在={untouched}（原 PID {sorted(baseline)} 现存 {sorted(after & baseline)}）", flush=True)

    print("\n=== 交接日志 ===", flush=True)
    for line in log_tail(20):
        print("  " + line, flush=True)

    print(f"\n=== 清理测试实例 ===", flush=True)
    cleanup(list(procs3) + [p3.pid])
    print(f"剩余测试实例: {sorted(set(tray_procs()) - baseline)}", flush=True)

    print(
        "\n结果: " + ("PASS" if (ok1 and verdict2 and verdict2b and untouched) else "FAIL"),
        flush=True,
    )
    return 0 if (ok1 and verdict2 and verdict2b and untouched) else 1


if __name__ == "__main__":
    sys.exit(main())
