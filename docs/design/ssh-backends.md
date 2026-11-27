# SSH 백엔드 (시스템 ssh와 내장 백엔드)

`tasty-net-ssh`가 SSH 연결을 여는 두 방법을 다룬다. 다루는 것은 백엔드 선택, feature 계약, 진입점과 연결 정보, 옵션 적용 순서, 터널·exec·SFTP의 경계, OpenSSH 해석 이식 크레이트 `tasty-net-ssh-config`, 지시어 분류, 호스트 키 판정, 알고리즘, 설정값, 의존 사실, 시험의 자리다. 모바일 빌드에서 어느 조합을 쓰는지는 [모바일](mobile.md)이 정하고, 크레이트 이름과 의존 방향의 정본은 [크레이트 구성](crates.md)이다.

## 사용자 결정

| 날짜 | 원문 또는 결정 | 이 문서의 결과 |
|---|---|---|
| 2026-11-27 | "그래 폴백으로 넣자 / 환경 따지지말고 설치된 openssh가 없다면 자체 ssh 모듈로 동작하는거야" | [선택 규칙](#선택-규칙). OS로 나누지 않는다 |
| 2026-11-27 | "그냥 tasty 구현쪽에서 옵션에 ssh 관련 설정을 넣으면 될거야" | 백엔드 선택, 파일 위치, 덮어쓰기 값을 라이브러리 설정값으로 받는다([설정값](#설정값)) |
| 2026-11-27 | "그리고 자체 ssh의 설정파일이나 키파일 자체는 Openssh랑 동일한 방식으로 최대한 동작 가능하게 해" | 내장 백엔드는 OpenSSH와 같은 형식의 파일을 같은 뜻으로 읽는다. 앞 결정의 "OpenSSH 파일을 흉내 내지 않는다"는 철회됐다 |
| 2026-11-27 | 추가 결정 4: 내장 SSH는 "russh + OpenSSH 해석 이식", 모르는 키워드는 "OpenSSH처럼 거부 + 구분", 라이선스는 "별도 크레이트로 분리" | 연결과 암호는 russh가 맡는다. 설정 해석과 known_hosts 해석은 OpenSSH 코드를 옮긴 [이식 크레이트](#openssh-해석-이식-크레이트)가 맡는다. [지시어 분류](#지시어-분류), [호스트 키 판정](#호스트-키-판정), [알고리즘](#알고리즘) |
| 2026-11-27 | 추가 결정 4의 기술 판단(director): russh 보안 패치를 따라가고 CI에 cargo-audit를 둔다. `@cert-authority` 줄은 1단계에서 믿지 않는다. ppk처럼 OpenSSH가 읽지 못하는 키 형식은 읽지 않는다. 모바일은 앱 저장소의 `ssh/` 폴더를 `~/.ssh`로 본다. 암호 백엔드는 ring이다. `ssh-key =0.7.0-rc.12` 고정을 위험으로 적는다 | 아래 각 절 |
| 2026-11-27 | director 2852: tasty-android는 원격 파일을 SFTP로, Git 정보를 원격 git 명령(exec 채널)으로 가져온다 | [터널·exec·SFTP](#터널execsftp) |

## 백엔드

| 백엔드 | 하는 일 | feature |
|---|---|---|
| 시스템 ssh(`system`) | 지금의 동작이다. 설치된 OpenSSH `ssh`를 자식 프로세스로 띄워 `-L` 터널, 원격 포트 발견, exec, SFTP를 한다 | `system-ssh` |
| 내장(`embedded`) | 프로세스 안 SSH 클라이언트다. 연결·암호·채널은 russh가, 설정·known_hosts 해석은 `tasty-net-ssh-config`가 맡는다. 시스템 ssh와 같은 터널·포트 발견 계약을 지키고, exec와 SFTP도 준다 | `embedded-ssh` |

## feature 계약

[크레이트 구성](crates.md#의존-방향)의 "feature로 기능을 켜고 끄지 않는다"의 예외는 이 계약 하나다. 후속 작업 순서에 따라 두 단계로 들어간다.

### 1단계: transport ssh feature 분리 (내장 백엔드 전)

| 크레이트 | feature | 기본 | 켜는 것 |
|---|---|---|---|
| `tasty-net-ssh` | `system-ssh` | 켬. `default = ["system-ssh"]` | 시스템 ssh 백엔드. `std::process`를 쓰는 코드를 모두 `src/system/` 모듈에 모은다(`SpawnHook`·`SpawnFn`, `SshLauncher`, `resolve_ssh_path`, 자식 생성·등록·회수, 발견 자식, 터널 명령) |
| `tasty-net-ssh` | `system-ssh` 끔 | | `compile_error!("tasty-net-ssh needs the system-ssh or embedded-ssh feature")`. 백엔드 없는 SSH 크레이트는 만들지 않는다 |
| `tasty-net-transport` | `ssh` | 직접 켜지 않는다(내부 feature) | `tasty-net-ssh` 의존(`optional = true`, `default-features = false`)과 SSH 항목. 이것만 켜면 `tasty-net-ssh`가 백엔드 없이 들어와 위 `compile_error!`가 난다. 소비자는 아래 백엔드 feature를 켠다 |
| `tasty-net-transport` | `system-ssh` | 켬. `default = ["system-ssh"]` | `["ssh", "tasty-net-ssh/system-ssh"]` |

- 이 단계의 transport 조합은 기본(`system-ssh`)과 `--no-default-features`(SSH 없음) 둘이다.
- 이 단계에는 내장 백엔드가 없다. 그래서 `ssh.backend` 설정값과 선택 규칙도 아직 없고, 지금처럼 시스템 ssh만 쓴다.
- 모바일 cross check는 transport `--no-default-features`만 본다. ssh 크레이트는 아직 목록에 없다([모바일 cross check](mobile.md#모바일-cross-check)).

### 2단계: 내장 백엔드 (이식 크레이트 뒤)

| 크레이트 | feature | 기본 | 켜는 것 |
|---|---|---|---|
| `tasty-net-ssh` | `system-ssh` | 켬 | 1단계와 같다 |
| `tasty-net-ssh` | `embedded-ssh` | 켬 | 내장 백엔드. 의존: `russh`(정확한 판, `default-features = false`, `features = ["ring", "rsa"]`), `russh-sftp`(정확한 판), `tasty-net-ssh-config`, `tokio`(`rt-multi-thread`·`net`·`time`·`sync`·`io-util`, `process` 없음) |
| `tasty-net-ssh` | 둘 다 끔 | | `compile_error!` |
| `tasty-net-transport` | `system-ssh` | 켬 | 1단계와 같다 |
| `tasty-net-transport` | `embedded-ssh` | 켬 | `["ssh", "tasty-net-ssh/embedded-ssh"]` |

- 기본값은 두 크레이트 모두 `["system-ssh", "embedded-ssh"]`이다. 데스크톱 소비자(tasty)는 기본 그대로 쓰고, OpenSSH가 없는 데스크톱에서도 폴백이 된다.
- 모바일 조합은 `--no-default-features --features embedded-ssh`다. 이 단계에서 cross check에 ssh와 transport의 내장 조합 두 행이 더해진다.
- `tasty-net-ssh-config`는 feature가 없다. `std::process`를 쓰지 않는다.
- 이 단계가 내장 백엔드 작업과 같은 커밋 묶음에서 들어간다. 그 전에 `embedded-ssh`라는 이름의 빈 feature를 만들지 않는다. 빈 feature를 두면 `--no-default-features --features embedded-ssh`가 백엔드 없는 ssh 크레이트가 되어 `compile_error!` 계약과 어긋난다.

### `ssh`가 꺼진 transport에서 사라지는 것

공개 API에서 사라지는 항목:

| 항목 | 지금 자리 | 비고 |
|---|---|---|
| `SshTransport`, `FORWARDED_SEPARATOR`, `LOCAL_CONNECT_TIMEOUT` | `src/ssh.rs`, `src/lib.rs` 다시 내보내기 | |
| `AttemptCancel::ssh_cancel` | `src/cancel.rs:93` | |
| `ConnectError::Discovery(PortDiscoveryError)`, `ConnectError::Tunnel(TunnelError)` | `src/transport.rs:200`·`:202` | `ConnectError`는 `#[non_exhaustive]`(`:193`) |
| `ConnectError::Backend(SshBackendError)` | 2단계에서 새로 생긴다 | 같다 |
| `HandshakeError::RemoteListenerAbsent(RemoteListenerAbsent)` | `src/reject.rs:226` | `HandshakeError`는 `#[non_exhaustive]`(`:185`) |
| 연결 목록 `Cause::Discovery(PortDiscoveryFailureKind)` | `src/connection_table/info.rs:153` | `Cause`는 `#[non_exhaustive]`(`:151`) |
| `ConnectionInfo.ssh` 필드의 타입 `SshEntryInfo` | 2단계에서 새로 생긴다 | 아래 [연결 정보](#연결-정보) |

- 세 enum이 모두 `#[non_exhaustive]`라서 소비자의 `match`는 어느 조합에서도 `_` 갈래를 이미 갖는다. 변형이 사라져도 소비자 코드는 그대로 컴파일된다.
- 원인 코드 문자열 `discovery_failed`(`info.rs:194`)는 `ssh`가 꺼진 빌드에서 나오지 않을 뿐 다른 뜻으로 다시 쓰지 않는다. 연결 목록 타입에는 serde 직렬화가 없다(transport의 `connection_table`에 `Serialize` 0건). 외부 표현은 `Cause::as_str()`의 코드뿐이다.
- FFI 모양: `tasty-net-ffi`는 늘 `embedded-ssh`로 빌드하므로([모바일 바인딩 층](mobile.md#바인딩-층)) FFI에 보이는 변형은 조합에 따라 달라지지 않는다. SSH 없는 transport는 Rust 소비자만 쓰는 조합이다.
- 남는 공개 항목: `Transport`·`Listener`·`RawChannel`·`Link`, handshake v2, 메모리 전송, P2P 어댑터, `ConnectionTable`과 그 설정값, `Redialer`, `Phase`의 `PortDiscovery`·`Tunnel`, `PathKind`의 `ssh_forward`·`ssh_role_reversed`·`ssh_inbound`, `Cause::TunnelFailed`·`TunnelExited`·`RemoteListenerAbsent`(SSH 타입을 싣지 않는 unit·문자열 변형), `Via::Ssh{target, remote_port}`. 받는 쪽 `ssh_inbound`는 꺼진 빌드에서도 생긴다(상대가 SSH로 연 연결을 루프백 listener로 받는다).
- 2단계에서 더하는 unit 원인 `Cause::SshBackendUnavailable`(`ssh_backend_unavailable`)과 `Cause::SshConfigRefused`(`ssh_config_refused`)도 SSH 타입을 싣지 않으므로 조합과 관계없이 남는다.

내부 항목의 처리:

| 항목 | `ssh`가 꺼진 빌드 |
|---|---|
| `RawChannel.verdict: Option<ListenerVerdict>`(`src/channel.rs:63`), `with_verdict`(`:179`), `listener_absent`(`:186`–`:192`) | `#[cfg(feature = "ssh")]`로 뺀다. 판정을 쓰는 곳은 SSH 채널뿐이다 |
| `handshake.rs` `explain()`(`:612`–`:625`)의 `tasty_net_ssh::connection_closed_error` 호출 | 그 함수는 `io::Error::new(UnexpectedEof, …)` 하나다(`tasty-net-ssh/src/listener.rs:118`). `explain`의 SSH 판정 갈래만 cfg로 빼고, 꺼진 빌드에서는 받은 오류를 그대로 돌려준다. 닫힘 오류 자체는 handshake가 직접 만든다 |
| 통합 시험 `tests/ssh.rs`(`:18`에서 `SpawnHook`·`SshLauncher` 사용) | 파일 첫 줄 `#![cfg(feature = "system-ssh")]` |
| 통합 시험 `tests/connection_table.rs`(`:7`에서 `tasty_net_ssh` 사용) | SSH 항목을 만드는 도우미와 시험에 `#[cfg(feature = "system-ssh")]`. 메모리 전송 시험은 남긴다. `cargo test -p tasty-net-transport --no-default-features`가 SSH 없이 돈다 |

### 공통 항목과 백엔드 전용 항목

| `tasty-net-ssh` 항목 | 있는 조합 |
|---|---|
| `SshTarget`, `SshSettings`와 설정값, `Backoff`, `SshTunnel`, `SshCancel`, `ListenerVerdict`·`RemoteListenerAbsent`, `PortDiscoveryError`·`PortDiscoveryFailureKind`, `TunnelError`, `connection_closed_error`, `SshConnector`, `SshSession`, `SshExec`·`ExitStatus`, `SshBackendKind`, `SelectedBackend`, `BackendReason`, `SshBackendError`, `SshPrompt` | 백엔드가 하나라도 있으면 늘 |
| `SshLauncher`, `SpawnHook`·`SpawnFn`, `resolve_ssh_path` | `system-ssh` |
| `ResolvedTarget`, `ConfigNote`의 해석 결과 채우기, `SessionHook`(시험 훅) | `embedded-ssh`. 타입은 공통이고 시스템 백엔드에서는 비어 있다 |
| `SftpClient`, `SftpError` | `embedded-ssh`(russh-sftp가 이 feature로 들어온다). 그 빌드에서는 두 백엔드가 모두 SFTP를 준다 |

- 위 표는 2단계 기준이다. 1단계에는 지금의 항목과 `system-ssh` 전용 항목만 있다(`SshConnector`·`SshSession`·exec·SFTP·선택 타입은 2단계에서 생긴다).

## 진입점

- `SshConnector::new(settings: SshSettings) -> SshConnector`: 백엔드를 고르고 터널·포트 발견·exec·SFTP를 연다.
- `SshConnector::with_launcher(self, launcher: SshLauncher) -> SshConnector`(`system-ssh`): 시스템 백엔드의 프로그램·자식 생성 훅·`with_verify`를 준다. 커넥터의 설정이 런처의 설정을 이긴다(런처가 가진 `SshSettings`는 쓰지 않는다).
- `SshConnector::with_prompt(self, prompt: Arc<dyn SshPrompt>) -> SshConnector`: passphrase·비밀번호·keyboard-interactive·호스트 키 확인 질문을 받을 콜백.
- `SshConnector::select(&self, target: &SshTarget) -> Result<SelectedBackend, SshBackendError>`: 지금 판정하면 고를 백엔드와 이유. 연결하지 않는다.
- `SshConnector::settings(&self) -> &SshSettings`: 설정 조회(`entries()`가 `{key, value, source}`).
- transport:
  - `SshTransport::from_connector(connector: SshConnector, discovery: PortDiscoveryConfig, mode: PortMode) -> SshTransport`가 새 생성자다. 포트 발견 설정과 포트 방식은 지금처럼 transport 인자로 남는다.
  - 지금의 `SshTransport::new(launcher: SshLauncher, discovery: PortDiscoveryConfig, mode: PortMode)`(`src/ssh.rs:138`)는 `system-ssh`에서만 있다. 뜻은 `SshTransport::from_connector(SshConnector::new(launcher.settings().clone()).with_launcher(launcher), discovery, mode)`다. 기존 호출은 그대로 컴파일되고 동작도 같다(`ssh.backend`의 기본 `auto`가 그 런처의 프로그램을 찾으면 시스템을 고른다).

## 선택 규칙

설정값 `ssh.backend`가 정한다. 값은 `auto`(기본)·`system`·`embedded`다. 2단계부터 있다.

| `ssh.backend` | 빌드 | 고르는 백엔드 | 이유(`BackendReason`) |
|---|---|---|---|
| `auto` | 둘 다 | OpenSSH 클라이언트 파일을 찾으면 시스템, 못 찾으면 내장 | `found{path}` / `not_found{searched}` |
| `auto` | `embedded-ssh`만 | 내장 | `system_not_built` |
| `auto` | `system-ssh`만 | 찾으면 시스템. 못 찾으면 연결 시도가 `SshBackendError::NotFound{searched}`로 끝난다(transport는 `ConnectError::Backend`, 원인 `ssh_backend_unavailable`). 조용히 다른 길로 가지 않는다 | `found{path}` / `not_found{searched}` |
| `system` | `system-ssh` 있음 | 시스템. 못 찾으면 위와 같은 `NotFound`. 내장으로 넘어가지 않는다 | `configured` |
| `embedded` | `embedded-ssh` 있음 | 내장 | `configured` |
| 빌드에 없는 백엔드 이름 | | `SshSettings::set`이 `SettingsError`(범위 밖)로 거절한다 | |

- OS·플랫폼으로 나누지 않는다. 모바일은 `system-ssh` 없이 빌드하므로 `system_not_built`로 내장을 고른다.
- 발견은 다른 프로그램을 실행하지 않고 파일만 본다(`ssh -V`를 돌리지 않는다).
  1. 런처의 프로그램(`SshLauncher::program()`, 기본은 `resolve_ssh_path()`)이 경로 구분자를 가지면 그 경로 하나를 본다. Windows의 `%WINDIR%\System32\OpenSSH\ssh.exe` 우선(`tasty-net-ssh/src/lib.rs:63`–`:72`)은 기본 프로그램을 정하는 기존 규칙이다.
  2. 구분자가 없는 이름이면 `PATH`의 디렉터리를 앞에서부터 본다. Unix는 그 이름의 일반 파일에 실행 권한 비트가 있어야 한다. Windows는 디렉터리마다 `<이름>`, 이름에 확장자가 없으면 이어서 `<이름>.exe`를 본다.
  3. 시스템 백엔드는 발견한 절대 경로로 실행한다. 그래서 선택이 본 파일과 실행한 파일이 같다(Windows `Command`가 실행 파일 폴더·System32를 먼저 보는 규칙을 타지 않는다).
  4. 찾은 파일이 OpenSSH인지는 가리지 않는다. 같은 이름의 다른 클라이언트가 먼저 있으면 그것이 시스템 백엔드가 되고, 그 클라이언트가 옵션을 거절하면 시스템 백엔드의 오류로 끝난다. 알려진 한계다.
- 고르는 때: 연결 시도마다 한 번 고르고, 그 시도의 포트 발견과 터널은 같은 선택을 쓴다(발견은 시스템, 터널은 내장처럼 갈리지 않는다). 다음 시도는 다시 고르므로 실행 중에 OpenSSH를 설치하거나 지운 것이 반영된다. 이미 연 터널·세션은 처음 고른 백엔드로 끝까지 간다.
- 시스템 백엔드가 실패해도 내장으로 넘어가지 않는다. 폴백은 "찾지 못함"일 때만이다. 인증 실패나 호스트 키 거절을 다른 구현으로 다시 시도하면 사용자가 정한 동작과 다른 결과가 날 수 있다.

## 연결 정보

### `tasty-net-ssh` 쪽

```text
enum SshBackendKind { System, Embedded }
enum BackendReason { Found { path }, NotFound { searched: Vec<PathBuf> }, SystemNotBuilt, Configured }
struct SelectedBackend { backend: Option<SshBackendKind>, reason: BackendReason, selected_at: SystemTime }
struct ResolvedTarget {            // 내장 백엔드만. 시스템 백엔드는 None
    hostname, user, port, identity_files: Vec<PathBuf>, proxy_jump: Vec<JumpHop>,
    sources: Vec<ValueSource>,     // 값마다 {keyword, value, source}, source는 아래 "옵션 적용 순서"
}
struct ConfigNote { keyword, file, line, action: Shown | Refused | Ignored, class: NoteClass }
```

- `backend`가 `None`이면 고르지 못한 것이다(`NotFound`).
- `SshTunnel::backend()`, 포트 발견 결과의 `backend`, `SshSession::backend()`가 `SelectedBackend`를 돌려준다. `SshTunnel::resolved()`와 `config_notes()`가 해석 결과와 지시어 표시를 돌려준다.
- `ConfigNote`의 값 칸은 싣지 않는다(비밀 값이 들 수 있다). 지시어 이름·파일·줄·처리·분류만 싣는다.

### transport 연결 목록

| 필드 | 값 | 채우는 때 |
|---|---|---|
| `via` | 지금 그대로 `ssh{target, remote_port}`([전송 추상화](transport.md#필드)) | 지금과 같다 |
| `applied` | 지금 그대로 설정값만 싣는다(`{key, value, source}`). 2단계에서 그 연결에 쓰인 `ssh.backend`와 `embedded.*` 키가 더해진다 | 지금과 같다 |
| 새 필드 `ssh: Option<SshEntryInfo>` | `SshEntryInfo { backend: SelectedBackend, resolved: Option<ResolvedTarget>, config_notes: Vec<ConfigNote>, session_id: Option<u64> }`. SSH를 띄운 쪽 항목만 `Some`이다. 받는 쪽 `ssh_inbound`와 P2P 항목은 `None`이다(루프백으로 받은 TCP라 백엔드를 모른다) | 백엔드를 고른 직후, 포트 발견과 터널보다 먼저 채운다 |

- 실패한 시도: 고르지 못했거나(`NotFound`) 설정 해석이 거부되면(`Refused`) 항목은 네트워크에 나가기 전에 `disconnected`로 끝난다. 원인은 `ssh_backend_unavailable`·`ssh_config_refused`이고, `ssh` 필드에 이유·본 경로·거부한 지시어가 남는다. 포트 발견이나 터널이 실패한 항목도 `ssh` 필드는 그 시도의 선택을 그대로 갖는다.
- `session_id`는 내장 백엔드 세션 번호다(프로세스 안에서 1부터). 같은 공유 키의 터널이 세션을 다시 열면 바뀐다. 시스템 백엔드는 `None`이다.
- 1단계(내장 전)에는 이 필드가 없다. 2단계에서 [전송 추상화](transport.md#필드)의 필드 표에 이 행을 더한다.

## 옵션 적용 순서

같은 지시어가 여러 곳에 있으면 먼저 얻은 값이 이긴다(OpenSSH의 첫 값 우선, `ssh.c`가 명령행 `-o`·`-p`·`-i`를 설정 파일보다 먼저 읽는다). 두 백엔드가 같은 순서를 쓴다. 시스템 백엔드는 이 순서대로 `ssh` 인자를 만들고, 내장 백엔드는 같은 순서로 이식 크레이트에 값을 넣는다.

| 순서 | 출처 | 값 | `source` |
|---|---|---|---|
| 1 | 런처 고정 | `BatchMode=no` | `forced` |
| 2 | 설정값 | `ServerAliveInterval`·`ServerAliveCountMax`(`ssh.server_alive_*`) | `setting:<키>` |
| 3 | 확인 모드 | `with_verify`면 `StrictHostKeyChecking=accept-new` | `forced` |
| 4 | 터널 고정 | `ExitOnForwardFailure=yes`, `ControlMaster=no`, `ControlPath=none`, `ClearAllForwardings=no`와 fork 금지 옵션(`tunnel.rs:91`) | `forced` |
| 5 | 대상 | `SshTarget`의 포트(`-p`), 키 파일(`-i`) | `target` |
| 6 | 대상 | `SshTarget.extra_options` | `target` |
| 7 | 설정값 | `ssh.overrides`(`지시어=값` 목록) | `override` |
| 8 | 설정값 | `ConnectTimeout`(`ssh.connect_timeout`) | `setting:ssh.connect_timeout` |
| 9 | 사용자 설정 파일 | `~/.ssh/config` 또는 `embedded.config_file` | `file:<경로>:<줄>` |
| 10 | 시스템 설정 파일 | `/etc/ssh/ssh_config` 등 | `file:<경로>:<줄>` |
| 11 | OpenSSH 기본값 | `fill_default_options`(`readconf.c:2860`)의 값 | `default` |

- 1~4·6·8은 지금의 `push_common_opts` 순서(`tasty-net-ssh/src/lib.rs:126`–`:170`)와 같다. 바뀌는 것은 7(`ssh.overrides`)이 더해지는 것뿐이다. 대상 옵션이 전역 덮어쓰기를 이기고, 덮어쓰기가 `ssh.connect_timeout`을 이긴다.
- 그래서 `ssh.overrides`나 대상 옵션으로 `StrictHostKeyChecking`을 약하게 만들 수 없다(3이 먼저다). `StrictHostKeyChecking no`가 어디서 오든 내장 백엔드는 거부한다([지시어 분류](#지시어-분류)).
- 누적 지시어(`IdentityFile`·`CertificateFile`·`SendEnv`·`SetEnv`·`LocalForward` 등)는 첫 값 우선이 아니라 순서대로 쌓인다(OpenSSH와 같다).
- `ConnectionAttempts`·`NumberOfPasswordPrompts`처럼 설정값으로 두지 않은 동작값은 지시어로만 정해지고, 없으면 OpenSSH 기본값(`ConnectionAttempts` 1 `readconf.c:2922`, `NumberOfPasswordPrompts` 3 `:2924`)이다. 연결 정보의 `resolved.sources`에서 출처와 함께 보인다. 라이브러리 설정값 규칙의 "조회"는 이 경로로 채운다.
- 점프 호스트: OpenSSH는 `ProxyJump`를 `ssh -l <사용자> -p <포트> -J <나머지> -F <설정 파일> -W '[%h]:%p' <홉>` 명령으로 바꾼다(`ssh.c:1278`–`:1324`). 홉에는 `-o` 옵션이 하나도 넘어가지 않는다. 내장 백엔드도 같다: 홉은 같은 설정 파일로 자기 `Host`·`Match`를 해석하고, `ProxyJump` 문자열의 사용자·포트만 받는다. 위 1~8은 최종 목적지에만 적용된다. 홉의 해석 결과는 `resolved.proxy_jump`에 홉마다 있다.

## 터널·exec·SFTP

### 내장 백엔드: 세션 하나에 채널 여럿

- 세션의 단위는 transport의 공유 키(대상 해석 결과)다. 같은 공유 키의 터널, exec, SFTP는 SSH 세션 하나를 나눠 쓴다. 세션은 그 위 핸들(터널·`SshExec`·`SftpClient`)이 하나라도 살아 있는 동안 유지되고, 마지막 핸들이 닫히면 닫힌다. 세션을 붙들어 두는 시간은 두지 않는다(필요하면 android 요구와 함께 정한다, [후속 작업](#후속-작업과-미정)).
- 채널 종류:

  | 용도 | SSH 채널 | russh 0.65.4 |
  |---|---|---|
  | 터널 | 로컬 루프백 포워딩 종단(`127.0.0.1:<포트>`)에 온 TCP 연결마다 `direct-tcpip` | `channel_open_direct_tcpip` |
  | 포트 발견 | `session` 채널 + `exec`(지금과 같은 원격 명령), stdout·stderr·종료 상태를 지금의 분류 규칙(`discovery.*`)으로 읽는다 | `Channel::exec`(`channels/mod.rs:237`·`:542`) |
  | exec | `session` 채널 + `exec`. 표준 입력 쓰기, stdout·stderr 읽기, `exit-status`·`exit-signal` | 같다, `ChannelMsg::ExitStatus`(`:95`) |
  | SFTP | `session` 채널 + `subsystem sftp`, 그 채널 스트림 위에 russh-sftp | `Channel::request_subsystem`(`:251`·`:552`), `Channel::into_stream` |

- 터널의 수명·공유 키·`segments.local_forward` 구간은 두 백엔드가 같다. 루프백 포트를 거치지 않는 프로세스 안 스트림은 이 계약 밖의 후속 최적화다.
- 세션이 끊기면 그 위 채널이 모두 끝난다. 터널은 `TunnelExited`, 실행 중인 exec는 `ExitStatus::SessionLost`, SFTP는 `SftpError::SessionLost`다. transport 재연결은 새 세션을 연다(`session_id`가 바뀐다).
- 서버가 채널 열기를 거절하면(`SSH_MSG_CHANNEL_OPEN_FAILURE`, 예: sshd `MaxSessions`) 그 채널만 `ChannelOpenRefused{code, description}`으로 실패하고 세션은 남는다.
- 리스너 판정: 시스템 백엔드는 로컬 연결 끊김과 ssh stderr의 `open failed: connect failed` 줄이 함께 있을 때만 `RemoteListenerAbsent`로 본다(`tasty-net-ssh/src/listener.rs:110`–`:114`). 내장 백엔드는 `direct-tcpip` 열기 실패의 사유 코드가 2(`SSH_OPEN_CONNECT_FAILED`)일 때만 같은 판정을 내고, 그 로컬 연결을 닫는다. 코드 1(`ADMINISTRATIVELY_PROHIBITED`)·3(`UNKNOWN_CHANNEL_TYPE`)·4(`RESOURCE_SHORTAGE`)는 리스너 없음이 아니라 `ChannelOpenRefused`로 handshake의 연결 실패가 된다.

### 시스템 ssh 백엔드: 기능마다 자식 하나

| 용도 | 자식 명령 | 결과 |
|---|---|---|
| 터널 | 지금 그대로 `ssh -N -L …` | 지금과 같다 |
| 포트 발견 | 지금 그대로 `ssh <대상> <명령>` | 지금과 같다 |
| exec | `ssh <공통 옵션> <대상> -- <명령>`. 표준 입출력은 자식의 파이프 | 자식의 종료 코드가 0~254면 `ExitStatus::Code(n)`. 255는 원격 명령의 255와 ssh 자체 오류를 가를 수 없어 `ExitStatus::Ambiguous255`(stderr를 함께 준다). 신호 종료는 ssh가 255로 끝나므로 신호 이름을 주지 못한다 |
| SFTP | `ssh <공통 옵션> -s <대상> sftp`. 자식의 stdin·stdout을 tokio 쪽 스트림으로 잇고(파이프를 읽고 쓰는 스레드 둘, tokio `process` feature는 쓰지 않는다) 그 위에 russh-sftp `SftpSession::new(stream)` | 내장과 같은 `SftpClient` |

- 공통 옵션은 위 [옵션 적용 순서](#옵션-적용-순서)의 1~3·5~8이다. 4의 터널 고정 옵션은 터널 자식에만 붙인다.
- `ControlMaster=no`를 강제하므로 기능마다 SSH 세션과 인증이 따로다. 키에 passphrase가 있으면 기능마다 묻는다(agent가 있으면 묻지 않는다). 내장 백엔드와 다른 점으로 둔다.
- russh-sftp는 russh에 의존하지 않고 아무 `AsyncRead + AsyncWrite` 스트림을 받는다(아래 [의존 사실](#의존-사실)). 그래서 두 백엔드가 같은 SFTP 구현을 쓴다. 시스템 백엔드의 SFTP는 `system-ssh`와 `embedded-ssh`가 함께 켜진 빌드에서만 russh-sftp가 들어온다. `system-ssh`만 켠 1단계 빌드에는 SFTP가 없다. 2단계 기본 빌드는 둘 다 켜므로 데스크톱 소비자는 두 백엔드에서 SFTP를 쓴다. 그래서 `SftpClient`는 `embedded-ssh`가 켜진 빌드에만 있다([공통 항목과 백엔드 전용 항목](#공통-항목과-백엔드-전용-항목)).

### 공개 API (`tasty-net-ssh`)

```text
SshConnector::session(&self, target: &SshTarget) -> Result<SshSession, SshBackendError>
SshSession::exec(&self, command: &str, options: ExecOptions) -> Result<SshExec, ExecError>
SshExec::stdin(&mut self) -> &mut dyn Write;  stdout()/stderr() -> &mut dyn Read
SshExec::wait(self, timeout: Option<Duration>) -> Result<ExitStatus, ExecError>
SshExec::cancel(&self)                        // 내장: 채널 닫기, 시스템: 자식 종료
enum ExitStatus { Code(u32), Signal { name, core_dumped, message }, Ambiguous255 { stderr_tail }, SessionLost, NoStatus }
SshSession::sftp(&self) -> Result<SftpClient, SftpError>          // embedded-ssh 빌드
SftpClient: open / create / read / write / read_dir / metadata / set_metadata / rename / remove_file / remove_dir / create_dir / canonicalize / close
```

- 동기 API다. 막히는 호출은 내장 백엔드의 소유 런타임에서 돌고 호출자는 결과를 기다린다([전송 추상화](transport.md#동기비동기-경계)).
- `ExitStatus::NoStatus`는 서버가 `exit-status`·`exit-signal` 없이 채널을 닫은 경우다(RFC 4254 §6.10은 보내는 것을 SHOULD로 둔다).
- `ExecOptions`: 환경 변수(`SendEnv`·`SetEnv`와 같은 `env` 요청), 표준 입력 닫기, 출력 버퍼 상한(`exec.output_buffer`).
- 바인딩 API(Kotlin·Swift에 무엇을 낼지)는 tasty-android의 요구 목록을 받아 정한다([모바일 바인딩 층](mobile.md#바인딩-층)).

## OpenSSH 해석 이식 크레이트

새 크레이트 `tasty-net-ssh-config`(`crates/tasty-net-ssh-config`)가 OpenSSH의 설정 해석과 known_hosts 해석을 Rust로 옮긴 코드를 갖는다.

### 기준 판

| 항목 | 값 |
|---|---|
| 판 | OpenSSH portable 10.6p2 |
| 태그 | `V_10_6_P2`(태그 객체 `e9c0339ed598bd96aa814be2c776b6c3c2357ab4`) |
| 커밋 | `d449e662b6b8b8cb20e852d8084c269af1c5e8f1`(2026-11-05, `version.h` `OpenSSH_10.6`) |
| 저장소 | `https://github.com/openssh/openssh-portable` |

- 기준 판을 올리는 일은 그 판의 원본 차이를 이식 크레이트에 옮기고, 비교 시험을 그 판의 클라이언트로 다시 돌리는 일이다. 근거 보고서는 master `8038f615`의 줄 번호를 썼다. 이 문서의 줄 번호는 위 커밋 기준이다.

### 옮기는 원본

| 원본 파일(줄 수) | 옮기는 것 |
|---|---|
| `readconf.c`(3968), `readconf.h`(268) | 키워드 표 `keywords[]`(`:178`–`:325`), 줄 해석 `process_config_line_depth`(`:1143`), `Match` 판정 `match_cfg_line`(`:700`), 파일 읽기 `read_config_file_depth`(`:2631`, 상한 `READCONF_MAX_DEPTH` `:2629`), `initialize_options`(`:2714`), `fill_default_options`(`:2860`), `parse_forward`(`:3299`), `parse_jump`(`:3474`), `dump_client_config`(`:3739`, 비교 시험의 출력 형식) |
| `match.c`(406) | `match_pattern`(`:82`), `match_pattern_list`(`:161`), `match_hostname`(`:233`), `match_host_and_ip`(`:250`), `match_user`(`:275`), `match_list`(`:313`), `match_filter_denylist`·`allowlist`(`:393`·`:403`) |
| `hostfile.c`(982) | `extract_salt`(`:69`), `host_hash`(`:116`), `check_markers`(`:181`), `load_hostkeys_file`(`:266`), `check_key_not_revoked`(`:316`), `check_hostkeys_by_key_or_type`(`:350`), `check_key_in_hostkeys`(`:408`), `lookup_key_in_hostkeys_by_type`(`:417`), `lookup_marker_in_hostkeys`(`:425`), `add_host_to_hostfile`(`:542`), `hostkeys_foreach_file`(`:790`), `hostkeys_foreach`(`:966`) |
| `misc.c`(3352)의 해당 함수 | `a2port`(`:592`), `convtime`(`:746`), `parse_user_host_path`(`:914`), `tilde_expand`(`:1276`)·`tilde_expand_filename`(`:1344`), `dollar_expand`(`:1477`), `percent_expand`(`:1496`)·`percent_dollar_expand`(`:1515`), `argv_split`(`:2143`), `valid_env_name`(`:2532`) |
| `kex-names.c`(334), `myproposal.h`(124) | `kex_assemble_names`(`:241`, `+`·`-`·`^` 연산), 기본 알고리즘 목록 |
| `sshconnect.c`(1809)·`sshconnect2.c`의 판정 부분 | `check_host_key`(`:984`)의 known_hosts 판정 순서, `verify_host_key`(`:1499`)의 `RevokedHostKeys` 처리(`:1544`–`:1563`), `order_hostkeyalgs`(`sshconnect2.c:121`, known_hosts에 있는 키 형식을 앞으로) |

- 옮기는 것은 해석과 판정이다. 소켓·암호·서명 검증·프로세스 실행은 옮기지 않는다(그 자리는 russh와 거부 규칙이다).
- 키 비교는 공개키 blob 바이트로 한다. known_hosts 줄의 base64를 풀어 서버가 보낸 blob과 비교하므로 키 형식 크레이트에 의존하지 않는다. 의존은 `hmac`·`sha1`(해시 호스트 `|1|`), `base64`다.
- 다른 프로그램을 실행하지 않는다. `Match exec`·`ProxyCommand`·`KnownHostsCommand`·`LocalCommand`는 해석만 하고, 적용되면 거부 결과를 돌려준다.

### 결과 API

```text
ConfigSet::load(inputs: ConfigInputs) -> Result<ConfigSet, ConfigError>   // 파일 읽기, Include 펼치기
ConfigSet::resolve(&self, host: &str, ctx: ResolveContext) -> Resolution
Resolution { options: Options, sources: Vec<ValueSource>, notes: Vec<ConfigNote>, refusal: Option<Refusal> }
KnownHosts::load(files: &[PathBuf]) -> KnownHosts
KnownHosts::check(&self, host: &HostKeyQuery, key_blob: &[u8]) -> HostKeyVerdict
enum HostKeyVerdict { Match { file, line }, Mismatch { file, line }, Revoked { file, line }, NotFound, DistrustedCaLine { file, line } }
```

- `ResolveContext`는 명령행에 해당하는 값(위 순서 1~8), 로컬 사용자, 홈, `ssh/` 폴더, 지금 판정 중인 단계(`final` 여부)를 갖는다.
- `Refusal`이 있으면 내장 백엔드는 네트워크에 나가기 전에 `SshBackendError::ConfigRefused`로 끝낸다.

### 라이선스

- `Cargo.toml`: `license = "BSD-2-Clause AND ISC"`. 크레이트 폴더에 `LICENSE-OpenSSH`(원본 `LICENCE` 전문, 412줄)를 둔다.
- 옮긴 파일마다 원본 파일의 저작권 고지를 그대로 두고 `derived from OpenSSH, modified`를 적는다. 원본 파일과 함수를 모듈 문서에 적는다.
- 나머지 크레이트는 MIT 그대로다. `deny.toml`의 허용 목록에 `BSD-2-Clause`와 `ISC`가 이미 있다.
- Tasty·Android 앱의 라이선스 고지에 OpenSSH 고지를 넣는 일은 앱 쪽 전환 작업이다.

## 내장 백엔드가 읽는 파일

OpenSSH와 같은 형식을 같은 뜻으로 읽는다(이식 크레이트). 위치 기본값도 OpenSSH와 같다.

| 파일 | 기본 위치 | 형식 |
|---|---|---|
| 사용자 설정 | `<ssh 폴더>/config` | `ssh_config` |
| 시스템 설정 | Unix `/etc/ssh/ssh_config`, Windows `%PROGRAMDATA%\ssh\ssh_config` | 같다. 사용자 설정 다음에 읽는다 |
| 사용자 known_hosts | `<ssh 폴더>/known_hosts`, `known_hosts2` | 평문·와일드카드·부정·`[호스트]:포트`·해시(`|1|`)·`@revoked`. `@cert-authority` 줄은 믿지 않는다([호스트 키 판정](#호스트-키-판정)) |
| 시스템 known_hosts | Unix `/etc/ssh/ssh_known_hosts`, `ssh_known_hosts2`, Windows `%PROGRAMDATA%\ssh\` 아래 | 같다 |
| 기본 키 파일 | `<ssh 폴더>/id_rsa`, `id_ecdsa`, `id_ecdsa_sk`, `id_ed25519`, `id_ed25519_sk` | OpenSSH 개인키 형식, PEM(PKCS#1·SEC1·PKCS#8, 암호 걸린 PKCS#8과 PKCS#5 RSA). 읽기는 russh가 한다. PuTTY `ppk`처럼 OpenSSH가 읽지 못하는 형식은 읽지 않는다: russh `decode_secret_key`가 ppk를 기본으로 받으므로(`keys/format/mod.rs:45`) 그 함수를 부르기 전에 `PuTTY-User-Key-File-` 머리를 보고 건너뛰고 `ConfigNote`(`unsupported_key_format`)로 남긴다 |
| ssh-agent | `IdentityAgent`, 없으면 `SSH_AUTH_SOCK`. Windows는 `\\.\pipe\openssh-ssh-agent`와 Pageant | russh `AgentClient`. 소켓이 없으면 agent 인증을 건너뛰고 `ConfigNote`로 남긴다 |

- `<ssh 폴더>`는 설정값 `embedded.ssh_dir`이다. 기본은 `~/.ssh`(`~`는 `embedded.home`)다. 모바일은 바인딩 층이 앱 저장소의 `ssh/` 폴더를 `configured`로 넣는다(tasty 설정으로 덮어쓸 수 있다).
- `~` 펼침: 설정 파일 안의 `~/.ssh/` 로 시작하는 경로는 `<ssh 폴더>/`로, 그 밖의 `~/`는 `embedded.home`으로 펼친다. `embedded.ssh_dir`이 기본값이면 OpenSSH와 같다. 홈을 알 수 없고 설정도 없으면 `~`가 든 경로는 없는 파일로 다룬다.

## 지시어 분류

OpenSSH 10.6p2의 `keywords[]`에는 132개 항목이 있다(별칭 포함). 이식 크레이트는 그 전부를 알고 항목마다 아래 처리 하나를 갖는다. 지시어 이름은 대소문자를 가리지 않는다. 값의 대소문자는 OpenSSH가 그 지시어에서 하는 그대로다(예: `yes`·`no` 같은 다중 값은 대소문자 무시, 경로·명령은 구별, `Host` 패턴은 소문자로 바꿔 비교).

| 처리 | 뜻 | `ConfigNote.action` |
|---|---|---|
| 지원 | OpenSSH와 같은 뜻으로 적용한다 | 없음 |
| 읽었지만 미지원 | 값을 읽고 적용하지 않는다. 빠져도 고르는 호스트·사용자·포트·키가 같고 호스트 키 판정이 같거나 더 엄격한 지시어다 | `shown` |
| 보안 관련 미지원 | 적용되면 그 연결을 네트워크에 나가기 전에 거부한다 | `refused` |
| OpenSSH가 버린 지시어 | OpenSSH도 적용하지 않는다(`oDeprecated` 8개·`oUnsupported`·`oIgnore`, `readconf.c:2580`·`:2586`) | `shown`(`oIgnore`는 `ignored`) |
| 모르는 지시어 | OpenSSH처럼 거부한다(`Bad configuration option`, `readconf.c:970`). `IgnoreUnknown` 패턴에 든 이름만 `ignored` | `refused` / `ignored` |

- 보안 관련 거부는 `IgnoreUnknown`이 무를 수 없다. `IgnoreUnknown`은 OpenSSH처럼 모르는 이름에만 적용된다.
- 거부는 적용될 때만이다. 다른 `Host` 블록에 있어 이 연결에 적용되지 않는 지시어는 거부하지 않는다. 판정은 [옵션 적용 순서](#옵션-적용-순서)로 유효 값을 정한 뒤에 한다(예: `ProxyJump`에 밀려 쓰이지 않는 `ProxyCommand`는 거부하지 않는다).

### 지원

| 지시어 | 비고 |
|---|---|
| `Host`, `Match`(`all`·`canonical`·`final`·`host`·`originalhost`·`user`·`localuser`·`tagged`·`version`·`sessiontype`·`command`, 부정 `!`), `Include`, `Tag`, `IgnoreUnknown` | `final`은 OpenSSH처럼 설정을 한 번 더 읽는다. `sessiontype`은 터널 `none`, exec·포트 발견 `exec`, SFTP `subsystem`으로 판정한다. `command`는 exec의 명령, `version`은 기준 판의 `OpenSSH_10.6`으로 판정한다. `Include` 중첩 상한은 고정값([설정값](#설정값)) |
| `Hostname`, `User`, `Port`, `HostKeyAlias` | |
| `IdentityFile`, `IdentitiesOnly`, `CertificateFile`, `IdentityAgent` | |
| `PreferredAuthentications`, `PasswordAuthentication`, `KbdInteractiveAuthentication`(별칭 `ChallengeResponseAuthentication`·`SkeyAuthentication`·`TisAuthentication`), `KbdInteractiveDevices`, `NumberOfPasswordPrompts`, `BatchMode` | `BatchMode yes`면 `SshPrompt`를 부르지 않는다 |
| `PubkeyAuthentication`(별칭 `DSAAuthentication`) | `yes`·`no`·`unbound`를 지원한다. `yes`는 OpenSSH에서 서버가 주면 host-bound 인증을 쓰지만 russh 0.65.4에는 host-bound 인증이 없다(`publickey-hostbound` 검색 0건, 대조: 같은 소스에서 `ext-info-c` 1건). 그래서 `yes`는 unbound로 인증하고 `shown`으로 남긴다. `host-bound`는 거부 |
| `StrictHostKeyChecking` | `yes`, `ask`(`SshPrompt`가 없거나 `BatchMode yes`면 `yes`), `accept-new`. `no`·`off`는 거부 |
| `UserKnownHostsFile`, `GlobalKnownHostsFile`, `CheckHostIP`, `HashKnownHosts` | |
| `RevokedHostKeys` | 공개키 목록 파일. 파일이 KRL 바이너리면 거부. 파일이 없거나 읽지 못하면 OpenSSH처럼 호스트 인증을 실패시킨다(`sshconnect.c:1544`–`:1563`) |
| `RequiredRSASize` | 기본 1024(`readconf.c:3032`, `SSH_RSA_MINIMUM_MODULUS_SIZE`). 그보다 작은 RSA 호스트 키는 연결을 끊고, 작은 RSA 사용자 키는 건너뛴다(`sshconnect2.c:96`·`:1593`·`:1847`) |
| `HostKeyAlgorithms`, `KexAlgorithms`, `Ciphers`, `MACs`, `PubkeyAcceptedAlgorithms`(별칭 `PubkeyAcceptedKeyTypes`), `CASignatureAlgorithms` | [알고리즘](#알고리즘) |
| `RekeyLimit` | russh `Limits`의 범위로 자른다([설정값](#설정값)) |
| `ConnectTimeout`, `ConnectionAttempts`, `ServerAliveInterval`, `ServerAliveCountMax`, `TCPKeepAlive`(별칭 `KeepAlive`), `AddressFamily`, `BindAddress` | |
| `ProxyJump` | 여러 단계와 `none`. 앞 단계 세션의 `direct-tcpip` 채널 위에 다음 세션을 연다. 단계마다 호스트 키를 확인한다. 홉의 옵션은 [옵션 적용 순서](#옵션-적용-순서) |
| `ProxyCommand none` | 프로세스를 시작하지 않는다. `none`이 아닌 값은 거부 |
| `SendEnv`, `SetEnv` | exec·포트 발견 채널의 `env` 요청 |
| `RemoteCommand` | exec·포트 발견에 명령이 있는데 `RemoteCommand`(`none` 아님)도 적용되면 OpenSSH처럼 실패한다(`ssh.c:1363`, `Cannot execute command-line and remote command`). 터널(`-N`, 세션 없음)에는 쓰이지 않는다 |
| `CanonicalizeHostname no`와 `CanonicalDomains`·`CanonicalizeFallbackLocal`·`CanonicalizeMaxDots`·`CanonicalizePermittedCNAMEs` | 뒤의 넷은 `CanonicalizeHostname`이 켜졌을 때만 쓰인다. 켜는 값은 거부 |
| `NoHostAuthenticationForLocalhost no` | `yes`는 거부 |
| `RefuseConnection` | OpenSSH처럼 그 메시지로 연결을 끝낸다 |

### 보안 관련 미지원 (적용되면 거부)

| 지시어·값 | 이유 |
|---|---|
| `StrictHostKeyChecking no`·`off` | 호스트 키 확인을 건너뛰는 동작은 두지 않는다(사용자 결정) |
| `NoHostAuthenticationForLocalhost yes` | localhost의 호스트 키 확인을 건너뛴다(`sshconnect.c:1013`) |
| `KnownHostsCommand` | 다른 프로그램을 실행해야 하고, 빠지면 `accept-new`가 다른 키를 받을 수 있다 |
| `ProxyCommand`(`none` 아님), `ProxyJump` 뒤에 온 `ProxyCommand`(`none` 아님) | 다른 프로그램 실행이다. 뒤의 경우는 OpenSSH도 `inconsistent options: ProxyCommand+ProxyJump`로 끝낸다(`ssh.c:1300`–`:1303`). `ProxyCommand`가 먼저면 `ProxyJump`는 OpenSSH에서도 쓰이지 않는다(`parse_jump`의 `active &= proxy_command == NULL && jump_host == NULL`, `readconf.c:3493`) |
| `Match exec` | 다른 프로그램을 실행해야 블록 적용을 판정한다. 판정하지 못한 블록의 `Hostname`·`ProxyJump`가 빠지면 고르는 대상이 달라진다 |
| `Match localnetwork` | 블록 적용을 판정하지 못한다(같은 이유) |
| `CanonicalizeHostname yes`·`always` | 고르는 호스트가 달라진다 |
| `BindInterface` | 나가는 경로가 달라진다(사용자가 정한 경로를 벗어나 연결할 수 있다) |
| `PubkeyAuthentication host-bound` | 사용자가 요구한 인증 방식을 지킬 수 없다 |
| `RevokedHostKeys`가 KRL 파일 | 폐기 목록을 읽지 못하면 폐기된 키를 받을 수 있다 |
| 알고리즘 지시어의 교집합이 빔 | [알고리즘](#알고리즘) |
| 모르는 지시어 | OpenSSH와 같다 |

### 읽었지만 미지원 (표시하고 계속)

| 지시어 | 내장 백엔드의 동작 |
|---|---|
| `LocalForward`, `RemoteForward`, `DynamicForward`, `ExitOnForwardFailure`, `GatewayPorts`, `ClearAllForwardings`, `PermitRemoteOpen` | 이 레포는 자기 터널만 연다. 시스템 백엔드는 OpenSSH대로 설정 파일의 `LocalForward` 등을 함께 연다(터널 고정 옵션 `ClearAllForwardings=no`가 첫 값으로 이긴다). 두 백엔드가 다른 점이다 |
| `ControlMaster`, `ControlPath`, `ControlPersist` | 세션을 프로세스 밖과 공유하지 않는다 |
| `ForwardAgent`, `AddKeysToAgent` | agent를 넘기지 않고 키를 넣지 않는다 |
| `ForwardX11`, `ForwardX11Trusted`, `ForwardX11Timeout`, `XAuthLocation`, `Tunnel`, `TunnelDevice`, `StreamLocalBindMask`, `StreamLocalBindUnlink` | 해당 기능이 없다 |
| `RequestTTY`, `SessionType`, `StdinNull`, `ForkAfterAuthentication`, `EscapeChar`, `EnableEscapeCommandline`, `ObscureKeystrokeTiming`, `ChannelTimeout` | 대화형 세션이 없다. 세션 종류는 용도가 정한다 |
| `LocalCommand`, `PermitLocalCommand` | 실행하지 않는다 |
| `ProxyUseFdpass` | `ProxyCommand`가 거부되므로 쓰일 곳이 없다 |
| `Compression yes` | 압축 없이 연결한다(russh `flate2` feature를 켜지 않는다) |
| `GSSAPIAuthentication`, `GSSAPIDelegateCredentials`, `HostbasedAuthentication`, `HostbasedAcceptedAlgorithms`, `EnableSSHKeysign` | 그 인증 방법을 건너뛴다. russh에는 GSSAPI 구현이 없고, hostbased는 `ssh-keysign` 프로그램이 필요하다 |
| `PKCS11Provider`, `SmartcardDevice`, `SecurityKeyProvider` | 그 키 소스를 건너뛴다. `*_sk` 키는 agent가 있을 때만 쓴다 |
| `VerifyHostKeyDNS` | DNS SSHFP로 키를 받지 않는다. known_hosts만 보므로 더 엄격하다 |
| `UpdateHostKeys` | known_hosts에 새 키를 더하지 않는다 |
| `LogLevel`, `LogVerbose`, `SyslogFacility`, `VisualHostKey`, `FingerprintHash`, `WarnWeakCrypto`, `VersionAddendum`, `IPQoS` | 출력·표시·패킷 표시 값이다 |

### OpenSSH가 버린 지시어

- `oDeprecated`: `Cipher`, `FallBackToRsh`, `GlobalKnownHostsFile2`, `RhostsAuthentication`, `UserKnownHostsFile2`, `UseRoaming`, `UseRsh`, `UsePrivilegedPort`.
- `oUnsupported`: `AFSTokenPassing`, `KerberosAuthentication`, `KerberosTGTPassing`, `RSAAuthentication`, `RhostsRSAAuthentication`, `CompressionLevel`.
- `oIgnore`: `Protocol`.

## 호스트 키 판정

- 판정은 이식 크레이트의 `KnownHosts::check`가 한다. russh의 known_hosts 도우미(`check_known_hosts_path`)는 쓰지 않는다. 그 도우미는 `@revoked`를 무시하고 줄을 공백 하나로만 나눈다(근거 보고서).
- 순서는 OpenSSH `check_host_key`(`sshconnect.c:984`)와 같다: `@revoked` 일치면 거절, 일치하는 키가 있으면 통과, 같은 형식의 다른 키가 있으면 거절(바뀐 키), 없으면 `StrictHostKeyChecking`에 따라 묻거나(`ask`) 기록하거나(`accept-new`) 거절한다(`yes`). `CheckHostIP yes`면 주소로도 같은 판정을 한다.
- `@cert-authority` 줄은 1단계에서 믿지 않는다. 그 줄은 판정에서 통과 근거가 되지 않고, `DistrustedCaLine`으로 `ConfigNote`(`cert_authority_distrusted`)에 남는다. 호스트 인증서 알고리즘(`*-cert-v01@openssh.com`)은 제안 목록에서 빼므로 서버는 평문 호스트 키를 낸다. 그 키가 known_hosts에 없으면 위 순서대로 모르는 호스트다.
- `RevokedHostKeys`와 `RequiredRSASize`는 위 [지원](#지원) 표대로 판정에 들어간다.
- `accept-new`의 기록은 OpenSSH `add_host_to_hostfile`(`hostfile.c:542`)과 같은 형식이고 `HashKnownHosts`를 따른다. 기록할 파일은 `UserKnownHostsFile`의 첫 파일이다.

## 알고리즘

- 기본 목록은 OpenSSH 10.6p2 `myproposal.h`다. 거기에 지시어의 `+`·`-`·`^` 연산을 `kex_assemble_names`(`kex-names.c:241`)대로 적용하고, 그 결과와 russh가 구현한 이름의 교집합을 제안한다. 교집합이 비면 거부한다. 빠진 이름은 `ConfigNote`(`algorithm_not_implemented`)로 남긴다.
- 호스트 키 알고리즘은 `order_hostkeyalgs`(`sshconnect2.c:121`)대로 known_hosts에 이미 있는 키 형식을 앞으로 당긴다.
- `ssh-rsa`(SHA-1 서명)는 OpenSSH 10.x 기본처럼 호스트 키·사용자 키 기본 목록에 없다. 지시어 `+ssh-rsa`로 켤 수 있다(OpenSSH와 같다).
- `ext-info-c`와 `kex-strict-c-v00@openssh.com`은 목록 계산과 관계없이 늘 붙인다(OpenSSH와 같다).

| 종류 | OpenSSH 10.6p2 기본 | russh 0.65.4(`ring`)에 없는 이름 |
|---|---|---|
| kex | `mlkem768x25519-sha256`, `sntrup761x25519-sha512`(@openssh.com 포함), `curve25519-sha256`(@libssh.org 포함), `ecdh-sha2-nistp256`·`384`·`521`, `diffie-hellman-group-exchange-sha256`, `diffie-hellman-group16-sha512`, `group18-sha512`, `group14-sha256` | `sntrup761x25519-sha512`(russh `negotiation.rs:703`이 구현하지 않는다고 적는다) |
| 암호 | `chacha20-poly1305@openssh.com`, `aes128-gcm@openssh.com`, `aes256-gcm@openssh.com`, `aes128-ctr`, `aes192-ctr`, `aes256-ctr` | 없음 |
| MAC | `umac-64-etm`, `umac-128-etm`, `hmac-sha2-256-etm`, `hmac-sha2-512-etm`, `hmac-sha1-etm`, `umac-64`, `umac-128`, `hmac-sha2-256`, `hmac-sha2-512`, `hmac-sha1`(etm은 `@openssh.com`) | `umac-*` 넷 |
| 호스트 키·사용자 키 | 인증서 형식들, `ssh-mldsa44-ed25519`(인증서 포함), `ssh-ed25519`, `ecdsa-sha2-nistp256`·`384`·`521`, `sk-ssh-ed25519@openssh.com`, `sk-ecdsa-sha2-nistp256@openssh.com`, webauthn 형식, `rsa-sha2-512`, `rsa-sha2-256` | `ssh-mldsa44-ed25519`, webauthn 형식. 인증서 형식은 1단계에서 빼고 제안한다 |

- `CASignatureAlgorithms`는 사용자 인증서(`CertificateFile`)를 고를 때 쓴다. `PubkeyAcceptedAlgorithms`는 사용자 키 인증 서명 알고리즘이고, russh `Preferred`의 `key`는 호스트 키 협상용이라 사용자 키 쪽은 이 레포가 인증 시도 전에 거른다.

## 설정값

[크레이트 구성](crates.md#설정값-규칙)의 규칙(설정·조회, 출처, 코드 기본값 표 하나, 표와 코드 대조)을 따른다. 아래 표는 이 설계가 정한 항목이다. 구현 작업이 [SSH 크레이트 설정값](transport.md#ssh-크레이트-설정값) 표로 옮겨 기본값 대조 시험의 대상으로 만든다(그 전에는 이 표가 정본이다). 기존 `ssh.connect_timeout`·`ssh.server_alive_interval`·`ssh.server_alive_count_max`·`discovery.*`·`tunnel.*`·`backoff.*`는 두 백엔드가 함께 쓴다(내장은 `ServerAliveInterval`·`ServerAliveCountMax`를 russh `keepalive_interval`·`keepalive_max`에 넣는다).

| 키 | 기본 | 범위 | 근거 |
|---|---|---|---|
| `ssh.backend` | `auto` | `auto`·`system`·`embedded`(빌드에 없는 백엔드는 거절) | 사용자 결정 |
| `ssh.overrides` | 빈 목록 | `지시어=값` 목록, 64개까지 | `ssh -o`와 같은 뜻. 적용 순서는 위 7. 64는 OpenSSH 설정의 한 호스트에 흔히 쓰는 옵션 수보다 넉넉하고, 목록이 커지는 실수를 막는다 |
| `embedded.config_file` | 없음(사용자 설정 + 시스템 설정) | 경로 하나 또는 `none` | `ssh -F`와 같다. 주면 그 파일만 읽고 시스템 설정은 읽지 않는다 |
| `embedded.system_config_file` | OS의 시스템 설정 경로 | 경로 또는 `none` | 출처 `os_default`. 모바일에는 없다 |
| `embedded.home` | OS 홈(`HOME`, Windows `USERPROFILE`) | 경로 | 출처 `os_default`. `~` 펼침과 `%d` |
| `embedded.ssh_dir` | `<embedded.home>/.ssh` | 경로 | 추가 결정 4. 모바일은 바인딩 층이 앱 저장소의 `ssh/`를 넣는다 |
| `embedded.local_user` | OS 사용자 이름(`USER`, Windows `USERNAME`) | 문자열 | `%u`·`Match localuser`, 원격 사용자를 정하지 않았을 때의 기본 사용자. 값이 없고 설정도 없으면 원격 사용자가 정해지지 않은 연결은 `UserRequired`로 끝난다 |
| `embedded.channel_open_timeout` | 10 s | 100 ms ~ 10 min | 채널 열기 응답 기한. `ssh.connect_timeout` 기본과 같은 값 |
| `embedded.prompt_timeout` | 5 min | 1 s ~ 1 h | `SshPrompt`의 답을 기다리는 시간. 사람이 passphrase를 치는 시간이다. 이 대기 동안 `ssh.connect_timeout`은 멈춘다 |
| `embedded.runtime.worker_threads` | 1 | 1 ~ 16 | 몇 개의 SSH 세션에 워커 하나로 충분하다. 모바일 스레드 비용을 줄인다 |
| `embedded.window_size` | 2 MiB | 32 KiB ~ 64 MiB | russh `Config::window_size` 기본(2097152, `client/mod.rs:2790`) |
| `embedded.max_packet_size` | 32 KiB | 1 KiB ~ 64 KiB | russh 기본(32768). russh는 65535보다 큰 값을 받으면 경고만 찍고 계속하므로(`client/mod.rs:1232`) 상한을 64 KiB로 둔다 |
| `embedded.channel_buffer` | 100 | 1 ~ 10 000 | russh `channel_buffer_size` 기본 |
| `embedded.rekey.bytes` | 1 GiB | 1 MiB ~ 1 GiB | russh `Limits` 기본. `Limits::new`가 읽기·쓰기 한도 각각 1 GiB(`1 << 30`) 이하를 단언한다(`lib_inner.rs:274`). `RekeyLimit` 지시어가 이기고, 그 값이 1 GiB를 넘으면 1 GiB로 자르고 `ConfigNote`(`clamped`)로 남긴다 |
| `embedded.rekey.time` | 1 h | 1 min ~ 24 h | russh `Limits::rekey_time_limit` 기본 |
| `embedded.gex.min_bits`, `embedded.gex.preferred_bits`, `embedded.gex.max_bits` | 3072, 8192, 8192 | 각각 2048 ~ 8192이고 `min ≤ preferred ≤ max` | russh `GexParams` 기본. `GexParams::for_client_config`가 이 순서를 검사하므로(`client/mod.rs:2652`–`:2663`) `SshSettings::set`이 같은 검사를 하고 어긋나면 `SettingsError`로 거절한다 |
| `exec.output_buffer` | 1 MiB | 4 KiB ~ 64 MiB | 앱이 읽지 않은 stdout·stderr를 담는 상한. 넘치면 채널 창을 늘리지 않아 서버가 기다린다(데이터를 버리지 않는다) |
| `sftp.max_packet` | 256 KiB | 32 KiB ~ 256 KiB | russh-sftp `Config::max_packet_len` 기본(262144) |
| `sftp.max_concurrent_reads`, `sftp.max_concurrent_writes` | 16, 16 | 1 ~ 64 | russh-sftp 기본 |
| `sftp.max_write_packet` | 32 KiB | 4 KiB ~ 256 KiB | russh-sftp 기본(32768) |
| `sftp.request_timeout` | 10 s | 1 s ~ 10 min | russh-sftp `request_timeout_secs` 기본. 1초 단위다 |

고정값(바꿀 수 없다. 조회에는 출처 `fixed`로 나온다):

| 키 | 값 | 근거 |
|---|---|---|
| `config.include_max_depth` | 16 | OpenSSH `READCONF_MAX_DEPTH`(`readconf.c:2629`). 바꾸면 같은 설정 파일이 OpenSSH와 다르게 해석된다 |
| `config.openssh_version` | `OpenSSH_10.6` | 이식 기준 판. `Match version`의 판정 값 |

- russh 0.65.4 `client::Config`의 공개 필드 가운데 시간·횟수·크기·상한을 받는 것은 모두 위 표나 기존 `ssh.*`에 있다. `inactivity_timeout`은 쓰지 않는다(링크 heartbeat와 keepalive가 끊김을 본다). 구현 작업은 [P2P](p2p.md#의존성이-노출하는-값)와 같은 방식으로 고정판의 공개 setter 목록을 시험 자료로 두고 빠진 항목을 시험이 찾게 한다. russh-sftp `Config`의 다섯 필드도 같다.
- 알고리즘 목록은 설정값으로 따로 두지 않고 OpenSSH 지시어로 받는다([알고리즘](#알고리즘)).
- 호스트 키 확인을 건너뛰는 설정값은 두지 않는다.

## 의존 사실

2026-11-27에 crates.io와 배포 소스로 확인했다.

| 의존 | 판·게시일 | 라이선스 | 확인한 사실 |
|---|---|---|---|
| `russh` | 0.65.4(2026-11-21) | Apache-2.0 | `rust-version = "1.89"`(workspace와 같다). 이미 `tasty-net-demo`의 dev 의존이다(`Cargo.lock`). 기본 feature는 `flate2`·`aws-lc-rs`·`rsa`이고 `ring`도 `aws-lc-rs`도 없으면 `compile_error!`. `rsa`를 끄면 RSA 키 읽기·인증이 cfg로 빠진다(`keys/format/mod.rs:34`·`:74`·`:100`). 일반 의존 tokio의 feature에 `process`가 없다(`process`는 dev 의존에만). host-bound 인증이 없다 |
| `russh-sftp` | 4.0.0(2026-11-21). 앞 판 3.0.1(2026-09-28), 3.0.0, 2.4.0 | Apache-2.0 | russh에 의존하지 않는다(russh는 dev 의존 0.65.1). `SftpSession::new(stream)`이 아무 `AsyncRead + AsyncWrite`를 받는다. SFTP 프로토콜 판 3(`protocol/mod.rs:78`, OpenSSH `sftp-server`가 쓰는 판). tokio feature는 `io-util`·`rt`·`sync`·`time`·`macros`(`process` 없음). `rust-version`을 적지 않았다(edition 2021) |
| `ssh-key` | `=0.7.0-rc.12`(russh가 정확한 판으로 고정) | Apache-2.0 OR MIT | 위험: 정식판 전의 후보 판이 정확한 판으로 고정돼 있다. 정식판이 나오면 russh 판과 함께 올린다. `Certificate::validate_at`은 CA 지문·서명·시각만 본다(1단계는 호스트 인증서를 쓰지 않으므로 이 함수를 쓰지 않는다) |
| `russh-config` | 0.65.3 | Apache-2.0 | 쓰지 않는다. `Match`·`Include`가 없고, 줄을 첫 공백으로만 나누고, 모르는 지시어를 로그만 남기고 넘기고, `ProxyCommand`를 `tokio::process`로 실행한다 |

- 암호 백엔드는 `ring`이다. quinn·rustls가 이미 쓰므로 C 빌드 조건이 하나다([P2P](p2p.md#외부-의존과-고정)).
- 보안 패치: CI에 `cargo audit` 단계를 둔다(내장 백엔드 작업이 만든다). russh·russh-sftp·ssh-key의 보안 공지가 나오면 판을 올린다.

## 시험의 자리

구현 작업이 만든다. 정확한 조건은 시험 파일이 정본이다.

| 시험 | 자리 | 확인하는 것 |
|---|---|---|
| `ssh -G` 대조 | `tasty-net-ssh-config` `tests/matches_openssh.rs` | 같은 설정 묶음(Host·Match·Include·토큰·첫 값 우선·`ProxyJump`와 `ProxyCommand none`의 순서·홉 별칭)으로 `ssh -G <대상>`(대상이 홉이면 `ssh -G <홉>`)과 이식 크레이트의 `dump_client_config` 형식 출력이 같다. 시험이 클라이언트 판(`ssh -V`)을 찍는다. 판이 기준과 다르면 건너뛰지 않고 차이를 보고한다(차이 목록과 두 판을 출력하고 실패한다). 클라이언트가 없는 러너에서는 미실행으로 남기고 그 사실을 찍는다 |
| `ssh-keygen -F` 대조 | 같은 크레이트 `tests/known_hosts_matches_openssh.rs` | 해시·와일드카드·부정·`[호스트]:포트`·`@revoked`·`@cert-authority` 줄이 섞인 파일에서 `ssh-keygen -F <호스트> -f <파일>`이 찾는 줄과 이식 크레이트가 찾는 줄이 같다 |
| 지시어 분류 | 같은 크레이트 단위 시험 | `keywords[]` 132개가 모두 분류되어 있다(원본 표를 시험 자료로 둔다). 보안 관련 거부 지시어마다 거부, 표시 지시어마다 `shown`, 모르는 지시어 거부, `IgnoreUnknown`이 보안 거부를 무르지 않음 |
| 폴백 선택 | `tasty-net-ssh` 단위 시험 | 발견 함수에 프로그램 이름과 `PATH` 값을 인자로 주어(프로세스 환경 변수를 바꾸지 않는다) `found{path}`·`not_found{searched}`, Windows의 `.exe` 붙이기, `configured`, `system-ssh`만인 빌드의 `NotFound`를 본다. 다른 프로그램을 실행하지 않는다 |
| 내장 백엔드 attach | `crates/tasty-net-demo/tests/ssh_attach.rs`·`ssh_reverse_attach.rs`의 D2·D3 흐름을 내장 백엔드로 한 번 더(시험 이름 끝 `_embedded`) | demo 자식에게 `ssh.backend = auto`와 런처 프로그램 `ssh`(구분자 없는 이름)를 주고, 자식 환경의 `PATH`를 빈 폴더 하나로 준다. 구분자 없는 이름이라 Windows의 `%WINDIR%` 규칙을 타지 않으므로 세 OS에서 `not_found`로 내장을 고른다. 연결 목록의 `ssh.backend.reason`이 `not_found`인지 본다. 터널 종료·재연결 단계는 자식 PID 대신 `SessionHook`(시험 훅, `SpawnHook`과 같은 자리)으로 내장 세션을 끊고, 새 `session_id`와 새 연결 항목을 본다 |
| exec | demo 시험 | 시험 sshd에서 두 백엔드로 `exit 0`·`exit 3`·표준 입력 되돌리기·stderr를 보고 종료 상태와 출력이 같다. 내장은 `kill -TERM $$`의 `Signal{name: "TERM"}`, 시스템은 같은 경우 `Ambiguous255` |
| SFTP | demo 시험 | 시험 sshd(`Subsystem sftp internal-sftp`)에서 두 백엔드로 파일 쓰기·읽기·`read_dir`·`rename`·`remove_file`의 결과가 같다. 내장은 같은 세션 위에서 터널과 SFTP를 함께 연다 |
| 호스트 키 | `tasty-net-ssh` 시험과 demo 시험 하나 | 바뀐 키 거절, `@revoked` 거절, `@cert-authority` 줄만 있는 호스트는 모르는 호스트, `RevokedHostKeys` 파일이 없으면 거절, 1024비트보다 작은 RSA 호스트 키 거절, `accept-new` 기록 |
| 리스너 판정 | `tasty-net-ssh` 시험 | 사유 코드 2만 `RemoteListenerAbsent`, 1·3·4는 `ChannelOpenRefused` |

## 후속 작업과 미정

| 항목 | 상태 |
|---|---|
| 1단계 feature 분리 | 후속 작업(transport ssh feature 분리) |
| 이식 크레이트 `tasty-net-ssh-config` | 후속 작업(OpenSSH 해석 이식) |
| 2단계 내장 백엔드·선택 규칙·exec·SFTP·cargo-audit | 후속 작업(내장 SSH 백엔드). 이식 크레이트 뒤 |
| `@cert-authority`와 호스트 인증서 | 2단계 밖. 넣을 때 인증서 형식·주체 이름·중요 옵션·CA 서명 알고리즘·`@revoked` 적용을 따로 정한다 |
| 내장 세션을 붙들어 두는 시간 | 정하지 않음. 모바일에서 연속 exec마다 인증하는 비용이 문제가 되면 android 요구와 함께 설정값으로 정한다 |
| tasty 설정 화면의 SSH 항목 | tasty 전환 때. 이 레포 범위 밖 |
