# -*- coding: utf-8 -*-
"""矩阵测试：接口格式 × 思考强度 × DS三模型，验证思考与正文的格式分离。
另测 deepseek-v4.1-flash 最小图片识图（红/蓝对照）。"""
import base64
import json
import struct
import time
import urllib.error
import urllib.request
import zlib

BASE = "http://127.0.0.1:8787"
cfg = json.load(open(r"C:\Users\CelestNya\.zcode\v2\config.json", encoding="utf-8"))
KEY = cfg["provider"]["79db332f-fc32-40fb-8935-d6f77b320ca5"]["options"]["apiKey"]
HDRS = {"Content-Type": "application/json", "Authorization": "Bearer " + KEY}
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

MODELS = ["deepseek/deepseek-v4-pro", "deepseek/deepseek-v4-flash", "deepseek/deepseek-v4.1-flash"]
PROMPT = "1+1等于几？只回答数字。"


def png_b64(rgb, size=4):
    w = h = size
    raw = b"".join(b"\x00" + bytes(rgb) * w for _ in range(h))
    def chunk(typ, data):
        return (struct.pack(">I", len(data)) + typ + data
                + struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF))
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
           + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
    return base64.b64encode(png).decode()


def sse_parse(text):
    """解析 anthropic SSE：返回 (事件名列表, thinking文本, 正文文本, 顶层签名事件数, 签名是否嵌套)"""
    events, thinking, body, top_sig, nested_sig = [], [], 0, 0, False
    cur_event = None
    block_types = {}
    for block in text.split("\n\n"):
        lines = [l for l in block.split("\n") if l.strip()]
        if not lines:
            continue
        ev = None
        data = ""
        for l in lines:
            if l.startswith("event: "):
                ev = l[7:].strip()
                events.append(ev)
            elif l.startswith("data: "):
                data += l[6:]
        if ev and data:
            try:
                d = json.loads(data)
            except Exception:
                continue
            t = d.get("type")
            if t == "content_block_start":
                block_types[d.get("index")] = d.get("content_block", {}).get("type")
            elif t == "content_block_delta":
                delta = d.get("delta", {})
                if delta.get("type") == "thinking_delta":
                    thinking.append(delta.get("thinking") or "")
                elif delta.get("type") == "text_delta":
                    body += 1
                elif delta.get("type") == "signature_delta":
                    nested_sig = True
            elif t == "signature_delta":
                top_sig += 1
    return events, "".join(thinking), body, top_sig, nested_sig, block_types


def test_openai(mid, effort):
    body = {
        "model": mid, "messages": [{"role": "user", "content": PROMPT}],
        "max_tokens": 4000 if effort in ("max", None) else 2500, "stream": True,
    }
    if effort:
        body["reasoning_effort"] = effort
    label = f"openai:{effort or '省略'}"
    try:
        resp = opener.open(urllib.request.Request(BASE + "/v1/chat/completions",
                           data=json.dumps(body).encode(), headers=HDRS), timeout=180)
        text = resp.read().decode()
        reasoning, content, finish = "", "", None
        in_reason = in_content = False
        for line in text.split("\n"):
            if not line.startswith("data: ") or "[DONE]" in line:
                continue
            try:
                d = json.loads(line[6:])
            except Exception:
                continue
            ch = (d.get("choices") or [{}])[0]
            delta = ch.get("delta", {})
            if delta.get("reasoning_content"):
                reasoning += delta["reasoning_content"]
            if delta.get("content"):
                content += delta["content"]
            if ch.get("finish_reason"):
                finish = ch["finish_reason"]
        # 思考不得混入正文
        mixed = reasoning and reasoning[:20] in content
        ok = finish == "stop" and content.strip() != "" and not mixed
        note = f"reasoning={len(reasoning)}字 content={content.strip()[:20]!r}"
        if mixed:
            ok, note = False, "思考混入正文! " + note
        print(f"{'PASS' if ok else 'DIFF'} | {mid} | {label} | finish={finish} | {note}", flush=True)
    except urllib.error.HTTPError as e:
        print(f"FAIL | {mid} | {label} | HTTP {e.code}", flush=True)
    except Exception as e:
        print(f"FAIL | {mid} | {label} | {type(e).__name__}: {str(e)[:80]}", flush=True)


