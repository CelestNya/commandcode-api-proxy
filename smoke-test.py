# -*- coding: utf-8 -*-
"""个人版冒烟测试：逐个模型请求固定两字输出，验证代理全链路。
用法：python smoke-test.py [并发数]   结果实时打印，末尾汇总。"""
import json
import sys
import time
import urllib.error
import urllib.request
import concurrent.futures

BASE = "http://127.0.0.1:8787"
# key 从 ZCode 配置读取（个人版代理本身不存 key）
cfg = json.load(open(r"C:\Users\CelestNya\.zcode\v2\config.json", encoding="utf-8"))
KEY = cfg["provider"]["79db332f-fc32-40fb-8935-d6f77b320ca5"]["options"]["apiKey"]

# 不走系统代理（本机回环）
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def get_models():
    r = urllib.request.Request(BASE + "/v1/models")
    return [m["id"] for m in json.load(opener.open(r, timeout=20))["data"]]


def chat(mid, timeout=100):
    body = json.dumps({
        "model": mid,
        "messages": [{"role": "user", "content": "只输出两个字：好的。不要输出任何其他内容。"}],
        "max_tokens": 800,
        "stream": False,
    }).encode("utf-8")
    r = urllib.request.Request(
        BASE + "/v1/chat/completions", data=body,
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + KEY},
    )
    t0 = time.time()
    try:
        resp = json.load(opener.open(r, timeout=timeout))
        choice = resp["choices"][0]
        content = (choice["message"].get("content") or "").strip()
        finish = choice.get("finish_reason")
        usage = resp.get("usage", {})
        secs = round(time.time() - t0, 1)
        ok = content == "好的"
        tag = "PASS" if ok and finish == "stop" else "DIFF"
        return f"{tag} | {mid} | finish={finish} | {secs}s | out={content!r} | comp_tokens={usage.get('completion_tokens')}"
    except urllib.error.HTTPError as e:
        try:
            detail = json.loads(e.read().decode())["error"]["message"]
            detail = detail[:150].replace("\n", " ")
        except Exception:
            detail = str(e)[:150]
        return f"FAIL | {mid} | HTTP {e.code} | {detail}"
    except Exception as e:
        return f"FAIL | {mid} | {type(e).__name__}: {str(e)[:120]}"


def main():
    workers = int(sys.argv[1]) if len(sys.argv) > 1 else 4

    # 先用 v4.1 裸名触发一次目录发现，让 /v1/models 拿到完整在线列表
    chat("deepseek-v4.1-flash", timeout=120)
    ids = get_models()
    print(f"MODEL_LIST ({len(ids)}):", flush=True)
    for i in ids:
        print("  " + i, flush=True)

    print(f"\nSMOKE START workers={workers}", flush=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as ex:
        futures = {ex.submit(chat, mid): mid for mid in ids}
        done = 0
        for fut in concurrent.futures.as_completed(futures):
            done += 1
            print(f"[{done}/{len(ids)}] {fut.result()}", flush=True)

    # 汇总（按原始列表顺序重打一遍，便于阅读）
    print("\n===== RESULTS =====", flush=True)


if __name__ == "__main__":
    main()
