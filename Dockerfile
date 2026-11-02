# syntax=docker/dockerfile:1
FROM node:24-bookworm-slim AS ha-web
WORKDIR /source
COPY package.json package-lock.json ./
RUN npm ci
COPY vite.ha.config.js ./
COPY ha-web ./ha-web
COPY assets ./assets
RUN npm run build:ha

FROM rust:1.86-bookworm AS build-source
ARG MYBREWFOLIO_SYNC_API_URL=https://mybrewfolio.com
ARG MYBREWFOLIO_SYNC_OAUTH_CLIENT_ID
ARG MYBREWFOLIO_SYNC_AUTHORIZE_URL=https://clerk.mybrewfolio.com/oauth/authorize
ARG MYBREWFOLIO_SYNC_TOKEN_URL=https://clerk.mybrewfolio.com/oauth/token
ARG MYBREWFOLIO_SYNC_DEVICE_CALLBACK_URL=https://mybrewfolio.com/v1/sync/device-auth/callback
ENV MYBREWFOLIO_SYNC_API_URL=$MYBREWFOLIO_SYNC_API_URL \
    MYBREWFOLIO_SYNC_OAUTH_CLIENT_ID=$MYBREWFOLIO_SYNC_OAUTH_CLIENT_ID \
    MYBREWFOLIO_SYNC_AUTHORIZE_URL=$MYBREWFOLIO_SYNC_AUTHORIZE_URL \
    MYBREWFOLIO_SYNC_TOKEN_URL=$MYBREWFOLIO_SYNC_TOKEN_URL \
    MYBREWFOLIO_SYNC_DEVICE_CALLBACK_URL=$MYBREWFOLIO_SYNC_DEVICE_CALLBACK_URL
WORKDIR /source
COPY src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/build.rs ./src-tauri/
COPY src-tauri/src ./src-tauri/src
WORKDIR /source/src-tauri

FROM build-source AS build
RUN cargo build --locked --release --no-default-features --features headless --bin mybrewfolio-syncd

FROM build-source AS build-ha
RUN cargo build --locked --release --no-default-features --features headless-web --bin mybrewfolio-syncd

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
  && groupadd --gid 10001 mybrewfolio-sync && useradd --uid 10001 --gid mybrewfolio-sync --home-dir /data --create-home --shell /usr/sbin/nologin mybrewfolio-sync
USER mybrewfolio-sync:mybrewfolio-sync
VOLUME ["/data"]
ENV MYBREWFOLIO_SYNC_DATA_DIR=/data
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD mybrewfolio-syncd health
ENTRYPOINT ["mybrewfolio-syncd"]
CMD ["daemon"]

# A separate image target keeps the normal Docker image and its startup contract unchanged.
FROM runtime AS home-assistant
COPY --from=build-ha /source/src-tauri/target/release/mybrewfolio-syncd /usr/local/bin/mybrewfolio-syncd
USER root
RUN apt-get update && apt-get install -y --no-install-recommends util-linux && rm -rf /var/lib/apt/lists/*
COPY --from=ha-web /source/dist-ha /usr/share/mybrewfolio-sync/web
COPY home-assistant/entrypoint.sh /usr/local/bin/home-assistant-entrypoint
RUN chmod 755 /usr/local/bin/home-assistant-entrypoint
ENV MYBREWFOLIO_SYNC_DATA_DIR=/data/state
LABEL io.hass.type="addon" io.hass.name="MyBrewFolio Sync" io.hass.description="GaggiMate to MyBrewFolio synchronization"
ENTRYPOINT ["/usr/local/bin/home-assistant-entrypoint"]
CMD ["daemon", "--home-assistant"]

FROM runtime AS headless
COPY --from=build /source/src-tauri/target/release/mybrewfolio-syncd /usr/local/bin/mybrewfolio-syncd
