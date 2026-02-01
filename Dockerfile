FROM rust:1.88-bookworm AS builder

WORKDIR /app

# 构建依赖（reqwest/native-tls 等会用到 OpenSSL）
RUN apt-get update \
  && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
  && rm -rf /var/lib/apt/lists/*

# 先复制依赖清单（利用 Docker layer cache 加速构建）
COPY Cargo.toml Cargo.lock askama.toml ./
COPY src ./src
COPY templates ./templates

RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/ant2api /app/server

ENV HOST=0.0.0.0
ENV PORT=8045

# 运行时数据目录：默认与仓库 .env/.env.example 一致（相对 /app）
ENV DATA_DIR=./data

# 容器环境建议更积极归还内存（jemalloc）
ENV MALLOC_CONF=background_thread:true,dirty_decay_ms:0,muzzy_decay_ms:0,narenas:1

# RSS 守护：当 RSS 超过阈值时触发 jemalloc purge（best-effort）
ENV RSS_GUARD_MAX_MB=50
ENV RSS_GUARD_INTERVAL_MS=1000
ENV RSS_GUARD_COOLDOWN_MS=5000

# 数据卷挂载点（对应 DATA_DIR=./data）
VOLUME ["/app/data"]
EXPOSE 8045

ENTRYPOINT ["/app/server"]
