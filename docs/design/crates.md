# 크레이트 구성

이 표가 크레이트 이름·경로·의존 방향의 정본이다. 다른 문서와 작업 계획은 이 표를 따른다. 이름을 바꾸려면 이 표를 먼저 고친다.

## 크레이트 표

| 크레이트 | 경로 | 가안 → 확정 | 맡는 것 | 이 레포 안 의존 |
|---|---|---|---|---|
| `tasty-net-frame` | `crates/tasty-net-frame` | 가안 그대로 | 프레임 코덱(`write_frame`·`read_frame`), `StreamTag`, 프레임 상한, heartbeat 상수, 읽기 실패 분류(`ReadFailure`), mesh 청크 분할·재조립, bulk 청크·mux 바이트 형식 | 없음 |
| `tasty-net-ssh` | `crates/tasty-net-ssh` | 가안 그대로 | 시스템 `ssh` 실행, `-L` 터널, 원격 포트 발견, 셸 감지, 취소, backoff, 리스너 판정, 자식 생성 훅 | 없음 |
| `tasty-net-identity` | `crates/tasty-net-identity` | 가안 그대로 | 기기 키·기기 ID, 서명·검증, 연결 토큰, 권한 걸린 비밀 파일, TOFU 판정 | 없음 |
| `tasty-net-transport` | `crates/tasty-net-transport` | 가안 그대로 | 전송 trait, 논리 채널, handshake v2, 역할, bulk 자격, 요청 채널, heartbeat 송수신, SSH 정방향·역할 반전 전송, 메모리 전송, P2P 어댑터(`tasty-net-p2p`의 스트림 → `RawChannel`, 연결 공유), 세 경로를 모은 연결 목록([전송 추상화](transport.md#연결-목록)). `P2pCloseReason`은 p2p의 것을 다시 내보낸다 | frame, ssh, identity, p2p |
| `tasty-net-p2p` | `crates/tasty-net-p2p` | 다시 정함(2026-11-27, iroh 제거) | P2P 소켓과 패킷 나누기, NAT 판별, 펀칭, quinn 연결과 스트림, 기기 키 인증, 페어링 허용 목록, 상태 기계·연결 정보·설정값 조회, `P2pCloseReason`과 QUIC 종료 코드 표, 소유 런타임과 동기 API | identity, rendezvous |
| `tasty-net-rendezvous` | `crates/tasty-net-rendezvous` | 새로 정함(2026-11-27) | 라이브러리: 조정 메시지 형식, STUN 부분 집합 코덱, 펀칭 프로브 형식, raw public key TLS 설정, 서버 본체(상태·상한·접근 정책). 실행 파일 `tasty-rendezvous` | identity |
| `tasty-net-topology` | `crates/tasty-net-topology` | 가안 그대로 | 노드·엣지 모델, 질의·결과 메시지, 탐색 상태 기계 | frame, identity |
| `tasty-net-demo` | `crates/tasty-net-demo` | 새로 정함 | 예제 바이너리, 전송 계층 두 프로세스 통합 시험(SSH, transport 위 P2P), 시험용 sshd 도우미. 배포하지 않는다(`publish = false`) | transport, p2p, rendezvous, ssh, identity, frame |
| (크레이트 아님) | `rendezvous/`(지금의 `relay/`를 바꾼다) | 다시 정함(2026-11-27) | rendezvous 서버 이미지 빌드 파일, 설정 예시, compose 예시, 설정 검사 스크립트, 자체 운영 안내([rendezvous](rendezvous.md#배포-형태)) | 없음 |

가안과 달라진 자리:

- 예제와 두 프로세스 통합 시험은 `tasty-net-demo` 한 크레이트에 둔다. 통합 시험이 띄우는 바이너리는 `env!("CARGO_BIN_EXE_<이름>")`로 찾는데, 이 값은 같은 패키지의 `[[bin]]`에만 생긴다. 그래서 바이너리와 시험을 한 패키지에 둔다. 시험용 sshd 도우미도 이 크레이트의 `tests/support/`에 둔다. 제품 크레이트는 시험 도구에 의존하지 않는다.
- 펀칭 측정과 P2P 두 프로세스 연결 예제는 `tasty-net-p2p`의 `examples/`(`p2p_peer.rs`, `punch_probe.rs`)에 둔다. P2P 크레이트가 단독으로 시험과 예제를 돌릴 수 있어야 한다는 요구(사용자 결정 2026-11-27) 때문이다. 예제도 `cargo build --example`로 실행 파일 하나가 되므로 다른 망의 기기로 옮기기 쉽다. 크레이트 안의 두 프로세스 시험은 시험 바이너리가 자기 자신을 자식 프로세스로 다시 실행하는 방식으로 띄운다(`CARGO_BIN_EXE_*`는 `[[bin]]`에만 생기므로 예제 경로를 쓰지 않는다). 예제 실행 파일 두 개를 띄우는 확인은 시험이 아니라 검사 단계 `p2p-example-pair`가 한다([CI와 시험 환경](testing.md#크레이트-단독)).

`tasty-net-demo` 바이너리 이름(정본):

| 바이너리 | 하는 일 |
|---|---|
| `demo-server` | handshake v2를 받는 쪽. 정방향이면 workspace 제공 쪽 흉내로 링크 시작 때 Control로 snapshot 흉내를 한 번 보내고 Data를 그대로 돌려준다(echo). 역할 반전 제안은 옵션에 따라 수락·거절하고, 수락한 링크에서는 holder로서 dialer의 descriptor를 받고 입력을 보내 echo를 받는다 |
| `demo-dialer` | SSH 정방향 또는 역할 반전으로 연결한다. 정방향은 입력을 보내 echo를 받고, 역할 반전은 workspace 제공 쪽으로 descriptor를 보낸 뒤 받은 Data를 돌려준다. 끊기면 backoff 뒤 다시 연결 |
| `p2p-link` | transport 위 P2P 링크 하나. `--listen` 또는 `--dial <기기 ID>`로 동작을 고르고 rendezvous 서버 주소·공개키를 받는다. `tests/p2p_attach.rs`가 띄운다 |

## 의존 방향

화살표는 의존하는 크레이트에서 의존 대상으로 간다(위 표의 "이 레포 안 의존"과 같다).

```
transport  ─> frame, ssh, identity, p2p
p2p        ─> rendezvous, identity
rendezvous ─> identity
topology   ─> frame, identity
demo       ─> transport, p2p, rendezvous, ssh, identity, frame
frame, ssh, identity ─> (없음)
```

- 이 그림에 없는 의존은 만들지 않는다. 특히 화살표를 거꾸로 가는 의존(의존 대상이 의존하는 크레이트에 의존)은 순환이라 만들 수 없고, 만들지 않는다. p2p와 rendezvous는 transport에 의존하지 않는다(아래 "P2P 크레이트 격리"). topology는 transport에 의존하지 않는다. 탐색은 "이웃에게 질의를 보내는 함수"를 주입받는 순수 상태 기계라서 전송을 몰라도 된다([토폴로지](topology.md)).
- tasty 레포의 크레이트에는 경로·git 어느 방식으로도 의존하지 않는다. 확인 방법은 [동작 수준 기준](done-criteria.md)의 D7이다. `cargo metadata`에서 path 의존과 workspace 멤버는 둘 다 `source`가 null이므로, 판별은 git 의존의 `source` URL과 path 의존 패키지의 `manifest_path`가 이 workspace 루트 밖인지를 함께 본다.
- feature로 기능을 켜고 끄지 않는다. 소비자는 크레이트 단위로 고른다.
- 의존 비용: transport가 p2p에 의존하므로(사용자 결정 2026-11-27, transport가 p2p를 어댑터로 쓴다) transport를 쓰는 소비자의 의존 그래프에는 quinn·rustls·ring·tokio가 들어온다. SSH 경로만 쓰는 소비자도 같다. 프레임·SSH만 쓰려는 소비자는 `tasty-net-frame`·`tasty-net-ssh`를 직접 쓰면 이 의존이 들어오지 않는다.

## P2P 크레이트 격리

사용자 결정(2026-11-27, 원문 "tasty-network쪽에 이것도 격리된 crate 같은거로 네트워크 라이브러리 만들어서 넣어"): P2P 직접 구현은 workspace 안의 독립 크레이트로 둔다.

- `tasty-net-p2p`(P2P 전송 라이브러리)와 `tasty-net-rendezvous`(서버 라이브러리 + 실행 파일)를 따로 둔다.
- 둘은 tasty 레포에 의존하지 않고, 이 workspace의 `tasty-net-ssh`·`tasty-net-topology`·`tasty-net-transport`·`tasty-net-frame`에도 의존하지 않는다. 이 workspace 안에서 의존하는 것은 공용 크레이트 `tasty-net-identity` 하나와, p2p → rendezvous(같은 P2P 묶음 안)뿐이다.
- 공용 크레이트를 identity 그대로 쓰는 근거:
  1. P2P에 필요한 공용 타입은 기기 키·기기 ID·서명 검증이고, identity가 이미 그 정본이다. transport의 handshake와 topology도 같은 `DeviceId` 타입을 쓰므로, 따로 떼면 같은 값의 타입이 둘이 되거나 identity를 쪼개는 코드 이동이 필요하다.
  2. identity의 외부 의존(serde_json, getrandom, subtle, libc, windows-sys, ed25519 구현)에는 네트워크·런타임이 없다. 연결 토큰과 TOFU 판정은 P2P가 쓰지 않지만 의존을 더하지 않는다.
  3. frame은 쓰지 않는다. P2P 스트림은 바이트 스트림이고 프레임은 transport가 그 위에 얹는다. 조정 메시지의 길이 접두는 rendezvous의 형식 모듈이 따로 정한다(4바이트 길이 + JSON, frame의 tag 구조가 필요 없다).
- rendezvous → p2p 방향이 아니라 p2p → rendezvous인 근거: p2p의 시험과 예제가 서버를 프로세스 안에서 띄워야 하고, 메시지 형식은 한 곳에 있어야 한다. 서버가 p2p에 의존하면 p2p 시험이 서버를 쓰기 위해 순환(dev 의존)이 생긴다.
- transport는 p2p를 어댑터로 쓴다. `P2pCloseReason`과 QUIC 종료 코드 표의 정의는 p2p로 옮기고 transport는 다시 내보낸다(지금 develop의 transport 코드에 있는 정의를 옮기는 일은 후속 작업이다).
- 확인: 정적 검사 `p2p-isolation`(`scripts/check.sh p2p-isolation`, `cargo metadata`에서 두 크레이트의 workspace 안 의존이 위 범위 밖이면 실패)이 [동작 수준 기준](done-criteria.md) D8이다.

## 외부 의존 원칙

- 제품 크레이트의 외부 의존은 필요한 것만 둔다. 예정: `tracing`(모두), `serde`·`serde_json`(identity의 연결 레코드와 그 파서, transport·topology·rendezvous), `ed25519-dalek`(identity, 아래), `quinn`·`rustls`·`tokio`(p2p·rendezvous, 정확한 판 고정, [P2P](p2p.md#외부-의존과-고정)·[버전·호환](versioning.md)), Windows API 바인딩 `windows-sys`(identity의 ACL, transport의 소켓 깨우기, rendezvous의 보내기 오류 번호). transport는 서명 대상 해시 `sha2`와 nonce·bulk 자격용 OS 난수 `getrandom`을 쓴다. topology는 질의의 `request_id`·`branch`용 OS 난수 `getrandom`을 쓴다. identity는 이 밖에 OS 난수 `getrandom`(기기 키 seed와 토큰), 상수 시간 비교 `subtle`, Unix의 `libc`(유효 사용자 ID와 `O_NOFOLLOW`)를 쓴다. rendezvous는 펀칭 토큰에 `getrandom`을, 서버 설정 파일(TOML) 파싱에 `toml`을, 작은 차수 공개키 거절에 `ed25519-dalek`(identity와 같은 판)의 `VerifyingKey::is_weak`를, 경로보다 큰 보내기의 오류 번호(`EMSGSIZE`)에 Unix의 `libc`를 쓴다.
- iroh 계열 의존(`iroh`, `iroh-relay`, `iroh-base`)은 두지 않는다(사용자 결정 2026-11-27, iroh 제거).
- 기기 키의 ed25519 구현은 `ed25519-dalek`을 직접 쓴다. identity는 `ed25519-dalek` 3.0.0을 정확한 판으로 직접 선언하고 기본 feature와 같은 `fast`·`zeroize`만 켠다. 전에 거치던 `iroh-base` 1.5.0(`key` feature)과 같은 판이라 키 바이트·서명·기기 ID 문자열은 바뀌지 않았다. `iroh-base`의 `relay` feature 몫으로 들어오던 `url`·`idna`·`icu_*`는 빠졌다([기기 키·토큰](identity.md)).

## 오류 규칙

- 공개 API의 오류는 크레이트마다 구체 타입(enum 또는 enum을 담은 구조체)이고 `std::error::Error`를 구현한다. 공개 API에 `anyhow`를 쓰지 않는다. 크레이트 안에서 쓰는 것은 된다.
- `Display` 문구는 영어 한 문장으로 고정한다. 번역하지 않는다. 사용자에게 보일 문구는 소비자가 enum 값으로 분기해 자기 i18n 키로 만든다.
- 사용자에게 보일 수 있는 실패는 종류를 enum 값으로 구분한다. 문자열을 비교해 분기할 일이 없게 한다.
- `ReadFailure::of_error`는 `&(dyn std::error::Error + 'static)`를 받아 `source()` 사슬에서 `io::Error`를 찾는다. tasty가 `anyhow::Error`를 그대로 넘길 수 있다(`anyhow::Error`는 `AsRef<dyn Error + Send + Sync>`).

## 동기·비동기 규칙

- 소비자에게 보이는 API는 동기(std 스레드·블로킹 I/O)다. tasty의 attach 호출 경로가 동기 스레드 코드이기 때문이다(`tasty:crates/tasty-ipc/src/client/stream.rs`, `tasty:crates/tasty-remote/src/transport.rs`). tasty가 tokio를 전혀 안 쓰는 것은 아니다(다른 경로에서 쓴다). tokio 쪽 코드에서 부를 때는 블로킹 경계를 거친다([전송 추상화](transport.md) "동기·비동기 경계").
- quinn은 tokio 위에서 돈다. `tasty-net-p2p`가 자기 tokio 런타임을 소유하고 동기 API 뒤로 숨긴다. transport 어댑터는 그 동기 API를 쓴다. 자세한 경계는 [전송 추상화](transport.md) "동기·비동기 경계". `tasty-net-rendezvous`의 서버는 비동기 라이브러리 API(서버를 띄우는 쪽이 런타임을 준다)와, 자기 런타임을 만드는 실행 파일을 둔다.

## 설정값 규칙

사용자 결정(2026-11-27, 원문 "타이머, 재시도횟수 이런것은 전부 설정가능 및 보기 가능값이어야하지"): 모든 타이머·재시도 횟수·간격·타임아웃·keepalive 주기·상한은 설정으로 바꿀 수 있고, 실행 중에 현재 적용값과 출처(`default`·`configured`)를 조회할 수 있어야 한다. 코드 안 상수로만 있는 값은 두지 않는다. 와이어 형식 값(바이트 길이, magic 값, 속성 번호)은 시간·횟수가 아니므로 해당하지 않는다.

| 크레이트 | 적용 |
|---|---|
| `tasty-net-p2p`, `tasty-net-rendezvous` | 처음부터 따른다. 항목 표는 [P2P](p2p.md#설정값), [rendezvous](rendezvous.md#서버-설정값) |
| `tasty-net-transport`, `tasty-net-ssh`, `tasty-net-identity` | develop에 이미 있는 상수(heartbeat 간격·시간 제한, 터널 준비 기한, 출력 상한 등)를 이 규칙에 맞추는 후속 작업으로 둔다. 그때 각 문서에 항목 표를 더한다 |
| `tasty-net-frame`, `tasty-net-topology` | 위와 같다(프레임 상한은 와이어 형식이므로 제외, 탐색 기한·상한은 대상) |

확인: 설정 항목 목록과 조회 결과가 같은지 보는 시험, 코드 기본값을 설계 표와 값까지 비교하는 시험, 설정 모듈 밖의 리터럴을 허용 목록(0·1도 전역 예외가 아니다)과 대조하는 정적 검사([CI와 시험 환경](testing.md#설정값-시험), [동작 수준 기준](done-criteria.md) D10).

## unsafe 규칙

- `tasty-net-frame`·`tasty-net-ssh`·`tasty-net-topology`는 `#![forbid(unsafe_code)]`다.
- OS API를 불러야 하는 크레이트(identity의 Windows ACL, transport의 Windows 소켓 I/O 취소)는 크레이트 루트에 `#![deny(unsafe_code)]`를 두고 OS 호출 모듈에서만 허용한다. 모든 `unsafe` 블록에 `SAFETY:` 주석을 단다.

## 시험의 자리

| 시험 | 자리 |
|---|---|
| 크레이트 단위 시험 | 각 크레이트의 `src/` 안 `mod tests`와 `tests/` |
| 소켓 없이 도는 handshake·heartbeat·취소 시험 | `tasty-net-transport`의 메모리 전송 위 |
| P2P 단위 시험(STUN 코덱, NAT 판별, 프로브 순서, 상태 기계 전이, 설정값 조회) | `tasty-net-p2p`의 `src/` 안 `mod tests` |
| 프로세스 안 서버와 모의 NAT로 도는 펀칭·연결 시험, 크레이트 안 두 프로세스 시험 | `tasty-net-p2p`의 `tests/`. 서버는 `tasty-net-rendezvous` 라이브러리로 띄운다 |
| 서버 시험(등록, 접근 정책, 상한, 만료, 밀어내기, 중계 없음) | `tasty-net-rendezvous`의 `tests/` |
| P2P 어댑터 위 채널 시험(한 스트림 안 순서, 닫기 범위, 첫 스트림 거절 ACK 전달, 양방향 마지막 프레임 전달) | `tasty-net-transport`의 `tests/`(p2p가 정식 의존이다) |
| 실제 `ssh`·`sshd`와 별도 OS 프로세스 두 개를 쓰는 시험 | `crates/tasty-net-demo/tests/ssh_attach.rs`, `ssh_reverse_attach.rs` |
| 별도 OS 프로세스 두 개의 P2P 시험 | `crates/tasty-net-demo/tests/p2p_attach.rs` |
| rendezvous 서버 설정 검사와 STUN 응답 확인 | `rendezvous/check-config.sh`(CI workflow `rendezvous.yml`, 후속 작업이 만든다) |
| 같은 망 안에서 실제 서버 프로세스를 거치는 연결 확인 | `rendezvous/container-check.sh`(원격 빌드 환경, D5 (b)) |
