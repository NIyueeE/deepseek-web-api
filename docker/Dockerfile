FROM alpine:3.21
RUN apk add --no-cache ca-certificates

WORKDIR /app

ARG TARGETARCH
COPY target/docker/${TARGETARCH}/ds-free-api /app/ds-free-api
# 配置示例单独存放：/app/config 经常被 bind mount 遮蔽（见 entrypoint.sh）
COPY docker/config.example.toml /app/config.example.toml
COPY docker/entrypoint.sh /app/entrypoint.sh
RUN chmod +x /app/entrypoint.sh

ENV RUST_LOG=info \
    DS_DATA_DIR=/app/data \
    DS_CONFIG_PATH=/app/config/config.toml

EXPOSE 22217

ENTRYPOINT ["/app/entrypoint.sh"]
