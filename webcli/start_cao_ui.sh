#!/bin/bash
# 一键启动 CAO（亚马逊 Agent Orchestrator）Web UI
# 用法: 双击或在 Git Bash 里: bash start_cao_ui.cmd 的替代——直接 wsl 执行本脚本
# 效果: WSL 里起 cao-server (9889) + vite dev server (5173)
#       Windows 浏览器访问 http://localhost:5173/
set -e
export PATH=/opt/node/bin:$PATH
cd /root/fleet-orchestrator || exit 1

echo "=== 1. 启动 CAO server (9889) ==="
if ! curl -s -o /dev/null http://localhost:9889/ 2>/dev/null; then
  nohup /root/.local/bin/cao-server > /tmp/cao-server.log 2>&1 < /dev/null &
  sleep 6
fi
curl -s -o /dev/null -w "   cao-server :9889 → HTTP %{http_code}\n" http://localhost:9889/ || true

echo "=== 2. 启动 Web UI (vite 5173) ==="
if ! curl -s -o /dev/null http://localhost:5173/ 2>/dev/null; then
  cd /root/fleet-orchestrator/web || exit 1
  pkill -f "vite.js" 2>/dev/null || true
  sleep 1
  setsid nohup /opt/node/bin/node node_modules/vite/bin/vite.js --host > /tmp/cao-ui.log 2>&1 < /dev/null &
  disown
  sleep 8
fi
curl -s -o /dev/null -w "   vite :5173 → HTTP %{http_code}\n" http://localhost:5173/ || true

echo ""
echo "✅ 完成！浏览器打开:  http://localhost:5173/"
echo "   (代理已配置: /sessions /terminals /agents /flows /memory → :9889)"
