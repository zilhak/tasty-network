# 기기 키·기기 ID와 연결 토큰

`tasty:<경로>`는 tasty 레포 698aa49e03 기준이다. 크레이트는 `tasty-net-identity`다.

## 두 비밀의 수명

| 비밀 | 수명 | 공유 범위 | 쓰는 곳 |
|---|---|---|---|
| 기기 키(ed25519 비밀키) | 영속. 디렉터리에 한 번 만들고 계속 쓴다 | 같은 디렉터리를 쓰는 모든 인스턴스. P2P endpoint는 그중 하나만 연다([P2P](p2p.md) "endpoint와 런타임") | handshake 서명, P2P endpoint 키, 토폴로지 노드 ID |
| 연결 토큰 | 인스턴스 하나. 시작 때 발급하고, 인스턴스가 끝나면 소비자가 게시 핸들로 자기 레코드만 정리한다(아래 "파일 형식") | 그 인스턴스 하나 | SSH 경로 handshake의 `token` |

API를 나눈다: `DeviceKey::load_or_create(dir) -> Result<DeviceKey, IdentityError>`와 `ConnectionToken::issue() -> Result<ConnectionToken, RandomError>`(값만 만들고 게시는 연결 레코드로 한다). 같은 디렉터리로 토큰을 두 번 발급하면 값이 다르다. 기기 키는 같다. OS 난수 원천이 실패하면 패닉하지 않고 `RandomError`(키 생성 중이면 `IdentityError::Random`)를 돌려준다.

## 저장 위치

- 이 레포는 위치를 정하지 않는다. 소비자가 디렉터리(기기 키)와 파일 경로(토큰)를 준다. 이 레포는 `tasty_home`·`TASTY_HOME`을 모른다.
- debug와 release 인스턴스가 다른 디렉터리를 주면 기기 ID가 다르게 된다. 어느 디렉터리를 줄지는 tasty가 정한다.

## 파일 형식

| 파일 | 내용 |
|---|---|
| 기기 키 `device_key` | 첫 줄 `tasty-net-device-key-v1`, 둘째 줄 32바이트 ed25519 seed의 소문자 hex 64자. 끝 줄바꿈은 있어도 된다 |
| 연결 레코드(경로는 소비자가 줌) | JSON 한 줄 `{"port": <u16>, "token": "<hex 64자>", "instance_id": "<문자열>"}`. 토큰은 32바이트 OS 난수의 소문자 hex 64자로, tasty `SessionToken::generate`와 같은 표현이다(`tasty:crates/tasty-ipc/src/caller.rs:13-33`) |

토큰을 포트와 따로 게시하지 않고 인스턴스 하나의 연결 레코드로 한 번에 게시한다. 포트와 토큰을 따로 읽으면 그 사이 인스턴스가 다시 시작했을 때 서로 다른 세대의 값이 묶일 수 있기 때문이다(포트 파일은 인스턴스가 게시·회수하는 수명 객체다, `tasty:crates/tasty-ipc/src/port_file.rs`). 게시는 `ConnectionRecord::publish(path)`가 권한 걸린 임시 파일 → 교체로 하고 게시 핸들 `RecordPublication`을 돌려준다. 핸들은 게시한 경로, 실제 대상, 파일 정체성(Unix 장치·inode, Windows 볼륨·파일 ID), 내용을 기억한다. 정리는 `RecordPublication::retire()` 하나뿐이다. 같은 경로의 게시와 정리가 함께 잡는 잠금 아래에서 대상의 정체성과 내용이 자기 게시분과 같을 때만 지우고, 다르면(새 인스턴스가 이미 교체함) 그대로 둔다. tasty `PortFilePublication`과 같은 방식이다(`tasty:crates/tasty-ipc/src/port_file.rs`). 소비자는 인스턴스가 끝날 때 핸들로 정리를 부른다. 경로를 받아 지우는 함수는 두지 않는다. 위 표의 "연결 토큰" 행도 이 레코드 안의 토큰을 뜻한다.