def test_anthropic(mid, budget):
    body = {"model": mid, "max_tokens": (budget + 2000) if budget else 2500,
            "stream": True, "messages": [{"role": "user", "content": PROMPT}]}
    label = "anthropic:省略" if budget is None else f"anthropic:{budget}({ ['low','medium','high','xhigh','max'][(budget>2000)+(budget>8000)+(budget>16000)+(budget>32000)] })"
    if budget:
        body["thinking"] = {"type": "enabled", "budget_tokens": budget}
    try:
        resp = opener.open(urllib.request.Request(BASE + "/v1/messages",
                           data=json.dumps(body).encode(), headers=HDRS), timeout=180)
        events, thinking, _, top_sig, nested_sig, blocks = sse_parse(resp.read().decode())
        has_think_block = "thinking" in blocks.values()
        has_text_block = "text" in blocks.values()
        # 期望：思考在 thinking 块、正文在 text 块、无顶层 signature_delta、签名嵌套
        ok = (top_sig == 0 and (not thinking or has_think_block) and has_text_block)
        note = f"think块={has_think_block} text块={has_text_block} 思考{len(thinking)}字 顶层签名={top_sig} 签名嵌套={nested_sig}"
        if top_sig > 0:
            note = "存在顶层signature_delta! " + note
        print(f"{'PASS' if ok else 'DIFF'} | {mid} | {label} | {note}", flush=True)
    except urllib.error.HTTPError as e:
        print(f"FAIL | {mid} | {label} | HTTP {e.code}", flush=True)
    except Exception as e:
        print(f"FAIL | {mid} | {label} | {type(e).__name__}: {str(e)[:80]}", flush=True)


def test_vision(path, rgb, color_expect):
    b64 = png_b64(rgb)
    size = len(b64) * 3 // 4
    if path == "openai":
        body = {"model": "deepseek/deepseek-v4.1-flash", "max_tokens": 1500, "stream": False,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "图里主要是什么颜色？只答颜色名。"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64," + b64}}]}]}
        url = "/v1/chat/completions"
    else:
        body = {"model": "deepseek/deepseek-v4.1-flash", "max_tokens": 1500, "stream": False,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "图里主要是什么颜色？只答颜色名。"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": b64}}]}]}
        url = "/v1/messages"
    name = {("openai", (220, 30, 30)): "红", ("openai", (30, 60, 220)): "蓝",
            ("anthropic", (220, 30, 30)): "红", ("anthropic", (30, 60, 220)): "蓝"}[(path, rgb)]
    try:
        t0 = time.time()
        resp = json.load(opener.open(urllib.request.Request(BASE + url,
                       data=json.dumps(body).encode(), headers=HDRS), timeout=180))
        if path == "anthropic":
            content = "".join((b.get("text") or "") for b in resp.get("content", [])
                              if b.get("type") == "text").strip()
            reason = ""
        else:
            ch = resp["choices"][0]
            content = (ch["message"].get("content") or "").strip()
            reason = ch["message"].get("reasoning_content") or ""
        hit = color_expect in content or color_expect.lower() in content.lower()
        print(f"{'PASS' if hit else 'DIFF'} | vision {path} | {size}字节纯色{name}图 | "
              f"答={content[:20]!r} | {round(time.time()-t0,1)}s | reason={len(reason)}字", flush=True)
    except urllib.error.HTTPError as e:
        print(f"FAIL | vision {path} | {name}图 {size}字节 | HTTP {e.code}", flush=True)
    except Exception as e:
        print(f"FAIL | vision {path} | {type(e).__name__}: {str(e)[:100]}", flush=True)


if __name__ == "__main__":
    print("== 矩阵：接口格式 × 思考强度 × DS三模型 ==", flush=True)
    for mid in MODELS:
        for eff in [None, "off", "high", "max"]:
            test_openai(mid, eff)
        for bud in [None, 1500, 8000, 16000, 40000]:
            test_anthropic(mid, bud)
    print("\n== DS4.1 识图（最小纯色PNG，红蓝对照，双格式）==", flush=True)
    test_vision("openai", (220, 30, 30), "红")
    test_vision("anthropic", (30, 60, 220), "蓝")
    print("\nDONE", flush=True)
