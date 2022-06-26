FROM lukemathwalker/cargo-chef:latest-rust-slim-bullseye as planner
LABEL builder=true multistage_tag="phrases-planner"
WORKDIR /app
COPY . .
RUN cargo chef prepare  --recipe-path recipe.json

FROM lukemathwalker/cargo-chef:latest-rust-slim-bullseye as builder
LABEL builder=true multistage_tag="phrases-builder"
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin dgg-phrases

FROM debian:bullseye-slim
WORKDIR /app
COPY --from=builder /app/target/release/dgg-phrases .
CMD ["./dgg-phrases"]