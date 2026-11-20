# ---- ビルド: musl の静的リンクで、単一バイナリ（静的ファイル込み）を作る
FROM rust:1.100-alpine3.24@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 AS build
RUN apk add --no-cache build-base
WORKDIR /src
# 依存だけ先にビルドしてキャッシュする（ソースを変えても依存の再ビルドを避ける）
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
COPY static ./static
RUN touch src/main.rs && cargo build --release --locked

# ---- 実行
FROM alpine:3.24@sha256:f59abe656b692d901e184be3445e084786602108599d2a51d229c0f5a48c71bf
RUN apk add --no-cache su-exec \
 && addgroup -S -g 10001 app && adduser -S -u 10001 -G app -H -h /data app \
 && mkdir -p /data
ENV DB_PATH=/data/anime.db
COPY --from=build /src/target/release/ani-cal /usr/local/bin/ani-cal
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 8000
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD ["ani-cal", "--healthcheck"]

ENTRYPOINT ["/entrypoint.sh"]
CMD ["ani-cal"]
