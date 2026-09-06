#!/usr/bin/env bash
# =====================================================================
# FlyGo tailscale serve 外网暴露：只留面板 / 控制面 / webcli（HTTPS）。
# 容器端口（5244/8096/2283…）不再走 serve：面板进程的 tcpfwd 转发层
# （src/api/tcpfwd.rs）直接在 Tailscale IP + 局域网 IP 上监听并中继到
# 127.0.0.1，是唯一 owner —— serve 只认域名不认 IP，远程经 IP 必 404，
# 且 serve 会抢占端口导致转发层 bind 冲突，所以容器端口必须让路。
# 面板收到容器端口后每 30s 调和时自动逐出残留的 serve 条目；
# 本脚本只负责面板三件套的重建（重装 / 重登 Tailscale 后跑一次）。
#
# 用法：
#   ./scripts/tailscale-serve.sh            # 幂等重建全部 serve 入口
#   ./scripts/tailscale-serve.sh --verify   # 重建后 curl 实测每个入口
#   ./scripts/tailscale-serve.sh --dry-run  # 只打印将执行的命令
#
# 说明：
#   - 面板/控制面/webcli 走 HTTPS（tailscale serve --https，自带证书）。
#   - 容器端口由面板 tcpfwd 转发层接管（Tailscale IP 直连，无需证书），
#     客户端用 http://<100.x>:<端口>/ 直接访问，不再需要 serve 条目。
#   - 本脚本幂等：重复执行只是把各挂载点重设为相同值，不中断已有连接。
#   - 重装 / 重登 Tailscale 后跑一次即可恢复面板外网入口。
# =====================================================================
set -euo pipefail
export MSYS_NO_PATHCONV=1   # 防 Git Bash 把 /ctrl 当路径转换

if command -v tailscale >/dev/null 2>&1; then
  TS=tailscale
elif [ -x "C:/Program Files/Tailscale/tailscale.exe" ]; then
  TS="C:/Program Files/Tailscale/tailscale.exe"
else
  echo "error: tailscale not found (install Tailscale or put it on PATH)" >&2
  exit 1
fi

# 格式: <端口> <target_host:port> <模式: http|https> [路径，可空]
# 只留面板三件套；容器端口由 tcpfwd 接管，此处不再挂载。
MOUNTS=(
  "443   127.0.0.1:8388  https"
  "443   127.0.0.1:9001  https  /ctrl"
  "8443  127.0.0.1:8788  https"
)

for m in "${MOUNTS[@]}"; do
  read -r port target mode path <<<"$m"
  if [ -n "$path" ]; then
    cmd=("$TS" serve --bg --${mode}="$port" --set-path="$path" "$target")
    label=":$port$path"
  else
    cmd=("$TS" serve --bg --${mode}="$port" "$target")
    label=":$port/"
  fi
  if [ "${1:-}" = "--dry-run" ]; then
    echo "DRY: ${cmd[*]}"
    continue
  fi
  echo "== serve $label -> $target =="
  "${cmd[@]}"
done

if [ "${1:-}" = "--dry-run" ]; then echo "(dry-run only)"; exit 0; fi

echo; echo "== final serve status =="
"$TS" serve status

if [ "${1:-}" = "--verify" ]; then
  FQDN=${TS_FQDN:-}
  if [ -z "$FQDN" ]; then
    # status --json 里 Peer 也可能带 DNSName，必须取 Self 块的（有同名 Peer 也不会串名）
    if command -v python >/dev/null 2>&1; then
      FQDN=$("$TS" status --json 2>/dev/null | python -c "import json,sys; d=json.load(sys.stdin); print((d.get('Self') or {}).get('DNSName','').rstrip('.'))" 2>/dev/null || true)
    fi
    if [ -z "$FQDN" ]; then
      FQDN=$("$TS" status --json 2>/dev/null | sed -n 's/.*"Self": {.*"DNSName": *"\([^"]*\)".*/\1/p' | head -n1 | sed 's/\.$//')
    fi
  fi
  echo; echo "== curl -I https://$FQDN/<...> =="
  for u in "https://$FQDN/" "https://$FQDN:8443/"; do
    # MSYS_NO_PATHCONV=1 下 curl -o /dev/null 不可用（会被当字面路径），改用临时文件
    _out=$(mktemp)
    code=$(curl -sk -o "$_out" -w "%{http_code}" --max-time 10 "$u" || echo 000)
    rm -f "$_out"
    echo "$u -> HTTP $code"
  done
fi
