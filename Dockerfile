FROM rust:1.86 as builder
WORKDIR /app
# Install build dependencies for ffmpeg-sys-next and bindgen (libclang)
RUN apt-get update && apt-get install -y pkg-config libavutil-dev libavformat-dev libavcodec-dev libavdevice-dev libavfilter-dev libswscale-dev libswresample-dev ffmpeg clang libclang-dev && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ffmpeg && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/videostream-rs /usr/local/bin/videostream-rs
COPY config.toml ./
ENTRYPOINT ["/usr/local/bin/videostream-rs"]