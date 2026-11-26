# 기기 키·기기 ID와 연결 토큰

`tasty:<경로>`는 tasty 레포 698aa49e03 기준이다. 크레이트는 `tasty-net-identity`다.

## 두 비밀의 수명

| 비밀 | 수명 | 공유 범위 | 쓰는 곳 |
|---|---|---|---|
| 기기 키(ed25519 비밀키) | 영속. 디렉터리에 한 번 만들고 계속 쓴다 | 같은 디렉터리를 쓰는 모든 인스턴스 | handshake 서명, P2P endpoint 키, 토폴로지 노드 ID |
| 연결 토큰 | 인스턴스 하나. 시작 때 발급하고 인스턴스가 끝나면 소비자가 지운다 | 그 인스턴스 하나 | SSH 경로 handshake의 `token` |

API를 나눈다: `DeviceKey::load_or_create(dir)`와 `ConnectionToken::issue(path)`. 같은 디렉터리로 토큰을 두 번 발급하면 값이 다르다. 기기 키는 같다.

## 저장 위치

- 이 레포는 위치를 정하지 않는다. 소비자가 디렉터리(기기 키)와 파일 경로(토큰)를 준다. 이 레포는 `tasty_home`·`TASTY_HOME`을 모른다.
- debug와 release 인스턴스가 다른 디렉터리를 주면 기기 ID가 다르게 된다. 어느 디렉터리를 줄지는 tasty가 정한다.

## 파일 형식

| 파일 | 내용 |
|---|---|
| 기기 키 `device_key` | 첫 줄 `tasty-net-device-key-v1`, 둘째 줄 32바이트 ed25519 seed의 소문자 hex 64자. 끝 줄바꿈은 있어도 된다 |
| 토큰 파일(경로는 소비자가 줌) | 32바이트 OS 난수의 소문자 hex 64자 한 줄. tasty `SessionToken::generate`와 같은 표현(`tasty:crates/tasty-ipc/src/caller.rs:13-33`) |

- 형식이 맞지 않는 파일은 손상으로 본다. `load_or_create`는 손상 파일을 만나면 `IdentityError::Corrupt`를 돌려주고 파일을 그대로 둔다. 새 키로 덮지 않는다. 덮으면 모든 상대가 이 기기를 다른 기기로 본다.
- 키의 정본 표현은 32바이트 seed다. `iroh-base` 1.5.0의 `SecretKey::from_bytes(&[u8; 32])`로 그대로 iroh endpoint에 넘긴다(P2P와 SSH에서 같은 기기 ID).

## 기기 ID 문자열

기기 ID는 ed25519 공개키다. 문자열 표현은 `iroh-base` 1.5.0 `PublicKey`의 `Display`와 같게 한다. 같은 키가 토폴로지·relay 로그·iroh 로그에서 같은 문자열로 보인다. 구현 때 v1.5.0 소스로 형식(길이, 문자 집합)을 확인해 이 절에 적는다. `FromStr`은 같은 형식만 받는다.

## 파일 권한

| OS | 계약 |
|---|---|
| Unix | 디렉터리 0700, 파일 0600. 비밀을 쓰기 전에 권한을 건다. 디렉터리가 이미 있고 더 넓은 권한이면 좁힌다 |
| Windows | 비밀을 쓰기 전에 빈 파일에 소유자 = 현재 사용자 SID, 상속 차단, 현재 SID FullControl 항목 하나만 있는 DACL을 건다. 다시 읽어 그대로인지 확인한다. 설정이나 확인이 실패하면 내용을 쓰지 않고 오류를 돌려준다 |

- Windows 계약은 tasty의 `tasty:crates/tasty-remote-profiles/src/secret_file.rs`와 같다. 구현은 PowerShell 대신 `windows-sys`의 보안 API로 한다. 외부 프로세스를 띄우지 않으므로 PowerShell 실행 정책·경로에 기대지 않고, 같은 프로세스 안에서 시험으로 다시 읽어 확인할 수 있다.
- 소비자가 임의 디렉터리를 줄 수 있으므로 "사용자 프로필 아래라서 안전하다"는 가정을 하지 않는다.

## 동시 첫 생성

같은 디렉터리에서 두 프로세스(또는 스레드)가 동시에 처음 키를 만들어도 모두 같은 키를 읽어야 한다.

1. 권한을 건 임시 파일(같은 디렉터리, 고유 이름)에 키를 쓰고 `sync_all`한다.
2. `std::fs::hard_link(임시, device_key)`로 게시한다. 대상이 있으면 실패하므로 덮어쓰지 않는다. `rename`은 대상이 있으면 바꿔 버릴 수 있어 쓰지 않는다.
3. `AlreadyExists`면 임시 파일을 지우고 이미 있는 파일을 읽는다.
4. 파일 시스템이 하드 링크를 지원하지 않으면 오류를 돌려준다(조용히 다른 방식으로 바꾸지 않는다).

## 서명

- `DeviceKey::sign(&[u8]) -> Signature`, `DeviceId::verify(&[u8], &Signature) -> bool`.
- handshake는 서명 대상의 SHA-256에 서명한다([handshake v2](handshake.md) "흐름 (SSH 경로)").

## 토큰 비교

- 받은 토큰과 자기 토큰은 상수 시간으로 비교한다. 길이가 다르면 바로 false, 같으면 바이트 XOR을 모두 누적해 비교한다(또는 `subtle`).
- 토큰은 로그에 쓰지 않는다. `Debug`·`Display`는 값을 가린다.

## 토큰 전달

토큰은 SSH로만 읽을 수 있는 경로로 상대에게 간다. 이 레포의 포트 발견(`tasty-net-ssh`)이 원격의 포트를 읽을 때 같은 방식으로 토큰도 읽는다(subcommand 출력 또는 권한 걸린 파일). 파일 이름·명령 이름은 `PortDiscoveryConfig`로 받는다([레포 경계](repo-boundary.md) "포트 발견의 tasty 관례"). 원격 쪽이 토큰을 그 파일에 쓰는 일은 tasty 몫이다.

## TOFU

처음 본 상대 기기 ID를 기록하고, 다음부터 다른 키면 멈춘다.

- 판정 로직은 이 레포, 기록 저장소는 소비자가 trait로 준다: `PeerKeyStore { fn get(&self, peer: &PeerRef) -> Result<Option<DeviceId>>; fn put(&self, peer: &PeerRef, id: &DeviceId) -> Result<()> }`. `PeerRef`는 소비자가 정하는 상대 이름(tasty는 원격 프로필 이름)이다.
- 결과: `TofuVerdict::{New, Match, Mismatch { expected, got }}`. `New`면 기록하고 통과, `Match`면 통과, `Mismatch`면 기록을 바꾸지 않고 handshake를 `peer_key_mismatch`로 끝낸다.
- 사용자가 명시적으로 다시 등록하는 경로: `reregister(store, peer, id)`가 기록을 바꾼다. 경고 화면과 확인은 tasty 몫이다.
- handshake는 TOFU 판정을 도메인 콜백(attach 승인) 전에 부른다([handshake v2](handshake.md) "검사 순서").
