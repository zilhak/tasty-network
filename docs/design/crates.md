# 크레이트 구성

이 표가 크레이트 이름·경로·의존 방향의 정본이다. 다른 문서와 작업 계획은 이 표를 따른다. 이름을 바꾸려면 이 표를 먼저 고친다.

## 크레이트 표

| 크레이트 | 경로 | 가안 → 확정 | 맡는 것 | 이 레포 안 의존 |
|---|---|---|---|---|
| `tasty-net-frame` | `crates/tasty-net-frame` | 가안 그대로 | 프레임 코덱(`write_frame`·`read_frame`), `StreamTag`, 프레임 상한, heartbeat 상수, 읽기 실패 분류(`ReadFailure`), mesh 청크 분할·재조립, bulk 청크·mux 바이트 형식 | 없음 |
| `tasty-net-ssh` | `crates/tasty-net-ssh` | 가안 그대로 | 시스템 `ssh` 실행, `-L` 터널, 원격 포트 발견, 셸 감지, 취소, backoff, 리스너 판정, 자식 생성 훅 | 없음 |
| `tasty-net-identity` | `crates/tasty-net-identity` | 가안 그대로 | 기기 키·기기 ID, 서명·검증, 연결 토큰, 권한 걸린 비밀 파일, TOFU 판정 | 없음 |
| `tasty-net-transport` | `crates/tasty-net-transport` | 가안 그대로 | 전송 trait, 논리 채널, handshake v2, 역할, bulk 자격, 요청 채널, heartbeat 송수신, SSH 정방향·역할 반전 전송, 메모리 전송, P2P 종료 사유 enum `P2pCloseReason`(p2p가 쓰지만 정의는 여기) | frame, ssh, identity |
| `tasty-net-p2p` | `crates/tasty-net-p2p` | 가안 그대로 | iroh endpoint, QUIC 스트림 ↔ 논리 채널, 경로 감시와 직접 경로 정책, 페어링 | frame, identity, transport |
| `tasty-net-topology` | `crates/tasty-net-topology` | 가안 그대로 | 노드·엣지 모델, 질의·결과 메시지, 탐색 상태 기계 | frame, identity |
| `tasty-net-demo` | `crates/tasty-net-demo` | 새로 정함 | 예제 바이너리, 두 프로세스 통합 시험, 시험용 sshd 도우미. 배포하지 않는다(`publish = false`) | transport, p2p, ssh, identity, frame |
| (크레이트 아님) | `relay/` | 가안 그대로 | iroh-relay 설정 예시, compose 예시, 설정 검사 스크립트, 자체 운영 안내 | 없음 |

가안과 달라진 자리:

- 예제와 두 프로세스 통합 시험은 `tasty-net-demo` 한 크레이트에 둔다. 통합 시험이 띄우는 바이너리는 `env!("CARGO_BIN_EXE_<이름>")`로 찾는데, 이 값은 같은 패키지의 `[[bin]]`에만 생긴다. 그래서 바이너리와 시험을 한 패키지에 둔다. 시험용 sshd 도우미도 이 크레이트의 `tests/support/`에 둔다. 제품 크레이트는 시험 도구에 의존하지 않는다.
- 펀칭 측정 예제는 `crates/tasty-net-p2p/examples/punch_probe.rs`가 아니라 `tasty-net-demo`의 바이너리 `punch-probe`로 둔다. 사람이 다른 네트워크의 기기에 하나의 실행 파일로 옮겨 돌리기 쉽다.

`tasty-net-demo` 바이너리 이름(정본):

| 바이너리 | 하는 일 |
|---|---|
| `demo-server` | workspace 제공 쪽 흉내. handshake v2를 받고 Data를 그대로 돌려준다(echo). 시작 때 Control로 snapshot 흉내 한 번 |
| `demo-dialer` | SSH 정방향 또는 역할 반전으로 연결하고 입력을 보내 echo를 받는다. 끊기면 backoff 뒤 다시 연결 |
| `p2p-peer` | P2P endpoint 하나. `--listen` 또는 `--dial <주소>`로 동작을 고른다 |
| `punch-probe` | 펀칭 측정. 한 줄에 사건 하나를 고정 형식으로 기록한다([P2P](p2p.md) "펀칭 실험") |

## 의존 방향

```
frame ─┐
ssh ───┼─> transport ─> p2p
identity ┘      │
frame, identity ─> topology
demo ─> (transport, p2p, ssh, identity, frame)
```

- 화살표 반대 방향 의존은 만들지 않는다. topology는 transport에 의존하지 않는다. 탐색은 "이웃에게 질의를 보내는 함수"를 주입받는 순수 상태 기계라서 전송을 몰라도 된다([토폴로지](topology.md)).
- tasty 레포의 크레이트에는 경로·git 어느 방식으로도 의존하지 않는다. 확인 방법은 [동작 수준 기준](done-criteria.md)의 D7이다. `cargo metadata`에서 path 의존과 workspace 멤버는 둘 다 `source`가 null이므로, 판별은 git 의존의 `source` URL과 path 의존 패키지의 `manifest_path`가 이 workspace 루트 밖인지를 함께 본다.
- feature로 기능을 켜고 끄지 않는다. 소비자는 크레이트 단위로 고른다. P2P가 필요 없는 소비자는 `tasty-net-p2p`를 의존에 넣지 않으면 iroh·tokio가 들어오지 않는다.

