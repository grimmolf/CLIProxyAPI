FROM rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY rust ./rust
RUN cargo build --release --locked -p cliproxy-rs

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
RUN useradd --uid 10001 --create-home proxy
COPY --from=build /build/target/release/cliproxy-rs /usr/local/bin/cliproxy-rs
USER proxy
WORKDIR /home/proxy
VOLUME ["/home/proxy/data"]
EXPOSE 8318
ENTRYPOINT ["cliproxy-rs"]
CMD ["--listen", "0.0.0.0:8318", "--data-dir", "/home/proxy/data"]
