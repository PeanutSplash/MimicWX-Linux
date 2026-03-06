#!/bin/bash
# 容器内热编译脚本
# 监听 src/ 变化 → 自动编译 → 替换二进制 → 重启 MimicWX
#
# 用法: docker exec -u wechat -it mimicwx-linux bash /home/wechat/mimicwx-linux/docker/dev-watch.sh

set -e

cd /home/wechat/mimicwx-linux

# 加载 Rust 环境
export PATH="/home/wechat/.cargo/bin:$PATH"

# 颜色定义
RESET="\033[0m"
BOLD="\033[1m"
DIM="\033[2m"
GREEN="\033[32m"
YELLOW="\033[33m"
RED="\033[31m"
CYAN="\033[36m"

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

echo ""
echo -e "  ${BOLD}${CYAN}MIMICWX${RESET} ${GREEN}v${VERSION}${RESET}  ${DIM}dev server running${RESET}"
echo ""
echo -e "  ${DIM}watching${RESET}  src/"
echo -e "  ${DIM}mode${RESET}     debug"
echo ""

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

cargo watch \
  --quiet \
  --poll \
  -w src \
  -s "bash ${SCRIPT_DIR}/dev-build.sh"
