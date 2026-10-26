FROM rust:1-bookworm AS build
WORKDIR /src
# Cache dependencies separately from the application source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY migrations migrations
COPY ui ui
COPY src src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 apphost
COPY --from=build /src/target/release/apphost /usr/local/bin/apphost
USER apphost
ENV APPHOST_BIND=0.0.0.0:8080 APPHOST_LOG_JSON=true
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080"]
ENTRYPOINT ["/usr/local/bin/apphost"]
