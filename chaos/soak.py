# -*- coding: utf-8 -*-
"""混沌测试执行器：确定性故障场景 + 崩溃注入 + 随机浸泡。
用法：
  python chaos/soak.py                # 全流程（阶段0-3）
  python chaos/soak.py --resume       # 沿用已运行的混沌代理，跳过阶段0/1
  python chaos/soak.py --minutes 30   # 自定义浸泡时长"""
import json
import os
import random
import subprocess
import sys
import time
import urllib.error
import urllib.request

BASE = "http://127.0.0.1:8899"       # 混沌代理实例
MOCK = "http://127.0.0.1:9888"       # 故障注入上游
CC_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

ENV = dict(os.environ,
           PORT="8899", HOST="127.0.0.1",
           CC_API_BASE=MOCK,
           CC_IDLE_TIMEOUT_MS="5000",       # 压缩等待，快速验证 idle 行为
           CC_UPSTREAM_TIMEOUT_MS="4000",   # 建连超时 4s（配合 slow_headers 6s）
           LOG_LEVEL="warn")

proxy_proc = None


def start_proxy():
    global proxy_proc
    proxy_proc = subprocess.Popen(
        ["node", os.path.join(CC_DIR, "dist", "proxy.js")],
        cwd=CC_DIR, env=ENV,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def health(timeout=3):
    try:
        return opener.open(urllib.request.Request(BASE + "/health"), timeout=timeout).status
    except Exception:
        return None


def stats():
    return json.load(opener.open(urllib.request.Request(MOCK + "/__stats"), timeout=5))


def set_mode(mode):
    req = urllib.request.Request(MOCK + "/__mode",
                                 data=json.dumps({"mode": mode}).encode())
    opener.open(req, timeout=5).read()


def fire_nonstream(model="deepseek/deepseek-v4-flash", timeout=30):
    body = json.dumps({"model": model, "max_tokens": 500, "stream": False,
                       "messages": [{"role": "user", "content": "hi"}]}).encode()
    req = urllib.request.Request(BASE + "/v1/chat/completions", data=body,
                                 headers={"Content-Type": "application/json",
                                          "Authorization": "Bearer chaos-key"})
    t0 = time.time()
    try:
        resp = opener.open(req, timeout=timeout)
        d = json.loads(resp.read().decode())
        ch = d["choices"][0]
        return {"http": resp.status, "kind": "ok", "content": ch["message"].get("content", ""),
                "finish": ch.get("finish_reason"), "secs": round(time.time() - t0, 1)}
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        return {"http": e.code, "kind": "http_error", "msg": raw[:110],
                "secs": round(time.time() - t0, 1)}
    except Exception as e:
        return {"http": None, "kind": "exc", "msg": f"{type(e).__name__}: {str(e)[:80]}",
                "secs": round(time.time() - t0, 1)}


def fire_stream(timeout=25):
    body = json.dumps({"model": "deepseek/deepseek-v4-flash", "max_tokens": 500, "stream": True,
                       "messages": [{"role": "user", "content": "hi"}]}).encode()
    req = urllib.request.Request(BASE + "/v1/chat/completions", data=body,
                                 headers={"Content-Type": "application/json",
                                          "Authorization": "Bearer chaos-key"})
    t0 = time.time()
    try:
        resp = opener.open(req, timeout=timeout)
        text = resp.read().decode()
        err = '"error":{' in text
        done = "[DONE]" in text
        content = ""
        for l in text.split("\n"):
            if not l.startswith("data: ") or "[DONE]" in l or '"error"' in l or '"usage"' in l:
                continue
            try:
                content += json.loads(l[6:]).get("choices", [{}])[0].get("delta", {}).get("content", "") or ""
            except Exception:
                pass
        return {"http": resp.status, "kind": "stream", "envelope": err, "done": done,
                "content": content, "secs": round(time.time() - t0, 1)}
    except urllib.error.HTTPError as e:
        return {"http": e.code, "kind": "http_error", "msg": e.read().decode()[:110],
                "secs": round(time.time() - t0, 1)}
    except Exception as e:
        return {"http": None, "kind": "exc", "msg": f"{type(e).__name__}: {str(e)[:80]}",
                "secs": round(time.time() - t0, 1)}


def hits_delta(before, mode):
    return stats()["byMode"].get(mode, 0) - before.get("byMode", {}).get(mode, 0)


def scenario(name, fire, checks):
    set_mode(name)
    before = stats()
    r = fire()
    hits = hits_delta(before, name)
    verdicts = []
    for desc, fn in checks:
        try:
            verdicts.append(f"{desc}:{'✓' if fn(r, hits) else '✗'}")
        except Exception as ex:
            verdicts.append(f"{desc}:ERR({ex})")
    alive = health() == 200
    print(f"[{name}] http={r.get('http')} {round(r.get('secs', 0), 1)}s 上游命中={hits} "
          f"| {' '.join(verdicts)} | 代理存活={alive}", flush=True)
    return {"mode": name, "result": r, "hits": hits, "alive": alive}


def run_scenarios():
    print("== 阶段1：确定性故障场景（断言代理行为符合设计）==", flush=True)
    S = []
    S.append(scenario("ok", lambda: fire_nonstream(), [
        ("成功", lambda r, h: r["kind"] == "ok" and r["content"] == "好的"),
        ("单次命中", lambda r, h: h == 1)]))
    S.append(scenario("status_403", lambda: fire_nonstream(), [
        ("403透传", lambda r, h: r["kind"] == "http_error" and r["http"] == 403),
        ("不重试", lambda r, h: h == 1)]))
    S.append(scenario("status_429", lambda: fire_nonstream(), [
        ("429透传", lambda r, h: r["kind"] == "http_error" and r["http"] == 429),
        ("重试3次", lambda r, h: h == 3)]))
    S.append(scenario("status_500", lambda: fire_nonstream(), [
        ("错误返回", lambda r, h: r["kind"] == "http_error" and r["http"] in (500, 502)),
        ("重试3次", lambda r, h: h == 3)]))
    S.append(scenario("refuse", lambda: fire_nonstream(), [
        ("错误返回", lambda r, h: r["kind"] == "http_error" and r["http"] in (500, 502)),
        ("重试3次", lambda r, h: h == 3)]))
    S.append(scenario("slow_headers", lambda: fire_nonstream(timeout=40), [
        ("超时错误", lambda r, h: r["kind"] == "http_error"),
        ("重试3次", lambda r, h: h == 3),
        ("耗时≥12s", lambda r, h: r["secs"] >= 12)]))
    S.append(scenario("hang", lambda: fire_stream(timeout=20), [
        ("error信封", lambda r, h: r["envelope"]),
        ("流式收尾", lambda r, h: r["done"])]))
    S.append(scenario("reset_mid", lambda: fire_stream(timeout=20), [
        ("error信封", lambda r, h: r["envelope"]),
        ("部分内容先到", lambda r, h: "Hello" in r["content"])]))
    S.append(scenario("error_event", lambda: fire_nonstream(), [
        ("502不伪装", lambda r, h: r["kind"] == "http_error" and r["http"] == 502)]))
    S.append(scenario("garbage", lambda: fire_stream(timeout=15), [
        ("不崩溃", lambda r, h: r["kind"] in ("stream", "http_error"))]))
    S.append(scenario("trickle", lambda: fire_stream(timeout=25), [
        ("慢流完整", lambda r, h: r["kind"] == "stream" and r["content"] == "OK" * 15)]))
    n_ok = sum(1 for s in S if s["alive"])
    print(f"场景完成：{len(S)} 个，全程代理存活={n_ok}/{len(S)}", flush=True)


def port_owner_pid(port):
    out = subprocess.run(["netstat", "-ano"], capture_output=True).stdout.decode("gbk", "replace")
    for line in out.split("\n"):
        if f":{port}" in line and "LISTENING" in line:
            return line.split()[-1]
    return None


def crash_injection():
    print("\n== 阶段2：崩溃注入（强杀代理进程）==", flush=True)
    pid = port_owner_pid(8899)
    if not pid:
        print("未找到 8899 监听进程", flush=True)
        return
    subprocess.run(["taskkill", "/F", "/PID", pid], capture_output=True)
    time.sleep(1)
    print(f"强杀代理 PID={pid}，health={health()}", flush=True)
    start_proxy()
    for _ in range(20):
        if health() == 200:
            break
        time.sleep(0.5)
    print(f"重启后 health={health()}", flush=True)


def soak(minutes):
    print(f"\n== 阶段3：随机故障浸泡 {minutes} 分钟 ==", flush=True)
    modes = (["ok"] * 6 + ["refuse", "status_429", "status_500", "status_403",
                           "reset_mid", "error_event", "hang", "garbage", "trickle"])
    random.seed(42)
    t_end = time.time() + minutes * 60
    n = 0
    outcome = {"ok": 0, "http_error": 0, "exc": 0}
    max_sec = 0.0
    while time.time() < t_end:
        m = random.choice(modes)
        set_mode(m)
        r = fire_nonstream(timeout=35)
        n += 1
        if r["kind"] == "ok":
            outcome["ok"] += 1
        elif r["kind"] == "http_error":
            outcome["http_error"] += 1
        else:
            outcome["exc"] += 1
            print(f"  异常! mode={m} {r}", flush=True)
        max_sec = max(max_sec, r["secs"])
        if n % 25 == 0:
            print(f"  已请求 {n}，health={health()}", flush=True)
        if health() != 200:
            print(f"  代理失联! 最后结果={r}", flush=True)
            start_proxy()
            for _ in range(20):
                if health() == 200:
                    break
                time.sleep(0.5)
    print(f"浸泡完成：{n} 次请求 {outcome}，单次最大耗时 {max_sec}s，最终 health={health()}", flush=True)

    pid = port_owner_pid(8899)
    if pid:
        try:
            rss = subprocess.check_output(
                ["powershell", "-NoProfile", "-Command",
                 f"(Get-Process -Id {pid}).WorkingSet64"], text=True).strip()
            print(f"代理进程 RSS: {int(rss) / 1024 / 1024:.1f} MB", flush=True)
        except Exception as ex:
            print("RSS 采样失败", ex, flush=True)


def main():
    minutes = 6
    if "--minutes" in sys.argv:
        minutes = int(sys.argv[sys.argv.index("--minutes") + 1])
    resume = "--resume" in sys.argv

    if not resume:
        print("== 阶段0：启动混沌代理（独立端口 8899，指向故障注入上游）==", flush=True)
        start_proxy()
        for _ in range(20):
            if health() == 200:
                break
            time.sleep(0.5)
        print(f"health={health()}", flush=True)
        run_scenarios()
    else:
        print("== --resume：沿用已运行的混沌代理，跳过阶段0/1 ==", flush=True)

    crash_injection()
    soak(minutes)
    print("\nCHAOS DONE", flush=True)


if __name__ == "__main__":
    main()
