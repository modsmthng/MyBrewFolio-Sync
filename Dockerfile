# syntax=docker/dockerfile:1
FROM rust:1.86-bookworm AS build
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
RUN cargo build --locked --release --no-default-features --features headless --bin mybrewfolio-syncd

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
  && groupadd --gid 10001 mybrewfolio-sync && useradd --uid 10001 --gid mybrewfolio-sync --home-dir /data --create-home --shell /usr/sbin/nologin mybrewfolio-sync
COPY --from=build /source/src-tauri/target/release/mybrewfolio-syncd /usr/local/bin/mybrewfolio-syncd
USER mybrewfolio-sync:mybrewfolio-sync
VOLUME ["/data"]
ENV MYBREWFOLIO_SYNC_DATA_DIR=/data
ENTRYPOINT ["mybrewfolio-syncd"]
CMD ["daemon"]
