#!/bin/bash
# cargo watch 触发的单次构建脚本

cd /home/wechat/mimicwx-linux
export PATH="/home/wechat/.cargo/bin:$PATH"

RESET="\033[0m"
BOLD="\033[1m"
DIM="\033[2m"
GREEN="\033[32m"
YELLOW="\033[33m"
RED="\033[31m"
CYAN="\033[36m"

start_time=$(date +%s%N)
timestamp=$(date +%H:%M:%S)
tmpfile=$(mktemp)

echo -e "${DIM}${timestamp}${RESET} ${CYAN}[watch]${RESET} file change detected, rebuilding..."

if cargo build 2>"$tmpfile"; then
  end_time=$(date +%s%N)
  elapsed_ms=$(( (end_time - start_time) / 1000000 ))

  # 检查是否有 warnings
  warn_count=$(grep -c '^ *warning\[' "$tmpfile" 2>/dev/null || true)
  if [ "$warn_count" -gt 0 ]; then
    echo -e "${DIM}${timestamp}${RESET} ${YELLOW}[build]${RESET} ${warn_count} warning(s)"
  fi

  # 替换二进制
  sudo rm -f /usr/local/bin/mimicwx
  sudo cp target/debug/mimicwx /usr/local/bin/mimicwx
  sudo setcap cap_sys_admin+ep /usr/local/bin/mimicwx

  # 重启进程
  if sudo pkill -x mimicwx 2>/dev/null; then
    echo -e "${DIM}${timestamp}${RESET} ${GREEN}[build]${RESET} rebuilt & restarted in ${BOLD}${elapsed_ms}ms${RESET}"
  else
    echo -e "${DIM}${timestamp}${RESET} ${GREEN}[build]${RESET} rebuilt in ${BOLD}${elapsed_ms}ms${RESET} ${DIM}(not running)${RESET}"
  fi
else
  end_time=$(date +%s%N)
  elapsed_ms=$(( (end_time - start_time) / 1000000 ))

  echo -e "${DIM}${timestamp}${RESET} ${RED}[build]${RESET} failed in ${elapsed_ms}ms"
  echo ""
  grep -E '(^error|^\s*-->|^\s*\||^\s*=|help:|For more)' "$tmpfile" | while IFS= read -r line; do
    echo -e "  ${RED}${line}${RESET}"
  done
  echo ""
fi

rm -f "$tmpfile"
