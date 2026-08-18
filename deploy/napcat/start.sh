#!/usr/bin/env bash
# 启动 NapCat 容器。
# 首次登录：浏览器打开 http://127.0.0.1:6099 → 输入 WEBUI_TOKEN → 扫码登录机器人 QQ。
# 若当前用户不在 docker 组，需要 sudo（脚本已自带）。
# 生命周期：脚本进程只负责拉起容器，随后 `logs -f` 一直前台跟随日志；Ctrl+C 只退出
# 日志查看，不会停止采用 `-d` 后台运行的容器。
set -e
# set -e：任何未处理的失败立即结束脚本，避免“启动失败却继续显示旧日志”。
# dirname "$0"：取本脚本所在目录；无论用户从哪里执行，compose 都能找到相邻配置。
cd "$(dirname "$0")"
sudo docker compose up -d
sudo docker compose logs -f --tail=50
