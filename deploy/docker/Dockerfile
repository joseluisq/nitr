# Reference container image for a Nitr application.
#
# Build from your application directory (nitr.toml, app.lua, public/, ...):
#
#   docker build -f deploy/docker/Dockerfile -t myapp .
#   docker run -p 3000:3000 -v myapp-data:/app/data myapp
#
# Two stages: the first builds the `nitr` binary once (cache-friendly),
# the second is the runtime image with just the binary and the app files.

FROM rust:1-slim AS build
WORKDIR /src
# Build the released CLI from crates.io. To build from a checkout instead:
#   COPY . .  &&  cargo install --path crates/nitr-cli
RUN cargo install nitr-cli

FROM debian:stable-slim
# curl exists solely for the HEALTHCHECK below; drop both if your
# orchestrator probes over TCP itself (Kubernetes httpGet needs no curl).
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Run as a dedicated non-root user: Nitr embeds a scripting language, and
# nothing it does needs root.
RUN useradd --system --create-home --home-dir /app nitr
WORKDIR /app
COPY --from=build /usr/local/cargo/bin/nitr /usr/local/bin/nitr
# The application: configuration, scripts, static files, migrations.
COPY --chown=nitr:nitr . /app
USER nitr

# The SQLite database must live on a volume — it is mutable state, and an
# image layer is not where state belongs. Point `database` in nitr.toml at
# /app/data/app.db and mount a volume there.
VOLUME /app/data

EXPOSE 3000

# Nitr must be PID 1's direct child (or PID 1 itself) to see SIGTERM:
# `docker stop` signals PID 1 only. The exec-form ENTRYPOINT below runs
# nitr as PID 1 directly — no shell in between to swallow the signal —
# and Nitr's own graceful drain handles the rest. Give `docker stop`
# a timeout above [shutdown] grace + stream_grace (default 35s):
#   docker stop --time 40 myapp
ENTRYPOINT ["/usr/local/bin/nitr"]
CMD ["run"]

# Probes wire to the Rust-owned health endpoints ([health] in nitr.toml;
# enabled by default on the main listener):
#   liveness  GET /healthz  — process is alive; never touches Lua
#   readiness GET /readyz   — 200 while accepting, 503 once draining
HEALTHCHECK --interval=10s --timeout=2s --start-period=5s \
  CMD ["curl", "-fsS", "http://127.0.0.1:3000/healthz"]
