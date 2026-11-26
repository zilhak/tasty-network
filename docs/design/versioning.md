# 버전·호환 규칙

## 크레이트 버전

- 모든 크레이트가 workspace 버전 하나(`[workspace.package] version`)를 같이 쓴다. 크레이트끼리 서로의 판을 맞출 일이 없어지고, tasty는 태그 하나로 모든 크레이트를 고정한다.
- semver를 따른다. 1.0 전에는 공개 API를 깨는 변경이면 minor를, 그 밖은 patch를 올린다. 1.0부터는 표준 semver다.
- crates.io에 올리지 않는다. 모든 크레이트에 `publish = false`를 둔다. 올리기로 하면 이 문서를 먼저 고친다.

## 태그와 브랜치

- 브랜치는 git-flow(`develop`·`release`·`main`)이고 tasty의 `docs/dev-guide/branching.md`와 같다. 이 레포의 차이는 레포 기반 작업이 `docs/dev-guide/branching.md`에 적는다.
- 태그 형식은 `v<semver>`(예: `v0.1.0`)이고 `main`의 버전 커밋에만 붙인다.

## tasty가 쓰는 방식

- git 의존 + `rev` 고정: `tasty-net-transport = { git = "https://github.com/zilhak/tasty-network", rev = "<태그가 가리키는 커밋 sha>" }`. 태그 이름은 옮겨질 수 있으므로 `rev`로 고정하고 옆 주석에 태그 이름을 적는다.
- 로컬 개발은 Cargo의 `[patch."https://github.com/zilhak/tasty-network"]`로 이 레포 checkout 경로를 연결한다. 이 패치는 커밋하지 않는다(개인 `.cargo/config.toml` 등에 둔다).
- tasty가 여러 크레이트를 쓰면 모두 같은 `rev`를 쓴다.

## 와이어 버전과 크레이트 버전

- 와이어 버전은 handshake의 `proto`다. 지금 2([handshake v2](handshake.md)). 크레이트 버전과 따로 움직인다.
- `proto`는 handshake나 프레임 형식이 호환되지 않게 바뀔 때만 올린다. 기능 추가는 `caps`로 협상한다.
- `proto`를 올리는 릴리스는 크레이트 minor(1.0 뒤에는 major)도 올린다.
- 토폴로지 메시지의 형식 변경은 새 capability 이름(예: `net.topology.v2`)으로 낸다.
- rendezvous 조정 프로토콜은 따로 번호를 가진다. `hello.proto`(지금 1)와 서버 ALPN `tasty-rendezvous/1`이 같이 움직인다. 서버가 모르는 `proto`는 종료 코드 `unsupported_proto`로 닫는다([rendezvous](rendezvous.md#메시지)). 마이그레이션 미지원 정책(사용자 결정 2026-11-27)대로 서버는 한 번에 한 판만 받는다. 판을 올릴 때는 서버와 기기를 함께 올린다.
- 펀칭 프로브의 STUN 속성(`TASTY-PUNCH`)과 기기 사이 ALPN `tasty-net/2`는 위 두 번호에 묶인다. 프로브 형식을 바꾸면 조정 프로토콜 번호를 올린다.

## 구버전 peer와의 호환 범위

| 상대 | 결과 |
|---|---|
| 같은 `proto`(2)의 다른 크레이트 버전 | 연결된다. 모르는 `caps`는 무시한다 |
| `proto` 1(지금 tasty), SSH | 연결되지 않는다. 받는 쪽이 v2면 거절 ACK `legacy_protocol`, dialer가 v2면 로컬 오류 `PeerTooOld`([handshake v2](handshake.md) "구버전 처리") |
| `proto` 3 이상, SSH | 연결되지 않는다. 거절 ACK `unsupported_protocol`(ACK에 자기 `proto`를 싣는다) |
| `proto`가 다른 P2P | ALPN이 달라 기기 사이 QUIC/TLS 협상에서 실패한다. 앱 스트림이 열리지 않으므로 ACK도 사유도 없다. dialer는 로컬 오류 `P2pConnectError::AlpnMismatch`로 받고 다시 걸지 않는다. 받는 쪽은 소비자에게 알리지 않는다(로그만). 소비자는 이 오류를 "상대와 프로토콜 판이 다르다, 양쪽을 업데이트하라"는 안내로 바꾼다. 지금 P2P는 tasty에 없으므로 이 경우는 `proto` 3 이후에만 생긴다 |

## 의존 고정

- P2P의 QUIC ALPN(`tasty-net/2`)은 `proto`와 같은 숫자를 쓴다. `proto`를 올리면 ALPN도 바꾼다. ALPN이 다르면 와이어 버전을 읽을 길이 없으므로 P2P 쪽 호환 판정은 위 표의 ALPN 행이다.
- Rust 최소 판: workspace의 `rust-version`은 의존 그래프 전체의 최솟값보다 낮게 적지 않는다. 지금 값 1.91은 iroh 1.5.0의 선언에서 왔다. iroh 계열을 빼면 quinn 0.11.12(`rust-version = "1.85"`)·rustls 0.23.46(1.71)·`ed25519-dalek` 3.0.0(1.85)의 선언으로 다시 잰다. 그래서 workspace 값은 1.85 이상이어야 한다(quinn 문서의 "최소 1.74.1"은 옛 판의 값이라 쓰지 않는다). 낮출 수 있어도 고정 툴체인(`rust-toolchain.toml`)을 바꾸는지는 레포 기반 작업이 정한다.
- iroh 계열 의존은 두지 않는다(사용자 결정 2026-11-27). 앞선 설계의 iroh `=1.5.0` 고정과 relay 이미지 `n0computer/iroh-relay:v1.5.0` 고정은 없앤다. 지금 develop의 `relay/`가 그 이미지를 digest와 함께 고정하고 있고, 그 패키지를 없애는 일은 후속 작업이다([rendezvous](rendezvous.md#relay-패키지와-relayyml의-처리-방침)).
- `quinn`·`rustls`는 `=`로 정확히 고정한다. 둘 다 1.0 전 판(0.x)이라 minor 안에서도 API·동작이 바뀔 수 있고, raw public key처럼 쓰는 곳이 적은 경로에 기대기 때문이다. 올릴 때는 [P2P](p2p.md)의 API 서술(raw public key 설정, grease 설정, 경로 이동, keepalive)을 새 판으로 다시 확인하고 같은 커밋에서 고친다. `tokio`는 semver 범위로 둔다(`Cargo.lock`이 고정한다).
- rendezvous 서버 이미지는 이 레포가 빌드한다. 서버와 기기 크레이트는 같은 workspace 버전이다. 외부 서버 이미지를 고정할 일은 없다.
- `Cargo.lock`을 커밋하고 검사는 `--locked`로 한다.

## CHANGELOG

- 레포 루트에 `CHANGELOG.md`를 둔다. 태그마다 절 하나, 그 안에 "Wire"(와이어·`proto`·`caps` 변화)와 "API"(공개 API 변화)를 나눈다. tasty가 `rev`를 올릴 때 이 두 칸만 보면 된다.
- 첫 릴리스 태그를 준비하는 작업이 파일을 만든다.
