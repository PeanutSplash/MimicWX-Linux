#!/bin/bash
# MimicWX-Linux 容器启动脚本
# 启动顺序: D-Bus → X11/WM → AT-SPI2 → WeChat → GDB密钥提取 → MimicWX

set +e  # 不因单个命令失败而退出

export MIMICWX_DEBUG="${MIMICWX_DEBUG:-0}"
export MIMICWX_DISPLAY_NUM="${MIMICWX_DISPLAY_NUM:-1}"
export MIMICWX_DISPLAY=":${MIMICWX_DISPLAY_NUM}"
export MIMICWX_NOVNC_PORT="${MIMICWX_NOVNC_PORT:-6080}"
export MIMICWX_VNC_PORT="$((5900 + MIMICWX_DISPLAY_NUM))"

if [ "$MIMICWX_DEBUG" = "1" ] && ! command -v vncserver >/dev/null 2>&1; then
  echo "[start.sh] ⚠️  MIMICWX_DEBUG=1 但未安装 VNC 工具，回退到 headless 模式"
  echo "[start.sh] 构建时请使用 INSTALL_DEBUG_TOOLS=1"
  export MIMICWX_DEBUG=0
fi

# ============================================================
# 0) 系统服务 (root)
# ============================================================
mkdir -p /run/dbus
dbus-daemon --system --fork 2>/dev/null || true

# 允许 ptrace (GDB 密钥提取需要)
echo 0 > /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || true

# 修复权限
chmod 666 /dev/uinput 2>/dev/null || true
chown -R wechat:wechat /home/wechat/.xwechat 2>/dev/null || true
chown -R wechat:wechat /home/wechat/mimicwx-linux 2>/dev/null || true
mkdir -p /home/wechat/.xwechat/crashinfo/attachments
chown -R wechat:wechat /home/wechat/.xwechat

# 确保 /tmp/.X11-unix 存在且权限正确
mkdir -p /tmp/.X11-unix
chmod 1777 /tmp/.X11-unix

# VNC 密码 (仅 debug 模式需要)
if [ "$MIMICWX_DEBUG" = "1" ]; then
  su - wechat -c '
    mkdir -p ~/.vnc
    echo "mimicwx" | vncpasswd -f > ~/.vnc/passwd
    chmod 600 ~/.vnc/passwd
  '
fi

# ============================================================
# GDB 密钥提取监视器 (root 后台)
# 等待 WeChat PID 文件出现后自动 attach 提取密钥
# ============================================================
if [ ! -f /tmp/wechat_key.txt ]; then
  setsid bash -c '
    echo "[GDB] 密钥提取监视器启动, 等待 WeChat PID..."
    for _i in $(seq 1 90); do
      [ -f /tmp/wechat.pid ] && break
      sleep 1
    done
    if [ -f /tmp/wechat.pid ]; then
      WECHAT_PID=$(cat /tmp/wechat.pid)
      echo "[GDB] 检测到 WeChat (PID: $WECHAT_PID), 开始提取密钥..."
      sleep 5
      gdb -batch -nx -p "$WECHAT_PID" -x /usr/local/bin/extract_key.py \
        > /tmp/gdb_extract.log 2>&1 || true
      echo "[GDB] 密钥提取完成, 详见 /tmp/gdb_extract.log"
    else
      echo "[GDB] ❌ 超时: 未找到 WeChat PID"
    fi
  ' &
fi