- 연결 레코드 경로의 마지막 이름이 심볼릭 링크이거나 일반 파일이 아니면 게시하지 않고 `IdentityError::UnsafeFile`을 돌려준다(위 "파일 권한"의 읽기 검사가 링크를 거절하므로 링크를 따라가 게시하면 그 레코드를 다시 읽을 수 없다). 핸들이 기억하는 "실제 대상"은 정규화한 부모 디렉터리에 파일 이름을 붙인 경로다. 레코드의 부모 디렉터리도 기기 키 디렉터리와 같은 권한 계약(아래 "파일 권한")을 받는다. 게시·정리 잠금은 `<실제 대상>.lock` 파일의 배타 잠금이고, 잠금 파일은 정리 뒤에도 남는다. 그래서 파일 이름이 `.lock`으로 끝나는(대소문자 무관) 경로는 다른 레코드의 잠금 파일과 겹칠 수 있어 게시·정리 모두 `IdentityError::InvalidRecordPath`로 거절한다. 파일 이름이 없는 경로도 같은 오류다.
- `ConnectionRecord::parse(text)`는 레코드 파서 하나다. 앞뒤 공백을 무시하고, 십진 숫자만 있으면 `ParsedRecord::LegacyPort(port)`, JSON 객체면 `ParsedRecord::Record(..)`를 돌려준다. 모르는 필드는 무시한다. 실패는 `RecordParseError`이고 필드 이름이나 형식 종류만 담는다(아래 "토큰 비교"). `RecordPublication::retire()`는 `RetireOutcome::{Removed, Replaced, Missing}`을 돌려준다. 부모 디렉터리가 이미 없으면 `Missing`이다.
- `publish`는 읽는 쪽이 거절할 레코드(포트 0, 빈 인스턴스 ID, 4096바이트를 넘는 줄)를 쓰기 전에 `IdentityError::InvalidRecord`로 거절하고 기존 레코드를 그대로 둔다. 파일 정체성은 교체 전 임시 파일의 핸들에서 얻는다. 교체 뒤 확인 단계가 실패하면 잠금을 쥔 채 교체한 파일을 지우고 오류를 돌려준다. 핸들 없이 토큰이 든 레코드가 남지 않게 하기 위해서다.
- 이 레포가 쓴 파일(임시 파일, 교체 뒤 실패한 레코드)을 지우지 못하면 `IdentityError::Leftover { leftover, cause, cleanup }`로 남은 경로와 처음 오류를 함께 돌려준다. 그 파일에는 비밀이 들어 있을 수 있다. 오류에는 비밀 원문을 담지 않는다.
- 형식이 맞지 않는 파일은 손상으로 본다. `load_or_create`는 손상 파일을 만나면 `IdentityError::Corrupt`를 돌려주고 파일을 그대로 둔다. 새 키로 덮지 않는다. 덮으면 모든 상대가 이 기기를 다른 기기로 본다.
- 키의 정본 표현은 32바이트 seed다. `iroh-base` 1.5.0의 `SecretKey::from_bytes(&[u8; 32])`로 그대로 iroh endpoint에 넘긴다(P2P와 SSH에서 같은 기기 ID).

## 기기 ID 문자열

기기 ID는 ed25519 공개키다. 문자열 표현은 소문자 hex 64자이고, `iroh-base` 1.5.0 `PublicKey`의 `Display`(`data_encoding::HEXLOWER`, `iroh-base/src/key.rs`)와 같다. 같은 키가 토폴로지, relay 접속 허용 헤더, `PublicKey`의 `Display`에서 같은 전체 문자열로 보인다. iroh와 iroh-relay의 내부 로그(연결 성립, relay 등록)는 `fmt_short()`로 처음 5바이트(hex 10자)만 찍으므로 전체 ID와 같지 않다(`iroh-base/src/key.rs` `fmt_short`). 이 레포의 로그와 오류는 전체 ID를 쓴다. iroh의 `FromStr`은 hex와 base32를 모두 받으므로, 이 레포의 `DeviceId::from_str`은 와이어와 파일에서 소문자 hex 64자만 받도록 따로 검사한다.

## 파일 권한

| OS | 계약 |
|---|---|
| Unix | 디렉터리 0700, 파일 0600. 비밀을 쓰기 전에 권한을 건다. 디렉터리가 이미 있고 더 넓은 권한이면 좁힌다. 읽을 때는 대상이 심볼릭 링크가 아닌 일반 파일이고, 소유자가 현재 사용자이며, 권한이 0600보다 넓지 않은지 확인한다. 어긋나거나 열 수 없으면 `IdentityError::UnsafeFile`(손상과 구별)로 멈춘다 |
| Windows | 임시 파일은 `CreateFileW`에 보안 설명자(소유자 = 현재 사용자 SID, 상속 차단, 현재 SID FullControl 항목 하나만 있는 보호된 DACL)를 주어 처음부터 그 권한으로 만들고, 쓰기가 끝날 때까지 공유 모드 0으로 연다. 비밀을 쓰기 전에 같은 보안을 다시 걸고 다시 읽어 그대로인지 확인한다. 설정이나 확인이 실패하면 내용을 쓰지 않고 오류를 돌려준다. 이미 있는 파일을 읽을 때(기존 키, 첫 생성 경쟁에서 진 경우)는 연 핸들 기준으로 일반 파일인지(재분석 지점이 아님), 소유자가 현재 SID인지, DACL이 게시 때와 같은 형태(보호된 DACL, 상속 차단, 현재 SID의 허용 항목 하나)인지 확인한다. DACL이 없거나 NULL DACL이면(Windows는 이때 모두에게 접근을 허용한다) 허용 항목이 하나도 없어 보여도 위반이다. 디렉터리나 재분석 지점(심볼릭 링크, junction)도 핸들로 열어 판정한다. 어긋나거나, 권한 때문에 열 수 없거나, 검사 자체가 실패하면 `IdentityError::UnsafeFile`(손상과 구별)로 멈춘다 |

