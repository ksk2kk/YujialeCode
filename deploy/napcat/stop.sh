#!/usr/bin/env bash
# 停止并移除 NapCat 容器（保留 config/ 与 logs/ 数据）
# `down` 会删除 compose 创建的容器/网络，但这里的绑定目录是宿主文件，不随容器销毁。
set -e
cd "$(dirname "$0")"
sudo docker compose down
