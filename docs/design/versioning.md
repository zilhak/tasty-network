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

## 구버전 peer와의 호환 범위

| 상대 | 결과 |
|---|---|
| 같은 `proto`(2)의 다른 크레이트 버전 | 연결된다. 모르는 `caps`는 무시한다 |
| `proto` 1(지금 tasty) | 연결되지 않는다. 사유 `legacy_protocol` / `PeerTooOld` |
| `proto` 3 이상 | 연결되지 않는다. 사유 `unsupported_protocol` |

## 의존 고정

- P2P의 QUIC ALPN(`tasty-net/2`)은 `proto`와 같은 숫자를 쓴다. `proto`를 올리면 ALPN도 바꾼다.
- Rust 최소 판: iroh·iroh-relay·iroh-base 1.5.0은 `rust-version = "1.91"`을 선언한다. workspace의 `rust-version`은 의존 그래프 전체의 최솟값보다 낮게 적지 않는다. 고정 툴체인(`rust-toolchain.toml`)은 레포 기반 작업이 정한다.
- iroh는 `=1.5.0`처럼 정확히 고정한다. relay 이미지(`n0computer/iroh-relay:v1.5.0`)와 클라이언트 판을 함께 옮긴다. 올릴 때는 [P2P](p2p.md)의 API 표와 [relay](relay.md)의 사실을 새 판 소스로 다시 확인하고 같은 커밋에서 고친다.
- `Cargo.lock`을 커밋하고 검사는 `--locked`로 한다.

## CHANGELOG

- 레포 루트에 `CHANGELOG.md`를 둔다. 태그마다 절 하나, 그 안에 "Wire"(와이어·`proto`·`caps` 변화)와 "API"(공개 API 변화)를 나눈다. tasty가 `rev`를 올릴 때 이 두 칸만 보면 된다.
- 첫 릴리스 태그를 준비하는 작업이 파일을 만든다.