# ============================================================
# 1-8) 用户空间服务 (wechat 用户)
# ============================================================
su - wechat << 'USEREOF'
  set +e  # 确保单个命令失败不会终止整个 heredoc
  export LANG=zh_CN.UTF-8
  export LANGUAGE=zh_CN:zh
  export LC_ALL=zh_CN.UTF-8

  # 1) D-Bus session
  eval $(dbus-launch --sh-syntax)
  export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
  export QT_ACCESSIBILITY=1

  export DISPLAY="${MIMICWX_DISPLAY}"
  export VNC_PORT="${MIMICWX_VNC_PORT}"
  export NOVNC_PORT="${MIMICWX_NOVNC_PORT}"

  # 2) X11 会话: 默认 headless (Xvfb + openbox), debug 才启用 VNC/XFCE
  if [ "$MIMICWX_DEBUG" = "1" ]; then
    echo "[start.sh] 启动 debug 桌面 (VNC + XFCE)..."
    vncserver -kill "$DISPLAY" 2>/dev/null || true
    rm -f "/tmp/.X${MIMICWX_DISPLAY_NUM}-lock" "/tmp/.X11-unix/X${MIMICWX_DISPLAY_NUM}" 2>/dev/null || true
    sleep 1
    vncserver "$DISPLAY" -geometry 1280x720 -depth 24 -localhost no 2>&1 | tee /tmp/vnc_startup.log
    VNC_EXIT=${PIPESTATUS[0]}
    if [ "$VNC_EXIT" != "0" ]; then
      echo "[start.sh] ⚠️ VNC 首次启动失败 (exit=$VNC_EXIT), 清理后重试..."
      vncserver -kill "$DISPLAY" 2>/dev/null || true
      rm -f "/tmp/.X${MIMICWX_DISPLAY_NUM}-lock" "/tmp/.X11-unix/X${MIMICWX_DISPLAY_NUM}" 2>/dev/null || true
      sleep 2
      vncserver "$DISPLAY" -geometry 1280x720 -depth 24 -localhost no 2>&1 | tee -a /tmp/vnc_startup.log
    fi
  else
    echo "[start.sh] 启动 headless 桌面 (Xvfb + openbox)..."
    pkill -f "Xvfb ${DISPLAY}" 2>/dev/null || true
    rm -f "/tmp/.X${MIMICWX_DISPLAY_NUM}-lock" "/tmp/.X11-unix/X${MIMICWX_DISPLAY_NUM}" 2>/dev/null || true
    Xvfb "$DISPLAY" -screen 0 1280x720x24 -ac > /tmp/xvfb.log 2>&1 &
    sleep 1
    openbox > /tmp/openbox.log 2>&1 &
  fi
  sleep 2

  if [ -e "/tmp/.X11-unix/X${MIMICWX_DISPLAY_NUM}" ]; then
    echo "[start.sh] ✅ X11 显示已就绪 (DISPLAY=$DISPLAY)"
  else
    echo "[start.sh] ❌ X11 显示未就绪! 后续服务可能不可用"
    cat /tmp/vnc_startup.log 2>/dev/null || true
    cat /tmp/xvfb.log 2>/dev/null || true
  fi

  # 禁用 XFCE 屏保/锁屏/电源管理 (防止息屏)
  xset s off 2>/dev/null || true
  xset -dpms 2>/dev/null || true
  xset s noblank 2>/dev/null || true
  xfconf-query -c xfce4-screensaver -p /saver/enabled -s false 2>/dev/null || true
  xfconf-query -c xfce4-power-manager -p /xfce4-power-manager/dpms-enabled -s false 2>/dev/null || true
  xfconf-query -c xfce4-power-manager -p /xfce4-power-manager/blank-on-ac -s 0 2>/dev/null || true

  # 3) 清理 XFCE 自启的 AT-SPI2 (避免 bus 冲突)
  for _r in 1 2 3; do
    pkill -9 -f at-spi-bus-launcher 2>/dev/null || true
    pkill -9 -f at-spi2-registryd 2>/dev/null || true
    sleep 0.5
  done
  rm -f ~/.cache/at-spi/bus_1 ~/.cache/at-spi/bus 2>/dev/null || true
  sleep 1

  # 4) 启动唯一的 AT-SPI2 bus
  /usr/libexec/at-spi-bus-launcher &
  sleep 2

  # 5) 获取 AT-SPI2 bus 地址
  A11Y_ADDR=$(dbus-send --session --dest=org.a11y.Bus --print-reply \
    /org/a11y/bus org.a11y.Bus.GetAddress 2>/dev/null \
    | grep string | sed 's/.*"\(.*\)"/\1/')
  if [ -n "$A11Y_ADDR" ]; then
    export AT_SPI_BUS_ADDRESS="$A11Y_ADDR"
    echo "[start.sh] ✅ AT-SPI2 bus: $A11Y_ADDR"
  else
    echo "[start.sh] ⚠️ AT-SPI2 bus address not found"
  fi

  # 保存环境变量 (供 docker exec 使用, 用 echo 避免嵌套 heredoc)
  echo "export DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS" > ~/.dbus_env
  echo "export DISPLAY=$DISPLAY" >> ~/.dbus_env
  echo "export LANG=zh_CN.UTF-8" >> ~/.dbus_env
  echo "export LANGUAGE=zh_CN:zh" >> ~/.dbus_env
  echo "export LC_ALL=zh_CN.UTF-8" >> ~/.dbus_env
  echo "export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1" >> ~/.dbus_env
  echo "export QT_ACCESSIBILITY=1" >> ~/.dbus_env
  [ -n "$AT_SPI_BUS_ADDRESS" ] && echo "export AT_SPI_BUS_ADDRESS=$AT_SPI_BUS_ADDRESS" >> ~/.dbus_env

  # 6) 启动微信 (写 PID 供 GDB 使用, 保留 stderr 日志)
  echo "[start.sh] 启动微信..."
  wechat --no-sandbox --disable-gpu > /tmp/wechat_stdout.log 2>&1 &
  WECHAT_PID=$!
  echo $WECHAT_PID > /tmp/wechat.pid
  echo "[start.sh] ✅ 微信已启动 (PID: $WECHAT_PID)"
  # 等待微信窗口就绪 (轮询替代固定 sleep, 最多 60 秒)
  echo "[start.sh] 等待微信窗口就绪..."
  for _wait in $(seq 1 30); do
    # 检查微信窗口 (替代 xdotool, 使用进程窗口检测)
    if pgrep -x wechat >/dev/null 2>&1 && \
       xprop -root _NET_CLIENT_LIST 2>/dev/null | grep -q "0x"; then
      echo "[start.sh] ✅ 微信窗口已就绪 (${_wait}x2s)"
      break
    fi
    sleep 2
  done

  # 验证微信是否存活
  if kill -0 $WECHAT_PID 2>/dev/null; then
    echo "[start.sh] ✅ 微信进程存活"
  else
    echo "[start.sh] ❌ 微信进程已退出! 日志:"
    cat /tmp/wechat_stdout.log 2>/dev/null | tail -20
  fi

  # 7) noVNC (仅 debug)
  if [ "$MIMICWX_DEBUG" = "1" ]; then
    echo "[start.sh] 启动 noVNC..."
    websockify --web /usr/share/novnc "$NOVNC_PORT" "localhost:$VNC_PORT" &
    echo "[start.sh] ✅ noVNC 已启动 (http://localhost:${NOVNC_PORT}/vnc.html)"
  else
    echo "[start.sh] headless 模式: 未启动 VNC/noVNC"
  fi

  # 环境变量已保存到 ~/.dbus_env (供 MimicWX 使用)
