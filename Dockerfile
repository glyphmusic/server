FROM rust:1-slim AS build
WORKDIR /src
COPY . .
# glyph-protocol is a path dependency; for Docker builds switch it to the git tag
# in the workspace Cargo.toml (see comment there), or build with the app repo
# checked out next to this one and use a larger build context.
RUN cargo build --release -p glyph-server-native

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* && useradd -r -u 10001 glyph
COPY --from=build /src/target/release/glyph-server /usr/local/bin/glyph-server
USER glyph
EXPOSE 8787
ENV GLYPH_BIND=0.0.0.0:8787
ENTRYPOINT ["glyph-server"]