- Windows 계약은 tasty의 `tasty:crates/tasty-remote-profiles/src/secret_file.rs`와 같다. 구현은 PowerShell 대신 `windows-sys`의 보안 API로 한다. 외부 프로세스를 띄우지 않으므로 PowerShell 실행 정책·경로에 기대지 않고, 같은 프로세스 안에서 시험으로 다시 읽어 확인할 수 있다.
- 읽기 검사는 이 레포가 로컬에서 비밀 파일을 여는 모든 경우에 같다. 기기 키와 연결 레코드(게시 확인, 정리 때 내용 비교) 모두다. 원격의 연결 레코드는 원격 `ssh` 명령이 읽으므로 이 검사 밖이고, 원격 쪽 권한은 그 레코드를 게시한 원격 인스턴스의 이 레포가 건다.
- 소비자가 임의 디렉터리를 줄 수 있으므로 "사용자 프로필 아래라서 안전하다"는 가정을 하지 않는다.
- 알려진 한계: Unix 검사는 모드 비트와 소유자만 본다. macOS의 확장 ACL(모드 비트보다 먼저 적용되고 새 파일에 상속될 수 있다)은 보지 않는다. tasty의 `tasty:crates/tasty-remote-profiles/src/secret_file.rs`도 같다.
- 기기 키 디렉터리와 연결 레코드의 부모 디렉터리는 없으면 만든다(Unix 0700). 만들기 전에 그 경로의 마지막 이름을 링크를 따라가지 않고 본다. 심볼릭 링크(끊어진 링크 포함)이거나 디렉터리가 아니면 `IdentityError::UnsafeFile`이다. 끝의 경로 구분자는 떼고 판정하므로 `링크/`도 링크로 거절된다. Windows는 디렉터리 권한을 바꾸지 않고 파일에만 위 계약을 건다.
- 새 비밀 파일의 권한 설정이나 확인이 실패하면 `IdentityError::Protect`(임시 파일은 지움, 내용 없음)다.

## 동시 첫 생성

같은 디렉터리에서 두 프로세스(또는 스레드)가 동시에 처음 키를 만들어도 모두 같은 키를 읽어야 한다.

1. 권한을 건 임시 파일(같은 디렉터리, 고유 이름, `create_new`로 만들어 기존 파일·심볼릭 링크를 따라가지 않음)에 키를 쓰고 `sync_all`한다.
2. `std::fs::hard_link(임시, device_key)`로 게시한다. 대상이 있으면 실패하므로 덮어쓰지 않는다. `rename`은 대상이 있으면 바꿔 버릴 수 있어 쓰지 않는다.
3. 게시에 성공해도, `AlreadyExists`여도, 그 밖의 오류여도 임시 이름은 지운다. 지우지 못하면 `IdentityError::Leftover`다(게시에 성공했으면 `cause`가 없다). `AlreadyExists`면 이미 있는 파일을 위 "파일 권한"의 읽기 검사와 함께 읽는다. 쓰기 도중 실패하면 임시 파일을 지우고 오류를 돌려준다.
4. 파일 시스템이 하드 링크를 지원하지 않으면 오류를 돌려준다(조용히 다른 방식으로 바꾸지 않는다).

## 서명

- `DeviceKey::sign(&[u8]) -> Signature`, `DeviceId::verify(&[u8], &Signature) -> bool`. 검증은 `verify_strict`다(`iroh-base` `PublicKey::verify`).
- `Signature`는 64바이트이고 문자열 표현은 소문자 hex 128자다(handshake `sig`). `FromStr`은 그 형식만 받는다.
- 파일 없이 키를 만드는 `DeviceKey::from_seed([u8; 32])`·`DeviceKey::generate() -> Result<DeviceKey, RandomError>`와 iroh에 넘길 `DeviceKey::to_seed()`를 둔다. `DeviceKey`의 `Debug`는 seed를 찍지 않는다.
- handshake는 서명 대상의 SHA-256에 서명한다([handshake v2](handshake.md) "흐름 (SSH 경로)").

## 토큰 비교

