# tasty-rendezvous 서버 이미지. 안내: rendezvous/README.md.
#
# 빌드 맥락은 레포 루트다(이 레포의 workspace 소스로 빌드한다. 외부 서버 이미지는 쓰지 않는다).
# 배포할 판은 그 태그를 checkout한 트리에서 빌드한다.
#
#   docker build -f rendezvous/Dockerfile -t tasty-rendezvous .
#
# 베이스 이미지는 태그가 옮겨질 수 있어 digest(multi-arch index)를 함께 고정한다.
# 빌드 이미지의 Rust 판은 rust-toolchain.toml의 채널과 같게 둔다. 채널을 올리면 이 줄도 바꾼다.
# 두 이미지는 같은 Debian 판(trixie)이라 실행 단계의 glibc가 빌드 단계와 맞는다.

FROM rust:1.96.0-slim-trixie@sha256:c37af730be4fd8104cbf9aedbd6ab259e51ca2d5437817a0f8680edf66ac6c28 AS build
# rust-toolchain.toml과 다른 툴체인을 몰래 받지 않는다. 판이 다르면 빌드가 멈춘다.
ENV RUSTUP_AUTO_INSTALL=0
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --locked -p tasty-net-rendezvous --bin tasty-rendezvous

FROM debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0
# 비root 사용자. 서버 키 폴더(/var/lib/tasty-rendezvous)는 이 사용자만 읽고 쓴다(0700).
# 이름 있는 볼륨을 이 경로에 처음 붙이면 Docker가 이 소유자와 권한을 볼륨에 옮긴다.
RUN groupadd --system --gid 10001 rendezvous \
    && useradd --system --uid 10001 --gid rendezvous --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin rendezvous \
    && install -d -o rendezvous -g rendezvous -m 0700 /var/lib/tasty-rendezvous \
    && install -d -m 0755 /etc/tasty-rendezvous
COPY --from=build /src/target/release/tasty-rendezvous /usr/local/bin/tasty-rendezvous
USER rendezvous:rendezvous
# 서버는 SIGINT(Ctrl-C)를 받아 연결을 닫고 끝난다. docker stop도 같은 신호를 보낸다.
STOPSIGNAL SIGINT
# 주 포트(QUIC 조정 연결과 STUN)와 보조 포트(STUN). 설정의 listen.*와 같게 둔다.
EXPOSE 3478/udp 3479/udp
ENTRYPOINT ["/usr/local/bin/tasty-rendezvous"]
# 설정 파일은 이미지에 넣지 않는다. 자기 공인 IP를 적은 파일을 이 경로에 붙인다.
CMD ["--config", "/etc/tasty-rendezvous/rendezvous.toml"]
