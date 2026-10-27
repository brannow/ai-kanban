# ai-kanban relay -- the server a team shares boards through. See docs/sync.md.
#
#   docker build -t ai-kanban-relay .
#   docker run -d --name kanban -v kanban-data:/data -p 7373:7373 ai-kanban-relay
#   docker exec kanban ai-kanban-relay user add benny
#   docker exec kanban ai-kanban-relay key add benny --tag b1 --label laptop
#
# TLS belongs to the reverse proxy in front of it. Behind nginx, the event stream needs
# `proxy_buffering off;` or notifications arrive in bursts.

# A C compiler is needed: rusqlite builds SQLite from source (`bundled`), so the image does
# not depend on whatever SQLite the base ships.
FROM rust:1.99-trixie AS build
WORKDIR /src
# The whole workspace. Only the relay is built, but Cargo loads every member (the client at
# the root among them) before it builds any one of them.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
# Cache mounts keep the registry and build artifacts between builds; the binary is copied out
# because the cached target directory is not part of the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ai-kanban-relay \
 && install -D target/release/ai-kanban-relay /out/ai-kanban-relay
# /data must exist and belong to the runtime user before the volume is first mounted: a named
# volume copies the image directory's ownership, and distroless has no shell to fix it later.
RUN install -d -o 65532 -g 65532 /out/data

# Distroless: glibc and CA certificates, no shell, no package manager, and a non-root user
# (65532) by default.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /out/ai-kanban-relay /usr/local/bin/ai-kanban-relay
COPY --from=build --chown=65532:65532 /out/data /data
ENV AI_KANBAN_SERVER_DB=/data/server.db
VOLUME /data
EXPOSE 7373
# `health` is a single request made with the standard library, because the image has no curl
# to make it with.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s \
    CMD ["/usr/local/bin/ai-kanban-relay", "health"]
ENTRYPOINT ["/usr/local/bin/ai-kanban-relay"]
# Inside the container the relay must listen on every interface; outside it, the default stays
# loopback. Snapshots every 6h, the newest 28 kept (a week), in /data/snapshots.
CMD ["--listen", "0.0.0.0:7373", "--snapshot-every", "6h", "--keep", "28"]
