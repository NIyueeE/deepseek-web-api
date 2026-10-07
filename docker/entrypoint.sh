#!/bin/sh
# 容器入口：确保配置文件存在，再启动服务。
#
# 为什么需要它：`/app/config` 在 compose / `-v` 场景下会被宿主机目录**整体遮蔽**，
# 镜像里预置的配置示例看不到，于是二进制会按代码默认值自动生成
# `host = "127.0.0.1"` 的配置 —— 服务只监听容器内回环，端口映射形同虚设
# （现象：容器 Up，但宿主机访问 22217 无响应）。
# 这里在配置缺失/为空时用内置示例（`host = "0.0.0.0"`）初始化。
set -eu

CONFIG="${DS_CONFIG_PATH:-/app/config/config.toml}"
EXAMPLE="/app/config.example.toml"

# 支持二进制自身的 `-c <path>`（它只认 -c；显式指定时若文件不存在会直接报错，
# 所以这里必须先按同一个路径做初始化）。getopts 只更新 OPTIND，不会改动 "$@"。
while getopts ":c:" opt; do
    case "$opt" in
        c) CONFIG="$OPTARG" ;;
    esac
done

if [ ! -s "$CONFIG" ]; then
    if mkdir -p "$(dirname "$CONFIG")" && cp "$EXAMPLE" "$CONFIG" && chmod 600 "$CONFIG"; then
        echo "[entrypoint] 已用内置示例初始化配置: $CONFIG（host = 0.0.0.0）"
    else
        # 只读根文件系统等场景：交给二进制自己处理，并显式提示可能的原因
        echo "[entrypoint] 警告: 无法写入 $CONFIG，将由服务自行创建配置" >&2
    fi
fi

exec /app/ds-free-api "$@"
