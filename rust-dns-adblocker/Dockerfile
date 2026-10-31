# dnsblock – Multi-Stage-Build (kein lokales Rust nötig, alles im Container)
# Build:  docker build -t dnsblock:local .
# Run:    siehe compose.yaml / docker compose up -d

# ---------- Build ----------
FROM rust:1-slim AS build
WORKDIR /build

# build-essential: linker/cc für cargo; git wird nicht gebraucht (Tarball-Quelle)
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ENV CARGO_NET_RETRY=10 CARGO_TERM_COLOR=never
RUN cargo build --release -p dnsblock

# ---------- Runtime ----------
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl dnsutils \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin dnsblock

COPY --from=build /build/target/release/dnsblock /usr/local/bin/dnsblock

# Listen + Konfig (Container fester Pfad /opt/dnsblock; Daten: Volume /var/lib/dnsblock)
RUN mkdir -p /opt/dnsblock/blocklists /var/lib/dnsblock
COPY blocklists/ /opt/dnsblock/blocklists/
COPY config/docker.toml /opt/dnsblock/config.toml
RUN chown -R dnsblock:dnsblock /opt/dnsblock /var/lib/dnsblock

# Port 53 als Nicht-Root (CAP_NET_BIND_SERVICE via setcap),
# zusätzlich docker: cap_add in compose.yaml
RUN setcap 'cap_net_bind_service=+ep' /usr/local/bin/dnsblock

USER dnsblock
EXPOSE 53/tcp 53/udp 8080/tcp
VOLUME ["/var/lib/dnsblock"]
ENTRYPOINT ["/usr/local/bin/dnsblock", "/opt/dnsblock/config.toml"]
