#!/bin/bash
# 容器内热编译脚本
# 监听 src/ 变化 → 自动编译 → 替换二进制 → 重启 MimicWX
#
# 用法: docker exec -u wechat -it mimicwx-linux bash /home/wechat/mimicwx-linux/docker/dev-watch.sh

set -e

cd /home/wechat/mimicwx-linux

# 加载 Rust 环境
export PATH="/home/wechat/.cargo/bin:$PATH"

echo "================================"
echo " MimicWX Dev Watch"
echo " 监听 src/ 变化, 自动编译+重启"
echo "================================"

cargo watch \
  --poll \
  -w src \
  -x 'build' \
  -s 'sudo rm -f /usr/local/bin/mimicwx && sudo cp target/debug/mimicwx /usr/local/bin/mimicwx && sudo setcap cap_sys_admin+ep /usr/local/bin/mimicwx && echo ">>> 二进制已替换" && sudo pkill -x mimicwx && echo ">>> 已重启" || echo ">>> 替换完成, MimicWX 未运行"'