USEREOF

# ============================================================
# 8) MimicWX (heredoc 之外运行, 保留 stdin 用于控制台命令)
# ============================================================
echo "=============================="
echo "MimicWX-Linux Ready!"
if [ "$MIMICWX_DEBUG" = "1" ]; then
  echo "noVNC: http://localhost:${MIMICWX_NOVNC_PORT}/vnc.html"
else
  echo "Desktop: headless (${MIMICWX_DISPLAY})"
fi
echo "API:   http://${MIMICWX_BIND_ADDR:-0.0.0.0:8899}"
echo "=============================="

# 重启循环: 退出码 42 = 重启请求
while true; do
  # 通过 su -c 运行, 加载已保存的环境变量, 保留 stdin
  su - wechat -c '
    source ~/.dbus_env 2>/dev/null
    export RUST_LOG=mimicwx=info
    exec /usr/local/bin/mimicwx
  '
  EXIT_CODE=$?
  if [ "$EXIT_CODE" = "42" ]; then
    echo "[start.sh] 🔄 MimicWX 重启中 (3秒后)..."
    sleep 3
    echo "[start.sh] 🔄 重新启动 MimicWX..."
    continue
  fi
  # 被信号 kill (热更新) → 也重启
  if [ "$EXIT_CODE" -gt 128 ] 2>/dev/null; then
    echo "[start.sh] 🔄 MimicWX 被信号终止 (code=$EXIT_CODE), 3秒后重启..."
    sleep 3
    continue
  fi
  echo "[start.sh] MimicWX 已退出 (code=$EXIT_CODE)"
  break
done

echo "[start.sh] 容器退出"
