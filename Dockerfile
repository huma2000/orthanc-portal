# ---------------------------------------------------------------------
#  Orthanc Portal - container image (two stages)
#  Build:  docker build -t orthanc-portal .
# ---------------------------------------------------------------------

# Stage 1: compile the program (needs a C compiler for the bundled SQLite;
# the official rust image already contains one).
FROM rust:1-bookworm AS build
WORKDIR /src

# 1a. Build ONLY the dependencies first. This layer is cached by Docker and is rebuilt
#     only when Cargo.toml / Cargo.lock change, so editing the program's own code
#     (or the web UI) no longer recompiles ~300 third-party crates every time.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
 && echo 'fn main() {}' > src/main.rs \
 && cargo build --release \
 && rm -rf src target/release/.fingerprint/orthanc-portal-*

# 1b. Now the program itself. The web UI (HTML/CSS/JS) is embedded in the executable
#     at compile time, hence the `static` folder.
COPY src ./src
COPY static ./static
# `touch`: COPY keeps the files' original dates, which can be OLDER than the placeholder
# built in 1a; Cargo would then think the program is up to date and ship the empty
# `fn main() {}` (a container that exits at once with no log). The size check stops
# the build if that ever happens again.
RUN find src static -type f -exec touch {} + \
 && cargo build --release \
 && test "$(stat -c %s target/release/orthanc-portal)" -gt 2000000

# Stage 2: tiny runtime image with just the executable.
FROM debian:bookworm-slim
# Run as an unprivileged user; /data holds the SQLite database (mount a volume here).
RUN useradd --system --uid 10001 --home /data portal \
 && mkdir -p /data && chown portal /data
COPY --from=build /src/target/release/orthanc-portal /usr/local/bin/orthanc-portal
USER portal
# The listening addresses always come from these variables (LISTENERS_FROM_ENV), never
# from Settings: they must match the ports docker-compose publishes. The compose files
# set them from INTRANET_PORT / INTERNET_PORT in their .env file.
ENV BIND_ADDR=0.0.0.0:8080 \
    REMOTE_BIND_ADDR=0.0.0.0:8081 \
    LISTENERS_FROM_ENV=true \
    PORTAL_INSTALL=docker \
    DATABASE_PATH=/data/portal.db
VOLUME /data
# Default ports: 8080 = intranet (staff portal), 8081 = internet (invitation links only,
# for the Cloudflare Tunnel). Documentation only: the real ones are BIND_ADDR / REMOTE_BIND_ADDR.
EXPOSE 8080 8081

# Docker marks the container "unhealthy" if the portal stops answering. (Uses bash's
# built-in TCP support: the slim image has no curl.) It checks the intranet port, taken
# from BIND_ADDR (the part after the last ':').
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
  CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/$(echo "$BIND_ADDR" | grep -o "[0-9]*$") && printf "GET /healthz HTTP/1.0\r\n\r\n" >&3 && head -n1 <&3 | grep -q " 200 "' || exit 1

CMD ["orthanc-portal"]
