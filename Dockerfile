
FROM rust:1.99-trixie AS builder

WORKDIR /app

ARG service
ARG features

COPY . .

RUN cargo build --release

FROM debian:trixie-slim AS runtime

ARG service


ENV USER=hcloud
ENV UID=42069

RUN useradd \
    --home-dir "/nonexistent" \
    --shell "/usr/sbin/nologin" \
    --no-create-home \
    --user-group \
    --uid "${UID}" \
    "${USER}"

# Install ca-certificates for HTTPS support
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app


COPY --from=builder /app/target/release/gitlab_hcloud /app/app

RUN chown -R "${USER}:${USER}" /app

# Use the flexi user
USER hcloud:hcloud
# Set entrypoint to run backend
ENTRYPOINT ["/app/app"]
