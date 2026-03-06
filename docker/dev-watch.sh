#!/bin/bash
# 容器内热编译脚本
# 监听 src/ 变化 → 自动编译 → 替换二进制 → 重启 MimicWX
#
# 用法: docker exec -it mimicwx-linux bash /home/wechat/mimicwx-linux/docker/dev-watch.sh

set -e

cd /home/wechat/mimicwx-linux

# 加载 Rust 环境
source /home/wechat/.cargo/env

echo "================================"
echo " MimicWX Dev Watch"
echo " 监听 src/ 变化, 自动编译+重启"
echo "================================"

cargo watch \
  -w src \
  -s '
    echo ">>> 编译中..."
    if cargo build --release 2>&1; then
      echo ">>> 编译成功, 替换二进制..."
      sudo cp target/release/mimicwx /usr/local/bin/mimicwx
      sudo setcap cap_sys_admin+ep /usr/local/bin/mimicwx
      # 给 MimicWX 发送重启信号 (SIGUSR1 → 优雅重启)
      # 回退: 直接 kill 让 start.sh 重启循环接管
      MIMICWX_PID=$(pgrep -x mimicwx || true)
      if [ -n "$MIMICWX_PID" ]; then
        echo ">>> 重启 MimicWX (PID: $MIMICWX_PID)..."
        kill "$MIMICWX_PID" 2>/dev/null || true
        echo ">>> 等待 start.sh 重启循环..."
      else
        echo ">>> MimicWX 未运行, 跳过重启"
      fi
      echo ">>> 热更新完成!"
    else
      echo ">>> 编译失败, 等待下次修改..."
    fi
  '
