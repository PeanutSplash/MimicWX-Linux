CONTAINER = mimicwx-linux

-include Makefile.local

# 首次构建并启动
up:
	docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d --build

# 启动（不重新构建）
start:
	docker compose -f docker-compose.yml -f docker-compose.dev.yml up -d

# 停止
stop:
	docker compose down

# 热更新开发模式
dev:
	docker exec -u wechat -it $(CONTAINER) bash /home/wechat/mimicwx-linux/docker/dev-watch.sh

# 容器内手动编译一次
build:
	docker exec -u wechat -it $(CONTAINER) bash -c 'export PATH="/home/wechat/.cargo/bin:$$PATH" && cd /home/wechat/mimicwx-linux && cargo build'

# 进入容器
shell:
	docker exec -u wechat -it $(CONTAINER) bash

# 查看日志
logs:
	docker logs -f $(CONTAINER)

# 控制台（Ctrl+P Ctrl+Q 退出）
attach:
	docker attach $(CONTAINER)

.PHONY: up start stop dev build shell logs attach
