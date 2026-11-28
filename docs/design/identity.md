# 기기 키·기기 ID와 연결 토큰

`tasty:<경로>`는 tasty 레포 698aa49e03 기준이다. 크레이트는 `tasty-net-identity`다.

## 두 비밀의 수명

| 비밀 | 수명 | 공유 범위 | 쓰는 곳 |
|---|---|---|---|
| 기기 키(ed25519 비밀키) | 영속. 디렉터리에 한 번 만들고 계속 쓴다 | 같은 디렉터리를 쓰는 모든 인스턴스. P2P endpoint는 그중 하나만 연다([P2P](p2p.md#endpoint와-런타임)) | handshake 서명, P2P TLS raw public key(기기 사이와 rendezvous 서버 연결), 토폴로지 노드 ID |
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

- 연결 레코드 경로의 마지막 이름이 심볼릭 링크이거나 일반 파일이 아니면 게시하지 않고 `IdentityError::UnsafeFile`을 돌려준다(위 "파일 권한"의 읽기 검사가 링크를 거절하므로 링크를 따라가 게시하면 그 레코드를 다시 읽을 수 없다). 핸들이 기억하는 "실제 대상"은 정규화한 부모 디렉터리에 파일 이름을 붙인 경로다. 레코드의 부모 디렉터리도 기기 키 디렉터리와 같은 권한 계약(아래 "파일 권한")을 받는다. 게시·정리 잠금은 `<실제 대상>.lock` 파일의 배타 잠금이고, 잠금 파일은 정리 뒤에도 남는다. Unix에서 이 크레이트가 만든 잠금 파일은 umask와 무관하게 0600이고, 이미 있는 잠금 파일은 권한을 바꾸지 않으며 쓰기로 열 수 없으면 읽기 전용으로 열어 잠근다(`flock`은 읽기 전용 파일 설명자에도 걸린다). 읽기 전용 잠금 열기는 로컬 파일 시스템 기준이다. Linux NFS에서는 `flock`이 쓰기 가능한 핸들을 요구해 0400 잠금 파일에서 게시·회수가 `Io(Lock)`로 실패한다. 잠금 파일은 멈추지 않게 열고(Unix `O_NONBLOCK`), 연 핸들이 일반 파일이 아니면(FIFO 등) 잠그지 않고 `IdentityError::UnsafeFile`이다. 그래서 파일 이름이 `.lock`으로 끝나는(대소문자 무관) 경로는 다른 레코드의 잠금 파일과 겹칠 수 있어 게시·정리 모두 `IdentityError::InvalidRecordPath`로 거절한다. 파일 이름이 없는 경로도 같은 오류다. 알려진 한계: 이 검사는 받은 철자만 본다. 8.3 별칭을 만드는 Windows 볼륨에서는 잠금 파일의 짧은 이름(확장자가 세 글자로 줄어든다)으로 이 검사를 피할 수 있다. 소비자는 레코드 경로를 긴 이름으로 준다.
- `ConnectionRecord::parse(text)`는 레코드 파서 하나다. 앞뒤 공백을 무시하고, 십진 숫자만 있으면 `ParsedRecord::LegacyPort(port)`, JSON 객체면 `ParsedRecord::Record(..)`를 돌려준다. 모르는 필드는 무시한다. 실패는 `RecordParseError`이고 필드 이름이나 형식 종류만 담는다(아래 "토큰 비교"). `RecordPublication::retire()`는 `RetireOutcome::{Removed, Replaced, Missing}`을 돌려준다. 부모 디렉터리가 이미 없으면 `Missing`이다. Windows에서 레코드가 지워졌지만 다른 핸들이 열려 있어 삭제 대기인 동안은 그 경로를 열 수 없다. 이때 게시 핸들이 쥔 파일이 삭제 대기 상태이면 `Missing`이다. 알려진 한계: 이 판정은 경로의 파일이 아니라 게시 핸들의 파일 상태로 본다. 그래서 레코드가 다른 이름으로 옮겨진 뒤 삭제 대기가 되고, 원래 경로에 읽을 수 없는 다른 파일이 들어온 경우도 `Missing`이 된다.
- `publish`는 읽는 쪽이 거절할 레코드(포트 0, 빈 인스턴스 ID, `max_secret_bytes`를 넘는 줄, 아래 "설정값")를 쓰기 전에 `IdentityError::InvalidRecord`로 거절하고 기존 레코드를 그대로 둔다. 파일 정체성은 교체 전 임시 파일의 핸들에서 얻는다. 교체 뒤 확인 단계가 실패하면 잠금을 쥔 채, 대상의 파일 정체성이 자기 게시분과 같을 때만 그 파일을 지우고 오류를 돌려준다. 핸들 없이 토큰이 든 레코드가 남지 않게 하기 위해서다. 정체성이 다르거나 알 수 없으면(잠금에 참여하지 않는 쪽이 대상을 바꿈) 그 파일은 남의 것이므로 지우지 않고 오류만 돌려준다. 알려진 한계: 이 정리는 파일 정체성을 확인한 뒤 경로 이름으로 지운다. 핸들로 지우는 이식 가능한 방법이 없어서다. 확인과 삭제 사이에 게시 잠금에 참여하지 않는 프로세스가 그 경로의 파일을 바꾸면 바뀐 파일이 지워질 수 있다. `retire`의 삭제도 같다.
- 이 레포가 쓴 파일(임시 파일, 교체 뒤 실패한 레코드)을 지우지 못하면 `IdentityError::Leftover { leftover, cause, cleanup }`로 남은 경로와 처음 오류를 함께 돌려준다. 그 파일에는 비밀이 들어 있을 수 있다. 오류에는 비밀 원문을 담지 않는다.
- 형식이 맞지 않는 파일은 손상으로 본다. `load_or_create`는 손상 파일을 만나면 `IdentityError::Corrupt`를 돌려주고 파일을 그대로 둔다. 새 키로 덮지 않는다. 덮으면 모든 상대가 이 기기를 다른 기기로 본다.
- 키의 정본 표현은 32바이트 seed다. ed25519 구현은 `ed25519-dalek`(`SigningKey::from_bytes(&[u8; 32])`)이다. P2P는 같은 seed로 TLS 서명 키(PKCS#8)와 raw public key를 만든다([P2P](p2p.md#인증)). 그래서 P2P와 SSH에서 같은 기기 ID다. identity는 `ed25519-dalek` 3.0.0을 직접 의존한다. 전에 거치던 `iroh-base` 1.5.0도 같은 판을 썼으므로 그 전에 만든 키 파일과 기기 ID, 서명 값은 그대로다(고정 seed 시험이 고정한다).

## 기기 ID 문자열

기기 ID는 ed25519 공개키다. 문자열 표현은 소문자 hex 64자다. 같은 키가 handshake, 토폴로지, rendezvous 서버의 기기 조회 키(`punch_request.peer`, [rendezvous](rendezvous.md#기기-인증)), P2P 연결 정보에서 같은 전체 문자열로 보인다. 이 레포의 로그와 오류는 전체 ID를 쓴다. `DeviceId::from_str`은 와이어와 파일에서 소문자 hex 64자만 받는다(소문자 hex 디코더가 곧 파서다). 그 밖의 문자열(대문자, base32, 길이 틀림)은 `DeviceIdParseError::Format`, hex지만 곡선 위의 점이 아닌 32바이트는 `DeviceIdParseError::InvalidKey`다.

## 파일 권한

| OS | 계약 |
|---|---|
| Unix | 디렉터리 0700, 파일 0600. 비밀을 쓰기 전에 권한을 건다. 디렉터리가 이미 있고 더 넓은 권한이면 좁힌다. 읽을 때는 대상이 심볼릭 링크가 아닌 일반 파일이고, 소유자가 현재 사용자이며, 권한이 0600보다 넓지 않은지 확인한다. 어긋나거나 열 수 없으면 `IdentityError::UnsafeFile`(손상과 구별)로 멈춘다 |
| Windows | 임시 파일은 `CreateFileW`에 보안 설명자(소유자 = 현재 사용자 SID, 상속 차단, 현재 SID FullControl 항목 하나만 있는 보호된 DACL)를 주어 처음부터 그 권한으로 만들고, 쓰기가 끝날 때까지 공유 모드 0으로 연다. 비밀을 쓰기 전에 같은 보안을 다시 걸고 다시 읽어 그대로인지 확인한다. 설정이나 확인이 실패하면 내용을 쓰지 않고 오류를 돌려준다. 이미 있는 파일을 읽을 때(기존 키, 첫 생성 경쟁에서 진 경우)는 연 핸들 기준으로 일반 파일인지(재분석 지점이 아님), 소유자가 현재 SID인지, DACL이 게시 때와 같은 형태(보호된 DACL, 상속 차단, 현재 SID의 허용 항목 하나)인지 확인한다. DACL이 없거나 NULL DACL이면(Windows는 이때 모두에게 접근을 허용한다) 허용 항목이 하나도 없어 보여도 위반이다. 디렉터리나 재분석 지점(심볼릭 링크, junction)도 핸들로 열어 판정한다. 어긋나거나, 권한 때문에 열 수 없거나, 검사 자체가 실패하면 `IdentityError::UnsafeFile`(손상과 구별)로 멈춘다 |

- Windows 계약은 tasty의 `tasty:crates/tasty-remote-profiles/src/secret_file.rs`와 같다. 구현은 PowerShell 대신 `windows-sys`의 보안 API로 한다. 외부 프로세스를 띄우지 않으므로 PowerShell 실행 정책·경로에 기대지 않고, 같은 프로세스 안에서 시험으로 다시 읽어 확인할 수 있다.
- 읽기 검사는 이 레포가 로컬에서 비밀 파일을 여는 모든 경우에 같다. 기기 키와 연결 레코드(게시 확인, 정리 때 내용 비교) 모두다. 원격의 연결 레코드는 원격 `ssh` 명령이 읽으므로 이 검사 밖이고, 원격 쪽 권한은 그 레코드를 게시한 원격 인스턴스의 이 레포가 건다.
- 소비자가 임의 디렉터리를 줄 수 있으므로 "사용자 프로필 아래라서 안전하다"는 가정을 하지 않는다.
- 알려진 한계: Unix 검사는 모드 비트와 소유자만 본다. macOS의 확장 ACL(모드 비트보다 먼저 적용되고 새 파일에 상속될 수 있다)은 보지 않는다. tasty의 `tasty:crates/tasty-remote-profiles/src/secret_file.rs`도 같다.
- 기기 키 디렉터리와 연결 레코드의 부모 디렉터리는 없으면 만든다(Unix 0700). 만들기 전에 그 경로의 마지막 이름을 링크를 따라가지 않고 본다. 심볼릭 링크(끊어진 링크 포함)이거나 디렉터리가 아니면 `IdentityError::UnsafeFile`이다. 끝의 경로 구분자는 떼고 판정하므로 `링크/`도 링크로 거절된다. Unix는 아래에서 위로 이미 있는 첫 조상을 찾고, 그 아래 없는 요소를 위에서부터 하나씩 만든다. 이 크레이트가 만든 디렉터리(중간 디렉터리 포함)마다 umask와 무관하게 0700을 건다. 이미 있던 중간 디렉터리의 권한은 바꾸지 않는다. 만드는 사이 다른 쪽이 먼저 만든 요소는 링크나 디렉터리가 아닌 것이면 `IdentityError::UnsafeFile`이다. 다 만든 뒤 없다고 본 요소들의 부모 디렉터리를 동기화한다(아래 "폴더 동기화"). 같은 경로를 동시에 만드는 다른 스레드의 부모가 아직 0700이 되기 전이면 잠깐 다시 시도한다(횟수와 간격은 아래 "설정값"의 `new_parent_retries`·`new_parent_wait`). Windows는 디렉터리 권한을 바꾸지 않고 파일에만 위 계약을 건다.
- 새 비밀 파일의 권한 설정이나 확인이 실패하면 `IdentityError::Protect`(임시 파일은 지움, 내용 없음)다. Windows는 보안 설명자를 붙여 파일을 만드므로, 그 구성 실패와 `CreateFileW`가 소유자·주 그룹·권한을 거부한 실패(`ERROR_INVALID_OWNER`·`ERROR_INVALID_PRIMARY_GROUP`·`ERROR_PRIVILEGE_NOT_HELD`)도 `Protect`다. 그 밖의 생성 실패는 `IdentityError::Io`다.

## 동시 첫 생성

같은 디렉터리에서 두 프로세스(또는 스레드)가 동시에 처음 키를 만들어도 모두 같은 키를 읽어야 한다.

1. 권한을 건 임시 파일(같은 디렉터리, 고유 이름, `create_new`로 만들어 기존 파일·심볼릭 링크를 따라가지 않음)에 키를 쓰고 `sync_all`한다.
2. 임시 파일을 `device_key` 이름으로 게시한다. 대상이 있으면 실패하고 덮어쓰지 않는 방법만 쓴다(아래 "게시 방법"). 대상을 바꿔 버리는 보통 `rename`은 쓰지 않는다.
3. 게시가 이름을 옮겼으면 임시 이름은 이미 없다. 하드 링크로 게시했거나, `AlreadyExists`였거나, 그 밖의 오류면 임시 이름을 지운다. 지우지 못하면 `IdentityError::Leftover`다(키가 제자리에 있으면, 곧 하드 링크 게시나 `AlreadyExists`면 `cause`가 없다). 키가 제자리에 있으면(이름을 옮겼거나, 하드 링크나 `AlreadyExists`였으면) 임시 이름을 지우려 한 뒤 디렉터리를 동기화한다(아래 "폴더 동기화"). 임시 이름을 지우지 못해 `Leftover`를 돌려줄 때도 동기화한 뒤 돌려준다. `AlreadyExists`면 이미 있는 파일을 위 "파일 권한"의 읽기 검사와 함께 읽는다. 쓰기 도중 실패하면 임시 파일을 지우고 오류를 돌려준다.
4. OS와 파일 시스템이 덮어쓰지 않는 이름 옮기기도 하드 링크도 지원하지 않으면 `IdentityError::Io`(`FileOp::Publish`)를 돌려준다. 이름 옮기기가 "지원하지 않음"으로 실패한 뒤 하드 링크도 실패했으면, 그 오류의 `io::Error`는 종류가 하드 링크 쪽이고, 메시지에 두 단계(하드 링크, 이름 옮기기)와 각 OS 오류 번호가 모두 남는다. 하드 링크 오류는 그 `source`다. 하드 링크가 `AlreadyExists`이면(다른 생성자가 이김) 그대로 돌려준다. 이름 옮기기를 시도하지 않는 OS(Windows·그 밖의 Unix)의 하드 링크 오류도 그대로다. 보통 `rename`이나 확인 뒤 쓰기처럼 덜 안전한 방식으로 조용히 바꾸지 않는다.

알려진 한계(Unix): umask가 소유자 권한 비트를 빼면(예: 0277·0377·0400) 이 크레이트가 새 디렉터리·잠금 파일을 만든 직후 0700·0600을 걸기 전의 짧은 틈이 생긴다. 그 틈에 같은 경로로 들어온 다른 호출은 권한 오류로 실패할 수 있다. 오류 종류는 어디서 막히느냐에 따라 다르다.

- 새 디렉터리 안에서 키 임시 파일을 만들 때(디렉터리가 0500): `IdentityError::Io`(`FileOp::CreateTemp`).
- 새 디렉터리 안에서 레코드 잠금 파일을 열 때, 또는 새 잠금 파일이 아직 읽을 수 없을 때(umask 0400에서 0200): `IdentityError::Io`(`FileOp::Lock`).
- 탐색 권한이 없는 새 중간 디렉터리 아래를 볼 때(0400): `IdentityError::Io`(`FileOp::CreateDir`).
- 탐색 권한이 없는 새 키 디렉터리에서 기존 키를 읽을 때(0400): 키 파일을 검사할 수 없으므로 `IdentityError::UnsafeFile`(`UnsafeReason::CheckFailed`).

비밀이 새거나 권한이 넓어지지는 않고, 다시 호출하면 성공한다. 소유자 비트를 빼는 umask는 일반적인 설정이 아니다.

### 게시 방법

| OS | 먼저 쓰는 방법 | 그 방법이 없을 때 |
|---|---|---|
| Linux·Android | `renameat2(AT_FDCWD, 임시, AT_FDCWD, device_key, RENAME_NOREPLACE)`를 `libc::syscall(SYS_renameat2, …)`로 부른다. bionic의 `renameat2` 래퍼는 API 30부터라 minSdk 29에서 쓸 수 없다. 번호는 libc 크레이트의 `SYS_renameat2`(aarch64 276, x86_64 316) | `ENOSYS`(커널 3.15 전)나 `EINVAL`(그 플래그를 모르는 파일 시스템)이면 하드 링크. 하드 링크도 실패하면 두 오류를 모두 담은 `Publish` 오류(위 4). 그 밖의 오류는 그대로 `Publish` 오류 |
| macOS·iOS | `renamex_np(임시, device_key, RENAME_EXCL)`(macOS 10.12·iOS 10부터. Rust가 지원하는 가장 낮은 판에 이미 있다) | `ENOTSUP`(그 플래그를 지원하지 않는 파일 시스템)이나 `EINVAL`이면 하드 링크. 하드 링크도 실패하면 Linux와 같다. 그 밖의 오류는 그대로 |
| Windows | 하드 링크(`std::fs::hard_link` = `CreateHardLinkW`). 이전과 같다 | 해당 없음 |
| 그 밖의 Unix | 하드 링크 | 해당 없음 |

- 하드 링크 대신 이름 옮기기를 먼저 쓰는 이유: Android 앱 문맥은 앱 데이터 폴더(`app_data_file`)의 `link`를 SELinux로 거절한다(`avc denied { link }`, API 37 기기에서 확인). 앱이 쓸 수 있는 내부 저장소는 모두 같은 종류라 폴더를 바꿔서는 풀리지 않는다.
- Android 10(API 29)의 앱 seccomp 필터는 `renameat2`를 허용한다(AOSP bionic android10-release `libc/SECCOMP_WHITELIST_COMMON.TXT`). API 30부터는 bionic의 시스템 호출 목록에 들어 있다. 허용 목록에 없는 시스템 호출은 `SECCOMP_RET_TRAP`(SIGSYS)이므로 이 확인이 필요했다.
- 하드 링크로 넘어가는 경우는 3.15 전 커널이나 `RENAME_NOREPLACE`를 모르는 파일 시스템(예: 일부 네트워크·FUSE 파일 시스템, 지원 전 판의 f2fs)이다. 하드 링크도 같은 계약(대상이 있으면 실패)을 지키므로 원자성은 낮아지지 않는다. 그런 Android 기기에서는 SELinux가 하드 링크도 막으므로 키 생성이 `Publish` 오류로 실패한다(조용히 다른 방식으로 바꾸지 않는다).
- Linux 데스크톱도 Android와 같은 경로를 탄다. 그래서 Linux 시험이 Android의 게시 경로를 그대로 돈다.

고른 근거가 된 비교(계약: 기존 키를 덮어쓰지 않음, 동시 생성이 같은 키를 얻음, 반쯤 쓴 파일이 안 보임, 기존 설치의 경로·형식을 그대로 읽음, 데스크톱 동작·원자성 그대로, Android minSdk 29와 iOS에서도 같은 계약):

| 후보 | 원자성 | 덮어쓰기 | 반쯤 쓴 파일 | OS 지원 | unsafe | 판정 |
|---|---|---|---|---|---|---|
| 하드 링크(이전 방식) | 원자적 | 안 함(`EEXIST`) | 안 보임(`sync_all` 뒤 게시) | Android 앱 폴더에서 SELinux로 거절 | 없음 | Windows·그 밖의 Unix와 대체 경로로만 쓴다 |
| `renameat2(RENAME_NOREPLACE)` syscall | 원자적 | 안 함(`EEXIST`) | 안 보임 | Linux 3.15+, Android 10의 seccomp 허용. 파일 시스템이 모르면 `EINVAL` | 시스템 호출 한 곳 | Linux·Android에 고름 |
| `renamex_np(RENAME_EXCL)` | 원자적 | 안 함(`EEXIST`) | 안 보임 | macOS 10.12+·iOS 10+. 파일 시스템이 모르면 `ENOTSUP` | 함수 호출 한 곳 | macOS·iOS에 고름. iOS 앱 컨테이너가 하드 링크를 허용하는지에 기대지 않는다 |
| Windows `MoveFileExW`(`MOVEFILE_REPLACE_EXISTING` 없이) | 같은 볼륨에서 원자적 | 안 함(`ERROR_ALREADY_EXISTS`) | 안 보임 | Windows | 있음(`std::fs::rename`은 바꾸는 플래그를 켜므로 직접 불러야 함) | 고르지 않음. 하드 링크가 Windows에서 막히지 않으므로 바꿀 이유가 없다 |
| 잠금 파일 + 있는지 확인 + 보통 `rename` | 잠금을 잡는 쪽끼리만 | 잠금 없이 게시하는 이전 판과 겹치면 덮어쓸 수 있음 | 안 보임 | `File::lock` 필요 | 없음 | 고르지 않음. 덮어쓰지 않음을 OS가 보장하지 않는다 |
| 최종 이름을 `O_CREAT\|O_EXCL`로 만들고 바로 씀 | 생성만 원자적 | 안 함 | 보임. 쓰다 죽으면 영구히 반쯤 쓴 파일 | 모두 | 없음 | 고르지 않음. 반쯤 쓴 파일이 `Corrupt`로 남고, 이전 판은 쓰는 중인 파일을 손상으로 읽는다 |

### 폴더 동기화

게시(이름 옮기기·하드 링크)와 새 디렉터리는 디렉터리 항목을 바꾼다. 키 파일 내용의 `sync_all`은 그 항목이 전원 차단 뒤에도 남는다고 보장하지 않는다. 항목이 사라지면 다음 실행이 새 키를 만들어 기기 ID가 바뀐다. 그래서 항목을 바꾼 디렉터리를 동기화한다.

- Unix: 키 디렉터리를 열어 `sync_all`을 한 번 부른다. 이름을 옮긴 게시, 하드 링크 뒤 임시 이름을 지운 경우, 다른 생성자에게 져서 임시 이름을 지운 경우, 그 두 경우에 임시 이름을 지우지 못한 경우(`Leftover`) 모두 같다. 같은 디렉터리의 항목 변경(새 이름, 지운 임시 이름)은 그 디렉터리 동기화 한 번이 덮는다. 진 쪽도 동기화하는 것은 이긴 쪽이 동기화하기 전에 진 쪽이 키를 돌려줄 수 있어서다. 키가 이미 있어 읽기만 한 호출은 동기화하지 않는다.
- 디렉터리를 만든 호출(위 "파일 권한"의 요소별 생성)은 없다고 본 요소마다 그 요소를 담은 부모 디렉터리를 가장 깊은 것부터 동기화하고, 이미 있던 조상에서 멈춘다. 만드는 사이 다른 쪽이 먼저 만든 요소도 넣는다. 그쪽의 동기화가 이 호출이 돌아가기 전에 끝난다는 보장이 없어서다. 연결 레코드의 부모 디렉터리도 같은 생성 경로를 탄다.
- 동기화는 경로를 다시 연다. 그 사이 경로의 마지막 요소가 다른 것으로 바뀌었을 수 있으므로 디렉터리로만 열고(`O_DIRECTORY`) 멈추지 않게 연다(`O_NONBLOCK`, FIFO가 들어와도 열기가 쓰는 쪽을 기다리지 않는다). 키 디렉터리와 이 호출이 만든 디렉터리는 마지막 요소의 링크를 거절한다(`O_NOFOLLOW`). 키 디렉터리는 연 핸들의 파일 정체성(장치·inode)이 위 "파일 권한"에서 링크를 따라가지 않고 확인한 디렉터리와 같아야 한다. 새 디렉터리를 만든 바로 위의 기존 조상은 소비자가 준 경로가 지나는 링크일 수 있어 링크를 따라가고, 남의 디렉터리일 수 있어 소유자를 보지 않는다. 어긋나면 동기화하지 않고 아래 실패와 같이 경고만 남긴다. 판정은 `secret_file/unix.rs`의 `sync_dir`, 시험은 `secret_file.rs`의 `a_directory_sync_refuses_what_replaced_the_directory`다.
- 알려진 한계: 동기화 대상은 이 호출이 처음 볼 때 없던 요소로 정한다. 다른 호출이 만들었지만 아직 동기화하지 않은 상위 디렉터리는 이미 있는 디렉터리로 보고 그 위를 동기화하지 않는다. 예를 들어 A가 `root/keys`를 만들고 `root`를 동기화하기 전에 B가 같은 경로로 들어오면, B는 키를 게시하고 `keys`만 동기화한 뒤 돌아갈 수 있다. 앞선 호출이 여러 요소 중 일부만 만들고 실패한 뒤의 재시도도 같다. 그래서 경로가 보이기 시작한 때와 그 항목이 영속된 때 사이에 키를 돌려줄 수 있다. 호출 사이에서 새 경로의 동기화를 맞추지 않는다.
- 연결 레코드의 교체(보통 `rename`)는 디렉터리를 동기화하지 않는다. 레코드는 인스턴스 하나의 수명이라 전원이 끊기면 그 인스턴스도 끝난다.
- macOS·iOS: `fsync`는 드라이브의 쓰기 캐시를 비우지 않는다. std의 `File::sync_all`은 이 OS들에서 `fcntl(F_FULLFSYNC)`를 부른다(툴체인 1.96.0 std `sys/fs/unix.rs`). 그래서 디렉터리도 키 파일 내용과 같은 수준(`F_FULLFSYNC`)으로 동기화된다. 따로 `unsafe` 호출을 두지 않는다.
- 동기화 실패는 경고 로그(`tracing::warn`)로 남기고 키를 돌려준다. 키는 이미 제자리에 있고, 오류로 돌려도 다음 호출은 그 키를 읽기만 하므로(동기화 없음) 오류가 내구성을 더하지 못한다. 디렉터리 생성 쪽의 동기화 실패도 같다.
- Windows: 디렉터리를 동기화하지 않는다. 게시는 `CreateHardLinkW`이고 이 호출은 쓰기 보장 플래그를 받지 않는다. `MOVEFILE_WRITE_THROUGH`는 `MoveFileExW`의 플래그이고(볼륨 사이의 복사 이동이 디스크에 내려간 뒤 돌아오게 한다) 이 크레이트는 `MoveFileExW`로 게시하지 않으므로 해당 없다. 새 디렉터리(`create_dir_all`)도 같다. 알려진 한계: NTFS가 새 하드 링크 항목을 언제 디스크에 내려 쓰는지와 그것을 강제하는 방법(디렉터리 핸들의 `FlushFileBuffers` 등)은 확인하지 않았다.
- 동기화 순서와 대상은 `device_key/tests.rs`의 `dir_sync` 시험과 하드 링크 경로 시험이 본다.

## 설정값

[크레이트 구성](crates.md#설정값-규칙)의 규칙을 따른다. 기본값의 원천은 코드의 기본값 표 하나다(`tasty_net_identity::settings::DEFAULTS`). 시험이 이 표를 읽어 코드와 값·범위까지 대조한다([CI와 시험 환경](testing.md#설정값-시험)).

- 바꾸기: `IdentitySettings::set(키, 값)`. 모르는 키, 다른 종류의 값, 범위 밖의 값은 `SettingsError`로 거절하고 아무것도 바꾸지 않는다. 설정을 받는 함수는 `DeviceKey::load_or_create_with(dir, &settings)`, `ConnectionRecord::publish_with(path, &settings)`다. 설정 없는 `load_or_create`·`publish`는 기본값(`IdentitySettings::default()`)을 쓴다. 게시 핸들은 게시 때의 `max_secret_bytes`를 기억해 `retire`의 다시 읽기에 쓴다.
- 조회: `IdentitySettings::entries()`(항목마다 `{key, value, source}`, 출처 `default`·`configured`)와 `get(키)`. 이 크레이트는 오래 사는 객체가 없으므로 소비자가 넘긴 설정 값 자체가 적용값이다.

| 키 | 기본 | 범위 | 근거 |
|---|---|---|---|
| `new_parent_retries` | 50 | 0 ~ 1000 | 같은 경로를 만드는 다른 스레드가 새 부모를 0700으로 바꿀 때까지 `EACCES`를 다시 시도하는 횟수(Unix, 위 "파일 권한"). 그 스레드는 `mkdir` 다음 호출에서 권한을 걸므로 간격과 곱한 0.5 s면 남는다. Windows는 쓰지 않는다 |
| `new_parent_wait` | 10 ms | 0 ms ~ 1 s | 그 재시도 사이의 대기 |
| `max_secret_bytes` | 4 KiB | 1 KiB ~ 64 KiB | 비밀 파일(기기 키, 연결 레코드)을 통째로 읽는 상한이자 게시하는 레코드 줄의 상한. 이 크레이트가 쓰는 파일은 기기 키 89바이트, 레코드 107바이트에 인스턴스 ID를 더한 것이라, 큰 파일을 메모리에 읽지 않으려는 자원 상한이다. 아래 끝은 기기 키 파일과 짧은 인스턴스 ID의 레코드가 들어가는 값이다. 다른 기기는 게시된 레코드를 이 값이 아니라 SSH 포트 발견의 출력 상한(`tasty-net-ssh`, 64 KiB)으로 읽으므로, 그보다 긴 레코드는 상대가 읽지 못한다. 그래서 위 끝을 그 값으로 둔다 |

설정이 아닌 값: 기기 키·토큰·서명의 길이와 hex 표현, 파일 권한 비트(0700·0600), Windows 오류 번호(`ERROR_INVALID_OWNER` 등), 확장 경로 접두(`\\?\`)를 붙이기 시작하는 경로 길이 248(std의 변환과 같다. 디렉터리 경로는 MAX_PATH 아래에 8.3 파일 이름 자리를 남겨야 한다)은 형식이나 OS 한계라 설정이 아니다. 목록은 `scripts/no-hidden-constants.list`의 identity 줄에 이유와 함께 있다.

## 서명

- `DeviceKey::sign(&[u8]) -> Signature`, `DeviceId::verify(&[u8], &Signature) -> bool`. 검증은 `verify_strict`다(`ed25519-dalek` `VerifyingKey::verify_strict`).
- `Signature`는 64바이트이고 문자열 표현은 소문자 hex 128자다(handshake `sig`). `FromStr`은 그 형식만 받는다.
- 파일 없이 키를 만드는 `DeviceKey::from_seed([u8; 32])`·`DeviceKey::generate() -> Result<DeviceKey, RandomError>`와 P2P TLS 키를 만들 때 쓰는 `DeviceKey::to_seed()`를 둔다. `DeviceKey`의 `Debug`는 seed를 찍지 않는다.
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
