# Build Stage
FROM ubuntu:20.04 as builder

## Install build dependencies.
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y cmake clang curl
RUN curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
RUN ${HOME}/.cargo/bin/rustup default nightly
RUN ${HOME}/.cargo/bin/cargo install -f cargo-fuzz

## Add source code to the build stage.
ADD . /milli
WORKDIR /milli
RUN ${HOME}/.cargo/bin/cargo build
RUN cd flatten-serde-json && ${HOME}/.cargo/bin/cargo build && ${HOME}/.cargo/bin/cargo fuzz build --fuzz-dir ./fuzz
RUN cd json-depth-checker && ${HOME}/.cargo/bin/cargo build && ${HOME}/.cargo/bin/cargo fuzz build --fuzz-dir ./fuzz
RUN cd milli && ${HOME}/.cargo/bin/cargo build && ${HOME}/.cargo/bin/cargo fuzz build --fuzz-dir ./fuzz

# Package Stage
FROM ubuntu:20.04

COPY --from=builder milli/flatten-serde-json/fuzz/target/x86_64-unknown-linux-gnu/release/* /
COPY --from=builder milli/json-depth-checker/fuzz/target/x86_64-unknown-linux-gnu/release/* /
COPY --from=builder milli/milli/fuzz/target/x86_64-unknown-linux-gnu/release/* /