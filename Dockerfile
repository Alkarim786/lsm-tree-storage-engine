FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
COPY examples ./examples
RUN cargo build --release --examples

FROM debian:bookworm-slim
COPY --from=build /src/target/release/examples/kv_cli /usr/local/bin/kv_cli
COPY --from=build /src/target/release/examples/bench /usr/local/bin/bench
VOLUME /data
ENTRYPOINT ["kv_cli"]
CMD ["/data"]
