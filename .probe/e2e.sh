#!/usr/bin/env bash
# 代理实例端到端功能测试。只读/只写临时数据目录，不碰 8787。
# 用法: bash .probe/e2e.sh [代理端口] [上游端口]
set -u
P="${1:-8798}"
U="${2:-9888}"
B="http://127.0.0.1:$P"
M="http://127.0.0.1:$U"
CURL="curl -s --noproxy * --max-time 20"
pass=0; fail=0
ok()   { printf '  \033[32mPASS\033[0m %-46s %s\n' "$1" "$2"; pass=$((pass+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %-46s %s\n' "$1" "$2"; fail=$((fail+1)); }
note() { printf '       %s\n' "$1"; }

code() { curl -s --noproxy '*' --max-time 20 -o /tmp/e2e.out -w '%{http_code}' "$@"; }
body() { cat /tmp/e2e.out; }

setmode() { curl -s --noproxy '*' -X POST "$M/__mode" -d "{\"mode\":\"$1\"}" >/dev/null 2>&1; }
resetmode() { setmode ok; }

echo "=== 1. 基础接口 ==="
c=$(code "$B/health")
[ "$c" = 200 ] && ok "GET /health" "$c" || bad "GET /health" "$c"

c=$(code "$B/v1/models")
if [ "$c" = 200 ] && grep -q '"data"' /tmp/e2e.out; then ok "GET /v1/models" "$c"; else bad "GET /v1/models" "$c $(head -c 80 /tmp/e2e.out)"; fi

echo "=== 2. 非流式转发 ==="
CHAT='{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hello"}],"max_tokens":32}'
c=$(code -X POST "$B/v1/chat/completions" -H 'Content-Type: application/json' -H 'Authorization: Bearer test-key' -d "$CHAT")
if [ "$c" = 200 ] && grep -q 'choices' /tmp/e2e.out; then ok "POST /v1/chat/completions" "$c"; else bad "POST /v1/chat/completions" "$c $(head -c 120 /tmp/e2e.out)"; fi

MSG='{"model":"claude-sonnet-4-5","max_tokens":32,"messages":[{"role":"user","content":"hello"}]}'
c=$(code -X POST "$B/v1/messages" -H 'Content-Type: application/json' -H 'x-api-key: test-key' -d "$MSG")
if [ "$c" = 200 ] && grep -q '"content"' /tmp/e2e.out; then ok "POST /v1/messages" "$c"; else bad "POST /v1/messages" "$c $(head -c 120 /tmp/e2e.out)"; fi

c=$(code -X POST "$B/v1/messages/count_tokens" -H 'Content-Type: application/json' -d "$MSG")
if [ "$c" = 200 ] && grep -q 'input_tokens' /tmp/e2e.out; then ok "POST /v1/messages/count_tokens" "$c"; else bad "POST /v1/messages/count_tokens" "$c $(head -c 120 /tmp/e2e.out)"; fi

echo "=== 3. 流式（SSE） ==="
curl -s --noproxy '*' --max-time 20 -D /tmp/e2e.hdr -o /tmp/e2e.sse \
  -X POST "$B/v1/messages" -H 'Content-Type: application/json' -H 'x-api-key: test-key' \
  -d '{"model":"claude-sonnet-4-5","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"hello"}]}'
c=$(awk 'NR==1{print $2}' /tmp/e2e.hdr)
if grep -qi 'text/event-stream' /tmp/e2e.hdr && grep -q '^event:' /tmp/e2e.sse; then
  ok "POST /v1/messages stream=true" "$c 事件数=$(grep -c '^event:' /tmp/e2e.sse)"
else
  bad "POST /v1/messages stream=true" "$c $(head -c 120 /tmp/e2e.sse)"
fi

curl -s --noproxy '*' --max-time 20 -D /tmp/e2e.hdr2 -o /tmp/e2e.oai \
  -X POST "$B/v1/chat/completions" -H 'Content-Type: application/json' -H 'Authorization: Bearer test-key' \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":16,"stream":true}'
if grep -qi 'text/event-stream' /tmp/e2e.hdr2 && grep -q 'data:' /tmp/e2e.oai; then
  ok "POST /v1/chat/completions stream=true" "$(awk 'NR==1{print $2}' /tmp/e2e.hdr2) 块数=$(grep -c '^data:' /tmp/e2e.oai)"
else
  bad "POST /v1/chat/completions stream=true" "$(head -c 120 /tmp/e2e.oai)"
fi

c=$(code -X POST "$B/v1/messages" -H 'Content-Type: application/json' -d "$MSG")
[ "$c" = 401 ] && ok "无鉴权 -> 401" "$c" || bad "无鉴权 -> 401" "$c"

echo "=== 4. 输入校验与边界 ==="
c=$(code -X POST "$B/v1/messages" -H 'Content-Type: application/json' -d '{not json')
[ "$c" = 400 ] && ok "非法 JSON -> 400" "$c" || bad "非法 JSON -> 400" "$c"

c=$(code -X POST "$B/v1/messages" -H 'Content-Type: application/json' -d '{"model":"x"}')
[ "$c" = 400 ] && ok "缺 messages -> 400" "$c" || bad "缺 messages -> 400" "$c"

c=$(code "$B/no-such-path")
[ "$c" = 404 ] && ok "未知路径 -> 404" "$c" || bad "未知路径 -> 404" "$c"

c=$(code -X OPTIONS "$B/v1/messages" -H 'Origin: http://x' -H 'Access-Control-Request-Method: POST')
if [ "$c" = 200 ] || [ "$c" = 204 ]; then ok "CORS 预检 OPTIONS" "$c"; else bad "CORS 预检 OPTIONS" "$c"; fi

c=$(code "$B/webui/../ccproxy.json")
if [ "$c" = 403 ] || [ "$c" = 404 ]; then ok "路径穿越被拒" "$c"; else bad "路径穿越被拒" "$c"; fi

python -c "print('{\"model\":\"x\",\"messages\":[{\"role\":\"user\",\"content\":\"' + 'A'*20000000 + '\"}]}')" > /tmp/e2e.big
c=$(code -X POST "$B/v1/messages" -H 'Content-Type: application/json' --data-binary @/tmp/e2e.big)
[ "$c" = 413 ] && ok "超大请求体 -> 413" "$c" || bad "超大请求体 -> 413" "$c"

echo "=== 5. 上游故障处理 ==="
setmode status_500
c=$(code -X POST "$B/v1/chat/completions" -H 'Content-Type: application/json' -H 'Authorization: Bearer test-key' -d "$CHAT")
if [ "${c:0:1}" = "5" ]; then ok "上游 500 -> 客户端 5xx" "$c"; else bad "上游 500 -> 客户端 5xx" "$c"; fi

setmode status_429
c=$(code -X POST "$B/v1/chat/completions" -H 'Content-Type: application/json' -H 'Authorization: Bearer test-key' -d "$CHAT")
note "上游 429 -> 客户端 $c"

setmode status_403
c=$(code -X POST "$B/v1/chat/completions" -H 'Content-Type: application/json' -H 'Authorization: Bearer test-key' -d "$CHAT")
note "上游 403 -> 客户端 $c"

setmode refuse
c=$(code -X POST "$B/v1/chat/completions" -H 'Content-Type: application/json' -H 'Authorization: Bearer test-key' -d "$CHAT")
if [ "${c:0:1}" = "5" ]; then ok "上游拒连 -> 客户端 5xx" "$c"; else bad "上游拒连 -> 客户端 5xx" "$c"; fi

resetmode
c=$(code -X POST "$B/v1/chat/completions" -H 'Content-Type: application/json' -H 'Authorization: Bearer test-key' -d "$CHAT")
[ "$c" = 200 ] && ok "故障恢复后正常" "$c" || bad "故障恢复后正常" "$c"

echo "=== 6. WebUI 接口 ==="
for p in /webui/ /webui/index.html /webui/css/theme-jelly-dark.css /webui/js/stats.js /webui/api/stats /webui/api/attempts /webui/api/logs /webui/api/settings; do
  c=$(code "$B$p")
  case "$p" in
    */api/*) [ "$c" = 200 ] && grep -q '[{\[]' /tmp/e2e.out && ok "GET $p" "$c" || bad "GET $p" "$c" ;;
    *)       [ "$c" = 200 ] && ok "GET $p" "$c" || bad "GET $p" "$c" ;;
  esac
done

echo "=== 7. 统计落库 ==="
n=$(curl -s --noproxy '*' "$B/webui/api/attempts?limit=5" | grep -o '"reqId"' | wc -l)
[ "$n" -gt 0 ] && ok "明细接口有记录" "$n 条" || bad "明细接口有记录" "$n 条"

echo
echo "================ 结果: $pass 通过 / $fail 失败 ================"