- 받은 토큰과 자기 토큰은 상수 시간으로 비교한다. 길이가 다르면 바로 false, 같으면 바이트 XOR을 모두 누적해 비교한다(또는 `subtle`). 구현은 `subtle`이고, 받은 와이어 바이트와 비교하는 `ConnectionToken::matches(&[u8]) -> bool`과 `constant_time_eq(&[u8], &[u8])`를 둔다. 값은 `ConnectionToken::expose_hex()`로만 꺼낸다.
- 토큰은 로그에 쓰지 않는다. `Debug`·`Display`는 값을 가린다.
- 토큰 타입으로 바뀌기 전의 원문도 같다. 연결 레코드 파싱이 실패하면 오류와 로그에는 실패한 필드 이름이나 형식 종류(예: `token: not 64 lowercase hex`)만 남기고 레코드 원문(발견 명령의 stdout)은 넣지 않는다. 지금 tasty의 포트 발견은 파싱 실패 때 stdout 전체를 진단 detail에 넣고 debug 로그에 쓰는데(`tasty:crates/tasty-ssh/src/lib.rs` `parse_port`·`PortDiscoveryError::new`), 숫자 포트에서는 진단 자료였지만 레코드에는 토큰이 있으므로 옮기지 않는다. 실패 분류 enum과 체인은 그대로 둔다.

## 토큰 전달

토큰은 SSH로만 읽을 수 있는 경로로 상대에게 간다. 이 레포의 포트 발견(`tasty-net-ssh`)이 원격의 연결 레코드를 한 번 읽어 포트·토큰·인스턴스 ID를 함께 얻는다(subcommand 출력 또는 권한 걸린 파일). 읽는 경로의 우선순위와 실패 분류는 포트 발견과 같다([레포 경계](repo-boundary.md) "포트 발견의 tasty 관례"). 원격 쪽이 레코드를 게시하는 일은 tasty 몫이다. 출력이 레코드가 아니라 포트 숫자 하나면(지금 tasty) 발견 결과는 `Discovered::LegacyPort(port)`다. SSH 전송은 이 결과로 `stream.open`을 보내지 않고 로컬 오류 `PeerTooOld { evidence: PortOnlyDiscovery }`로 끝낸다. 토큰 없이는 v2의 어느 목적도 열 수 없기 때문이다. 다시 걸지 않는다. 연결한 뒤 v1 거절 ACK를 읽은 경우(`evidence: V1Ack`)와는 `evidence`로 구별한다([handshake v2](handshake.md) "구버전 처리").

## TOFU

처음 본 상대 기기 ID를 기록하고, 다음부터 다른 키면 멈춘다.

- 판정 로직은 이 레포, 기록 저장소는 소비자가 trait로 준다: `PeerKeyStore { fn get(&self, peer: &PeerRef) -> Result<Option<DeviceId>, StoreError>; fn pin_if_absent(&self, peer: &PeerRef, id: &DeviceId) -> Result<PinOutcome, StoreError>; fn replace(&self, peer: &PeerRef, id: &DeviceId) -> Result<(), StoreError> }`. `PeerRef`는 소비자가 정하는 상대 이름(tasty는 원격 프로필 이름)이다. `StoreError`는 소비자 저장소의 오류를 그대로 담아 넘긴다. 판정 함수는 `check_peer(store, peer, id) -> Result<TofuVerdict, StoreError>`다.
- 첫 기록은 `pin_if_absent` 한 번으로 원자적으로 정한다. 결과 `PinOutcome::{Inserted, Existing(DeviceId)}`는 이 호출이 기록했는지와 확정된 값을 함께 준다. 기록이 없으면 받은 ID를 기록하고 `Inserted`, 이미 있으면 기록을 바꾸지 않고 `Existing(기존 값)`을 돌려준다. 판정은 `get`을 앞에 부르지 않고 이 결과 하나로 한다. 그래서 같은 상대에게 첫 연결 두 개가 겹쳐도 `Inserted`는 하나뿐이다.
- 결과: `TofuVerdict::{New, Match, Mismatch { expected, got }}`. `Inserted` → `New`(통과), `Existing`이 받은 ID와 같으면 `Match`(통과), 다르면 `Mismatch`이고 기록을 바꾸지 않고 handshake를 `peer_key_mismatch`로 끝낸다. `get`은 표시용이다.
- 기존 기록을 바꾸는 길은 사용자가 명시적으로 다시 등록하는 경로 하나다: `reregister(store, peer, id)`가 `replace`를 부른다. handshake는 `replace`를 부르지 않는다. 경고 화면과 확인은 tasty 몫이다.
- handshake는 TOFU 판정을 자기 자신 검사 뒤, 도메인 콜백(attach 승인) 전에 부른다. 자기 자신과의 연결은 기록을 남기지 않는다([handshake v2](handshake.md) "검사 순서").
