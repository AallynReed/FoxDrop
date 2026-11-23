FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked

FROM scratch
COPY --from=build /src/target/release/foxdrop /foxdrop
ENV FOXDROP_FILES=/files FOXDROP_DATA=/data FOXDROP_PUBLIC_ADDR=0.0.0.0:8080 FOXDROP_ADMIN_ADDR=0.0.0.0:8081
EXPOSE 8080 8081
ENTRYPOINT ["/foxdrop"]
