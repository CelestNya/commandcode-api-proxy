#!/usr/bin/env bash
# 抗故障路径测试：流中断/静默/垃圾/慢头/错误事件，验证重试与最终交付。
# 用法: bash .probe/resilience.sh [代理端口] [上游端口]
set -u
P="${1:-8799}"
U="${2:-9888}"
B="http://127.0.0.1:$P"
M="http://127.0.0.1:$U"
setmode() { curl -s --noproxy '*' -X POST "$M/__mode" -d "{\"mode\":\"$1\"}" >/dev/null 2>&1; }

REQ='{"model":"claude-sonnet-4-5","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"resilience-$RANDOM"}]}'

run() { # $1 = 模式, $2 = 说明
  setmode "$1"
  local t0=$(date +%s%N)
  curl -s --noproxy '*' --max-time 60 -o /tmp/r.body -D /tmp/r.hdr \
    -X POST "$B/v1/messages" -H 'Content-Type: application/json' -H 'x-api-key: k' -d "$REQ"
  local t1=$(date +%s%N)
  local ms=$(( (t1-t0)/1000000 ))
  local code=$(awk 'NR==1{print $2}' /tmp/r.hdr)
  local evs=$(grep -c '^event:' /tmp/r.body 2>/dev/null || echo 0)
  local bytes=$(wc -c < /tmp/r.body)
  sleep 0.4
  local last=$(curl -s --noproxy '*' "$B/webui/api/attempts?limit=1")
  local att=$(echo "$last" | python -c "import json,sys; d=json.load(sys.stdin); print(d[0]['attempts'] if d else '?')" 2>/dev/null)
  local tag=$(echo "$last" | python -c "import json,sys; d=json.load(sys.stdin); print(d[0].get('errorTag') or '-')" 2>/dev/null)
  local st=$(echo "$last" | python -c "import json,sys; d=json.load(sys.stdin); print(d[0]['status'] if d else '?')" 2>/dev/null)
  printf '  %-12s %-22s 状态=%-4s 事件=%-3s 字节=%-6s 尝试=%s 结果=%s 错误=%s\n' "$1" "$2" "$code" "$evs" "$bytes" "$att" "$st" "$tag"
}

echo "=== 上游故障模式下的客户端交付 ==="
run ok            "正常"
run reset_mid     "流中途断开"
run silent        "完全静默"
run gap           "出字后长空档"
run garbage       "返回垃圾数据"
run error_event   "上游报错事件"
run slow_headers  "响应头极慢"
run trickle       "极慢吐字"

setmode ok
echo "=== 恢复后复查 ==="
run ok            "恢复正常"
