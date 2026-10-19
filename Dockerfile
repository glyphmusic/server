FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY . .
# The shared wire protocol is included in this checkout.
RUN cargo build --release -p glyph-server-native

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* && useradd -r -u 10001 glyph
COPY --from=build /src/target/release/glyph-server /usr/local/bin/glyph
RUN ln -s glyph /usr/local/bin/glyph-server && mkdir -p /var/lib/glyph && chown glyph:glyph /var/lib/glyph
WORKDIR /var/lib/glyph
USER glyph
EXPOSE 8787
ENV GLYPH_BIND=0.0.0.0:8787
ENTRYPOINT ["glyph"]