## 외부 의존 원칙

- 제품 크레이트의 외부 의존은 필요한 것만 둔다. 예정: `tracing`(모두), `serde`·`serde_json`(identity의 연결 레코드와 그 파서, transport·topology·p2p), `iroh-base` 키 기능(identity, 아래), `iroh`(p2p, `=1.5.0` 고정, [버전·호환](versioning.md)), `tokio`(p2p), Windows API 바인딩 `windows-sys`(identity의 ACL, transport의 소켓 깨우기).
- 기기 키의 ed25519 구현은 `iroh-base` 1.5.0의 `SecretKey`·`PublicKey`를 쓴다. 의존 선언은 `iroh-base = { version = "=1.5.0", default-features = false, features = ["key"] }`다. `key` feature는 `relay` feature도 켜지만(`url` 등) manifest에 tokio가 없다(`iroh-base` 1.5.0 `Cargo.toml`). 같은 트리에 ed25519 구현이 두 판 들어가지 않고, 기기 ID 문자열이 iroh endpoint ID 표시와 저절로 같아진다. 구현 때 `cargo tree -p tasty-net-identity`로 무거운 의존이 없는지 다시 보고, 있으면 같은 판의 `ed25519-dalek`을 직접 쓰고 표시 형식을 맞춘다.

## 오류 규칙

- 공개 API의 오류는 크레이트마다 구체 타입(enum 또는 enum을 담은 구조체)이고 `std::error::Error`를 구현한다. 공개 API에 `anyhow`를 쓰지 않는다. 크레이트 안에서 쓰는 것은 된다.
- `Display` 문구는 영어 한 문장으로 고정한다. 번역하지 않는다. 사용자에게 보일 문구는 소비자가 enum 값으로 분기해 자기 i18n 키로 만든다.
- 사용자에게 보일 수 있는 실패는 종류를 enum 값으로 구분한다. 문자열을 비교해 분기할 일이 없게 한다.
- `ReadFailure::of_error`는 `&(dyn std::error::Error + 'static)`를 받아 `source()` 사슬에서 `io::Error`를 찾는다. tasty가 `anyhow::Error`를 그대로 넘길 수 있다(`anyhow::Error`는 `AsRef<dyn Error + Send + Sync>`).

## 동기·비동기 규칙

- 소비자에게 보이는 API는 동기(std 스레드·블로킹 I/O)다. tasty의 attach 호출 경로가 동기 스레드 코드이기 때문이다(`tasty:crates/tasty-ipc/src/client/stream.rs`, `tasty:crates/tasty-remote/src/transport.rs`). tasty가 tokio를 전혀 안 쓰는 것은 아니다(다른 경로에서 쓴다). tokio 쪽 코드에서 부를 때는 블로킹 경계를 거친다([전송 추상화](transport.md) "동기·비동기 경계").
- iroh는 tokio 위에서 돈다. `tasty-net-p2p`가 자기 tokio 런타임을 소유하고 동기 trait 뒤로 숨긴다. 자세한 경계는 [전송 추상화](transport.md) "동기·비동기 경계".

## unsafe 규칙

- `tasty-net-frame`·`tasty-net-ssh`·`tasty-net-topology`는 `#![forbid(unsafe_code)]`다.
- OS API를 불러야 하는 크레이트(identity의 Windows ACL, transport의 Windows 소켓 I/O 취소)는 크레이트 루트에 `#![deny(unsafe_code)]`를 두고 OS 호출 모듈에서만 허용한다. 모든 `unsafe` 블록에 `SAFETY:` 주석을 단다.

## 시험의 자리

| 시험 | 자리 |
|---|---|
| 크레이트 단위 시험 | 각 크레이트의 `src/` 안 `mod tests`와 `tests/` |
| 소켓 없이 도는 handshake·heartbeat·취소 시험 | `tasty-net-transport`의 메모리 전송 위 |
| 실제 iroh endpoint로 도는 P2P 채널 시험(한 스트림 안 순서, 닫기 범위, 첫 스트림 거절 ACK 전달, 양방향 마지막 프레임 전달) | `tasty-net-p2p`의 `tests/`. transport는 p2p를 dev-dependency로도 두지 않는다 |
| 실제 `ssh`·`sshd`와 별도 OS 프로세스 두 개를 쓰는 시험 | `crates/tasty-net-demo/tests/ssh_attach.rs`, `ssh_reverse_attach.rs` |
| 별도 OS 프로세스 두 개의 P2P 시험 | `crates/tasty-net-demo/tests/p2p_attach.rs` |
| relay 설정 기동 검사 | `relay/check-config.sh`(CI workflow `relay.yml`) |
| 실제 relay 컨테이너를 거치는 연결 확인 | `relay/container-check.sh`(원격 빌드 환경, D5 (b)) |
