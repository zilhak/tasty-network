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
| 2026-11-27 | director 2881: 이식 크레이트 라이선스는 `BSD-2-Clause AND ISC AND SSH-short`. 거부는 무시하면 보안이 약해지는 지시어에만, 제공하지 않는 인증 방법을 켜는 지시어는 표시하고 다음 인증으로 진행 | [라이선스](#라이선스), [지시어 분류](#지시어-분류) |
| 2026-11-27 | director 2931: 2881 분류 기준의 둘째 종류(다른 호스트·경로로 연결되는 값) 확장, `ChannelTimeout` 지원, 확인 끄기 값의 표시를 승인. 내장 SSH attach 시험은 동작 수준 기준과 따로 둔다 | [지시어 분류](#지시어-분류), [선택 규칙](#선택-규칙), [동작 수준 기준](done-criteria.md#모바일-공개-전-기준) |
| 2026-11-27 | director 3000: 원본(`openbsd-compat`)의 구조·변수 이름·경계 처리를 단계별로 따르는 코드는 파생으로 보고 원본 고지와 SPDX 식을 맞춘다. 이식 크레이트 라이선스에 `BSD-3-Clause`를 더한다 | [라이선스](#라이선스) |
| 2026-11-28 | director 3106: OS libc 차이로 내장 백엔드가 같은 OS의 시스템 ssh와 다른 주소로 붙으면 접속 대상이 바뀐다(대상·경로가 바뀌는 것은 안전하게). 숫자 주소를 실행 OS의 libc 규칙에 맞추고, 맞출 수 없는 표기는 거부하고 사유를 보인다. 시험은 OS별 기대값 표로 한다 | [옮기는 원본](#옮기는-원본)의 숫자 주소, [후속 작업과 미정](#후속-작업과-미정) |

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
- SSH 없는 조합은 검사 단계 `scripts/check.sh transport-no-ssh`가 본다: `--no-default-features`의 clippy·시험·문서(깨진 문서 링크는 실패)와, 그 `cargo tree`에 `tasty-net-ssh`가 없는지(기본 feature의 그래프에는 있는지 대조). CI는 세 OS의 clippy·test 잡에서 부른다([CI](../dev-guide/ci.md#ssh-없는-transport)).

### 2단계: 내장 백엔드 (이식 크레이트 뒤)

| 크레이트 | feature | 기본 | 켜는 것 |
|---|---|---|---|
| `tasty-net-ssh` | `system-ssh` | 켬 | 1단계와 같다 |
| `tasty-net-ssh` | `embedded-ssh` | 켬 | 내장 백엔드. 의존: `russh`(정확한 판, `default-features = false`, `features = ["ring", "rsa"]`), `russh-sftp`(정확한 판), `tasty-net-ssh-config`, `tokio`(`rt-multi-thread`·`net`·`time`·`sync`·`io-util`, `process` 없음) |
| `tasty-net-ssh` | 둘 다 끔 | | `compile_error!` |
| `tasty-net-transport` | `system-ssh` | 켬 | 1단계와 같다 |
| `tasty-net-transport` | `embedded-ssh` | 켬 | `["ssh", "tasty-net-ssh/embedded-ssh"]` |

- 기본값은 두 크레이트 모두 `["system-ssh", "embedded-ssh"]`이다. 데스크톱 소비자(tasty)는 기본 그대로 쓰고, OpenSSH가 없는 데스크톱에서도 폴백이 된다.
- `embedded-ssh`가 기본값에 드는 것은 [내장 폴백 활성 기준](done-criteria.md#내장-폴백-활성-기준)을 채운 커밋부터다(director 2931). 위 표의 `embedded-ssh` 기본 "켬"과 앞 문장은 그 뒤의 상태다. 그 전 2단계 커밋의 기본값은 `["system-ssh"]`이고, 내장 백엔드는 그 feature를 켜는 소비자(모바일 조합)만 쓴다.
- 모바일 조합은 `--no-default-features --features embedded-ssh`다. 이 단계에서 cross check에 ssh와 transport의 내장 조합 두 행이 더해진다.
- `tasty-net-ssh-config`는 feature가 없다. `std::process`를 쓰지 않는다.
- 이 단계가 내장 백엔드 작업과 같은 커밋 묶음에서 들어간다. 그 전에 `embedded-ssh`라는 이름의 빈 feature를 만들지 않는다. 빈 feature를 두면 `--no-default-features --features embedded-ssh`가 백엔드 없는 ssh 크레이트가 되어 `compile_error!` 계약과 어긋난다.
- 내장 백엔드 첫 커밋 묶음(터널·포트 발견·호스트 키·인증, exec·SFTP 전)의 실제 의존: `embedded-ssh = ["dep:russh", "dep:tasty-net-ssh-config", "dep:tokio", "dep:ring", "dep:base64"]`. `russh-sftp`는 SFTP가 들어가는 커밋에서 더한다. 직접 의존 `ring`(`=0.17.14`)은 `accept-new`가 기록하는 해시 이름의 HMAC-SHA1(`hostfile.c`의 `host_hash`)에, `base64`(`=0.23.1`)는 기록하는 줄의 키 blob에 쓴다. 두 판 모두 그래프에 이미 있던 판이다. demo도 같은 이름의 feature(`["tasty-net-ssh/embedded-ssh", "tasty-net-transport/embedded-ssh"]`)를 둔다.

### `ssh`가 꺼진 transport에서 사라지는 것

공개 API에서 사라지는 항목:

| 항목 | 지금 자리 | 비고 |
|---|---|---|
| `SshTransport`, `FORWARDED_SEPARATOR`, `LOCAL_CONNECT_TIMEOUT` | `src/ssh.rs`, `src/lib.rs` 다시 내보내기 | |
| `AttemptCancel::ssh_cancel` | `src/cancel.rs:97` | |
| `ConnectError::Discovery(PortDiscoveryError)`, `ConnectError::Tunnel(TunnelError)` | `src/transport.rs:202`·`:205` | `ConnectError`는 `#[non_exhaustive]`(`:194`) |
| `ConnectError::Backend(SshBackendError)` | 2단계에서 새로 생긴다 | 같다 |
| `HandshakeError::RemoteListenerAbsent(RemoteListenerAbsent)` | `src/reject.rs:228` | `HandshakeError`는 `#[non_exhaustive]`(`:186`) |
| 연결 목록 `Cause::Discovery(PortDiscoveryFailureKind)` | `src/connection_table/info.rs:155` | `Cause`는 `#[non_exhaustive]`(`:151`) |
| `ConnectionInfo.ssh` 필드의 타입 `SshEntryInfo` | 2단계에서 새로 생긴다 | 아래 [연결 정보](#연결-정보) |

- 세 enum이 모두 `#[non_exhaustive]`라서 소비자의 `match`는 어느 조합에서도 `_` 갈래를 이미 갖는다. 변형이 사라져도 소비자 코드는 그대로 컴파일된다.
- 원인 코드 문자열 `discovery_failed`(`info.rs:197`)는 `ssh`가 꺼진 빌드에서 나오지 않을 뿐 다른 뜻으로 다시 쓰지 않는다. 연결 목록 타입에는 serde 직렬화가 없다(transport의 `connection_table`에 `Serialize` 0건). 외부 표현은 `Cause::as_str()`의 코드뿐이다.
- FFI 모양: `tasty-net-ffi`는 늘 `embedded-ssh`로 빌드하므로([모바일 바인딩 층](mobile.md#바인딩-층)) FFI에 보이는 변형은 조합에 따라 달라지지 않는다. SSH 없는 transport는 Rust 소비자만 쓰는 조합이다.
- 남는 공개 항목: `Transport`·`Listener`·`RawChannel`·`Link`, handshake v2, 메모리 전송, P2P 어댑터, `ConnectionTable`과 그 설정값, `Redialer`, `Phase`의 `PortDiscovery`·`Tunnel`, `PathKind`의 `ssh_forward`·`ssh_role_reversed`·`ssh_inbound`, `Cause::TunnelFailed`·`TunnelExited`·`RemoteListenerAbsent`(SSH 타입을 싣지 않는 unit·문자열 변형), `Via::Ssh{target, remote_port}`. 받는 쪽 `ssh_inbound`는 꺼진 빌드에서도 생긴다(상대가 SSH로 연 연결을 루프백 listener로 받는다).
- 2단계에서 더하는 unit 원인 `Cause::SshBackendUnavailable`(`ssh_backend_unavailable`)과 `Cause::SshConfigRefused`(`ssh_config_refused`)도 SSH 타입을 싣지 않으므로 조합과 관계없이 남는다.

내부 항목의 처리:

| 항목 | `ssh`가 꺼진 빌드 |
|---|---|
| `RawChannel.verdict: Option<ListenerVerdict>`(`src/channel.rs:65`), `with_verdict`(`:186`), `listener_absent`(`:194`–`:199`) | `#[cfg(feature = "ssh")]`로 뺀다. 판정을 쓰는 곳은 SSH 채널뿐이다 |
| `handshake.rs` `explain()`(`:611`–`:629`, 꺼진 빌드의 것은 `:631`–`:635`)의 `tasty_net_ssh::connection_closed_error` 호출 | 그 함수는 `io::Error::new(UnexpectedEof, …)` 하나다(`tasty-net-ssh/src/listener.rs:118`). `explain`의 SSH 판정 갈래만 cfg로 빼고, 꺼진 빌드에서는 받은 오류를 그대로 돌려준다. 닫힘 오류 자체는 handshake가 직접 만든다 |
| 통합 시험 `tests/ssh.rs`(`:18`에서 `SpawnHook`·`SshLauncher` 사용) | 파일 첫 줄 `#![cfg(feature = "system-ssh")]` |
| 통합 시험 `tests/connection_table.rs`(SSH 항목은 `mod ssh`가 만든다) | 항목을 만드는 방법은 둘이다: 받은 루프백 채널의 handshake 중 dialer가 닫는 `ssh_inbound` 항목(SSH 없음, 늘 있다)과, `system-ssh`에서 `ssh`를 띄우지 못하는 SSH 시도(`mod ssh`, `#[cfg(feature = "system-ssh")]`). `endings()`를 쓰는 시험 넷(ID 비재사용·끊긴 항목 상한·이벤트 lag·이력 상한)은 빌드에 있는 방법 모두를 같은 단언으로 돌고, 재연결 사슬(`previous`)·포트 발견 phase와 원인은 SSH 방법에서만 본다. `redialer_follows_settings`는 어느 조합에서든 루프백 항목만 쓴다. 역할 반전 시험 `a_redial_keeps_the_chosen_role_reversal`은 SSH 항목만 쓰고 `system-ssh`에서만 돈다. `cargo test -p tasty-net-transport --no-default-features`가 SSH 없이 공통 시험을 돈다 |
| SSH 전송만 부르는 내부 도우미: `RawChannel::with_entry`·`with_joined`·`wrap_control`(`src/channel.rs`), `Joined::new`·`abandoned`, `EntryRef::choose_path`·`phase`(`src/connection_table/mod.rs`) | `#[cfg(feature = "ssh")]`로 뺀다. 꺼진 빌드에서 부르는 곳이 없다(workspace의 `dead_code = "deny"`). 다른 전송이 쓰게 되면 그 작업이 cfg를 거둔다 |

### 공통 항목과 백엔드 전용 항목

| `tasty-net-ssh` 항목 | 있는 조합 |
|---|---|
| `SshTarget`, `SshSettings`와 설정값, `Backoff`, `SshTunnel`, `SshCancel`, `ListenerVerdict`·`RemoteListenerAbsent`, `PortDiscoveryError`·`PortDiscoveryFailureKind`, `TunnelError`, `connection_closed_error`, `SshConnector`, `SshInspection`, `SshExec`·`ExitStatus`, `SshBackendKind`, `SelectedBackend`, `BackendReason`, `SshBackendError`, `SshPrompt` | 백엔드가 하나라도 있으면 늘 |
| `SshLauncher`, `SpawnHook`·`SpawnFn`, `resolve_ssh_path` | `system-ssh` |
| `ResolvedTarget`, `ConfigNote`의 해석 결과 채우기, `SessionHook`(시험 훅) | `embedded-ssh`. 타입은 공통이고 시스템 백엔드에서는 비어 있다 |
| `SftpClient`, `SftpError` | `embedded-ssh`(russh-sftp가 이 feature로 들어온다). 그 빌드에서는 두 백엔드가 모두 SFTP를 준다 |

- 위 표는 2단계 기준이다. 1단계에는 지금의 항목과 `system-ssh` 전용 항목만 있다(`SshConnector`·`inspect`·exec·SFTP·선택 타입은 2단계에서 생긴다).

## 진입점

- `SshConnector::new(settings: SshSettings) -> SshConnector`: 백엔드를 고르고 터널·포트 발견·exec·SFTP를 연다.
- `SshConnector::with_launcher(self, launcher: SshLauncher) -> SshConnector`(`system-ssh`): 시스템 백엔드의 프로그램·자식 생성 훅·`with_verify`를 준다. 커넥터의 설정이 런처의 설정을 이긴다(런처가 가진 `SshSettings`는 쓰지 않는다).
- `SshConnector::with_prompt(self, prompt: Arc<dyn SshPrompt>) -> SshConnector`: passphrase·비밀번호·keyboard-interactive·호스트 키 확인 질문을 받을 콜백.
- `SshConnector::select(&self, target: &SshTarget) -> Result<SelectedBackend, SshBackendError>`: 지금 판정하면 고를 백엔드와 이유. 연결하지 않는다.
- `SshConnector::settings(&self) -> &SshSettings`: 설정값 조회(`entries()`가 `{key, value, source}`).
- `SshConnector::inspect(&self, target: Option<&SshTarget>, purpose: SshPurpose) -> SshInspection`: 설정 조회의 진단 쪽이다. 연결하지 않고 다른 프로그램을 실행하지 않는다. 아래 [설정 조회](#설정-조회).
- transport:
  - `SshTransport::from_connector(connector: SshConnector, discovery: PortDiscoveryConfig, mode: PortMode) -> SshTransport`가 새 생성자다. 포트 발견 설정과 포트 방식은 지금처럼 transport 인자로 남는다.
  - 지금의 `SshTransport::new(launcher: SshLauncher, discovery: PortDiscoveryConfig, mode: PortMode)`(`src/ssh.rs:138`)는 `system-ssh`에서만 있다. 뜻은 `SshTransport::from_connector(SshConnector::new(launcher.settings().clone()).with_launcher(launcher), discovery, mode)`다. 기존 호출은 그대로 컴파일된다. 동작은 런처의 프로그램을 찾을 때 같다(`ssh.backend`의 기본 `auto`가 시스템을 고른다). 두 백엔드를 함께 빌드한 경우(기본 빌드로는 [내장 폴백 활성 기준](done-criteria.md#내장-폴백-활성-기준) 뒤)에 그 프로그램을 찾지 못하면 뜻이 바뀐다: 지금은 자식 생성 실패지만, 새 규칙에서는 `auto`가 내장 백엔드로 연결한다(사용자 결정의 폴백). 이때 런처의 `SpawnHook`과 `with_verify`는 쓰이지 않는다. `with_verify`의 `accept-new`에 해당하는 내장 쪽 값은 순서 3과 같은 자리의 `StrictHostKeyChecking=accept-new`다(런처에서 읽어 커넥터가 같은 값을 넣는다).

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
  1. 런처의 프로그램(`SshLauncher::program()`, 기본은 `resolve_ssh_path()`)이 경로 구분자를 가지면 그 경로 하나를 본다. Windows의 `%WINDIR%\System32\OpenSSH\ssh.exe` 우선(`tasty-net-ssh/src/system/mod.rs:25`–`:31`: `resolve_ssh_path`, `full.exists()`, 그 경로 반환)은 기본 프로그램을 정하는 기존 규칙이다.
  2. 구분자가 없는 이름이면 `PATH`의 디렉터리를 앞에서부터 본다. Unix는 그 이름의 일반 파일에 실행 권한 비트가 있어야 한다. Windows는 디렉터리마다 `<이름>`, 이름에 확장자가 없으면 이어서 `<이름>.exe`를 본다.
  3. 시스템 백엔드는 발견한 절대 경로로 실행한다. 그래서 선택이 본 파일과 실행한 파일이 같다(Windows `Command`가 실행 파일 폴더·System32를 먼저 보는 규칙을 타지 않는다).
  4. 찾은 파일이 OpenSSH인지는 가리지 않는다. 같은 이름의 다른 클라이언트가 먼저 있으면 그것이 시스템 백엔드가 되고, 그 클라이언트가 옵션을 거절하면 시스템 백엔드의 오류로 끝난다. 알려진 한계다.
- 고르는 때: 연결 시도마다 한 번 고르고, 그 시도의 포트 발견과 터널은 같은 선택을 쓴다(발견은 시스템, 터널은 내장처럼 갈리지 않는다). 다음 시도는 다시 고르므로 실행 중에 OpenSSH를 설치하거나 지운 것이 반영된다. 이미 연 터널·세션은 처음 고른 백엔드로 끝까지 간다.
- `auto`의 내장 폴백(위 표 첫 행)은 두 백엔드가 함께 빌드된 빌드에서 동작한다. 그런 빌드를 tasty 릴리스로 내는 것은 [내장 폴백 활성 기준](done-criteria.md#내장-폴백-활성-기준)을 채운 뒤다. 그 전에는 이 레포의 기본 빌드가 `system-ssh`만이고(아래 2단계 feature), 소비자(tasty)도 데스크톱 빌드에서 `embedded-ssh`를 켜지 않는다. 그래서 그 전의 tasty 전환 태그에서 `auto`가 OpenSSH를 찾지 못하면 지금처럼 `NotFound`(시스템 ssh 없음)로 끝난다. tasty에 이 조건을 전하는 길은 이 설계 밖이다.
- 시스템 백엔드가 실패해도 내장으로 넘어가지 않는다. 폴백은 "찾지 못함"일 때만이다. 인증 실패나 호스트 키 거절을 다른 구현으로 다시 시도하면 사용자가 정한 동작과 다른 결과가 날 수 있다.

## 연결 정보

### `tasty-net-ssh` 쪽

```text
enum SshBackendKind { System, Embedded }
enum BackendReason { Found { path }, NotFound { searched: Vec<PathBuf> }, SystemNotBuilt, Configured }
struct SelectedBackend { backend: Option<SshBackendKind>, reason: BackendReason, selected_at: SystemTime }
struct ResolvedTarget {            // 내장 백엔드만. 시스템 백엔드는 None
    hostname, user, port, identity_files: Vec<PathBuf>, proxy_jump: Vec<JumpHop>,
    sources: Vec<ValueSource>,     // 대상을 고르는 값만 {keyword, value, source}, source는 아래 "옵션 적용 순서"
}
struct ConfigNote { keyword, file, line, action: Shown | Refused | Ignored, class: NoteClass }
```

- `backend`가 `None`이면 고르지 못한 것이다(`NotFound`).
- `SshTunnel::backend()`, 포트 발견 결과의 `backend`, `SshExec::backend()`·`SftpClient::backend()`가 `SelectedBackend`를 돌려준다. `SshTunnel::resolved()`와 `config_notes()`가 해석 결과와 지시어 표시를 돌려준다.
- `ConfigNote`의 값 칸은 싣지 않는다(비밀 값이 들 수 있다). 지시어 이름·파일·줄·처리·분류만 싣는다.
- `ResolvedTarget.sources`도 같은 이유로 연결이 어디로 가는지를 정하는 값만 싣는다(결정됨, conductor M4 Gate 4): `hostname`·`user`·`port`·`hostkeyalias`·`proxyjump`·`identityfile`·`certificatefile`. `SetEnv`·`SendEnv`·`RemoteCommand`·`LocalCommand` 같은 다른 값은 연결 목록과 설정 조회에 나가지 않는다.

### transport 연결 목록

| 필드 | 값 | 채우는 때 |
|---|---|---|
| `via` | 지금 그대로 `ssh{target, remote_port}`([전송 추상화](transport.md#필드)) | 지금과 같다 |
| `applied` | 지금 그대로 설정값만 싣는다(`{key, value, source}`). `ssh.backend`와 `embedded.*` 키는 더하지 않는다(결정됨, conductor M4 1단계 판단 답). 그 연결의 백엔드와 이유는 아래 `ssh` 필드(`SshEntryInfo`의 `backend`)가 싣고, `embedded.*`의 값과 출처는 `SshConnector::inspect`의 `settings`로 본다. 값 타입 `SettingValue`는 `#[non_exhaustive]` 없는 공개 `Copy` enum이라, 글자 값을 담으면 공개 API가 바뀌기 때문이다 | 지금과 같다 | |
| 새 필드 `ssh: Option<SshEntryInfo>` | `SshEntryInfo { backend: SelectedBackend, resolved: Option<ResolvedTarget>, config_notes: Vec<ConfigNote>, session_id: Option<u64> }`. SSH를 띄운 쪽 항목만 `Some`이다. 받는 쪽 `ssh_inbound`와 P2P 항목은 `None`이다(루프백으로 받은 TCP라 백엔드를 모른다) | 백엔드를 고른 직후, 포트 발견과 터널보다 먼저 채운다 |

- 실패한 시도: 고르지 못했거나(`NotFound`) 설정 해석이 거부되면(`Refused`) 항목은 네트워크에 나가기 전에 `disconnected`로 끝난다. 원인은 `ssh_backend_unavailable`·`ssh_config_refused`이고, `ssh` 필드에 이유·본 경로·거부한 지시어가 남는다. 포트 발견이나 터널이 실패한 항목도 `ssh` 필드는 그 시도의 선택을 그대로 갖는다.
- `session_id`는 [SSH 세션 목록](#ssh-세션-목록)의 세션 번호다(커넥터 안에서 1부터). 내장은 SSH 세션 하나, 시스템은 `ssh` 자식 하나가 세션 하나다. 같은 공유 키의 터널이 세션을 다시 열면 바뀐다. 터널 항목의 `session_id`는 그 터널이 쓰는 세션(내장은 터널 채널을 연 세션, 시스템은 `ssh -N` 자식)이 열린 때 채운다. 포트 발견 세션은 터널 세션과 다를 수 있으므로(시스템은 늘 다른 자식이다) 그 번호는 이 필드에 넣지 않는다. 그래서 `ssh` 필드가 채워진 뒤 터널 세션이 열리기 전(포트 발견 중, 세션 전에 실패한 시도)에는 `None`이다.
- 1단계(내장 전)에는 이 필드가 없다. 2단계에서 [전송 추상화](transport.md#필드)의 필드 표에 이 행을 더한다.

## 옵션 적용 순서

같은 지시어가 여러 곳에 있으면 먼저 얻은 값이 이긴다(OpenSSH의 첫 값 우선, `ssh.c`가 명령행 `-o`·`-p`·`-i`를 설정 파일보다 먼저 읽는다). 두 백엔드가 같은 순서를 쓴다. 시스템 백엔드는 이 순서대로 `ssh` 인자를 만들고, 내장 백엔드는 같은 순서로 이식 크레이트에 값을 넣는다.

| 순서 | 출처 | 값 | `source` |
|---|---|---|---|
| 1 | 런처 고정 | `BatchMode=no` | `forced` |
| 2 | 설정값 | `ServerAliveInterval`·`ServerAliveCountMax`(`ssh.server_alive_*`) | `setting:<키>` |
| 3 | 확인 모드 | `with_verify`면 `StrictHostKeyChecking=accept-new` | `forced` |
| 4 | 용도 고정 | 세션 종류: 터널 `-N`, SFTP `-s`, 포트 발견·exec `-o SessionType=default`(그 `ssh`가 아는 경우만, 아래 8.7 미만 처리). 세션 공유 끄기 `ControlMaster=no`·`ControlPath=none`(모든 용도). 터널만: `ExitOnForwardFailure=yes`, `ClearAllForwardings=no`와 fork 금지 옵션(`tunnel.rs:95`) | `forced` |
| 5 | 대상 | `SshTarget`의 포트(`-p`), 키 파일(`-i`) | `target` |
| 6 | 대상 | `SshTarget.extra_options` | `target` |
| 7 | 설정값 | `ssh.overrides`(`지시어=값` 목록) | `override` |
| 8 | 설정값 | `ConnectTimeout`(`ssh.connect_timeout`) | `setting:ssh.connect_timeout` |
| 9 | 사용자 설정 파일 | `~/.ssh/config` 또는 `embedded.config_file` | `file:<경로>:<줄>` |
| 10 | 시스템 설정 파일 | `/etc/ssh/ssh_config` 등 | `file:<경로>:<줄>` |
| 11 | OpenSSH 기본값 | `fill_default_options`(`readconf.c:2860`)의 값 | `default` |

- 1~3·6·8과 4의 `-o` 옵션은 지금의 `push_common_opts` 순서(`tasty-net-ssh/src/lib.rs:106`–`:150`)와 같다. 바뀌는 것은 7(`ssh.overrides`)이 더해지는 것과 세션 종류 플래그의 자리다. 지금 터널은 `-N`을 대상 옵션 뒤에 둔다(`spawn.rs:146`–`:157`이 `push_common_opts` 다음에 플래그를 붙인다). OpenSSH는 `-N`·`-s`를 읽을 때 이미 다른 세션 종류가 정해져 있으면 실패하고(`ssh.c:1030`–`:1033`·`:1048`–`:1051`), `-o`는 첫 값이 이긴다. 그래서 대상 옵션에 `SessionType=subsystem`이 있으면 지금 터널은 `Cannot specify -N with -s/SessionType`로 끝난다. 2단계는 세 용도의 세션 종류 플래그·옵션을 4의 자리(대상 옵션과 `ssh.overrides` 앞)로 옮긴다. 그러면 뒤의 `SessionType`은 첫 값에 져서 쓰이지 않는다(OpenSSH 9.6p1의 `ssh -G`로 확인: `-s -o SessionType=none`은 `sessiontype subsystem`, `-o SessionType=none -s`는 그 오류). 대상 옵션이 전역 덮어쓰기를 이기고, 덮어쓰기가 `ssh.connect_timeout`을 이긴다.
- 대상 옵션·`ssh.overrides`·설정 파일의 `SessionType`은 OpenSSH도 값을 먼저 검사한다. 맞는 값(`none`·`subsystem`·`default`)이면 첫 값에 져서 쓰이지 않고, 두 백엔드 모두 용도의 값과 다르면 `ConfigNote`(`shown`)로 남긴다. 거절하지 않는 이유: 무시해도 보안이 약해지지 않고, OpenSSH도 같은 인자 순서에서 같은 값을 쓴다. 틀린 값이면 OpenSSH는 첫 값이 이미 있어도 그 줄에서 끝낸다(`readconf.c:2198`–`:2201`, gx10 9.6p1 `ssh -G -o SessionType=default -o SessionType=INVALID`가 `unsupported option "INVALID"`로 끝남). 내장 백엔드도 같은 자리에서 설정 오류로 끝낸다(이식 크레이트의 값 검사가 같다).
- OpenSSH 8.7 미만: `SessionType`은 8.7p1에서 생겼다(8.6p1 `readconf.c`에 `sessiontype` 0건, 8.7p1 `:301`). 그 판은 `-o SessionType=default`를 `command-line: line 0: Bad configuration option: sessiontype`으로 거절하고 끝난다. 지원 여부는 원격 출력과 무관한 증거로 정한다. exec·포트 발견 자식의 stderr에는 원격 명령의 stderr도 섞이므로 그 문구를 보고 다시 실행하지 않는다. 런처가 그 실행 파일로 처음 한 번 `ssh -G -F none -o SessionType=default tasty-probe`(접속하지 않고 해석만 한다)를 실행해, 옵션 해석 오류(위 줄과 종료 코드 255)로 끝나면 "모름", 성공하면 "앎"으로 정한다. `-F none`은 시스템·사용자 설정 파일을 모두 읽지 않으므로(기준판 `ssh.c:572`–`:592` `process_config_files`, 8.6p1 `:570`도 같다) 사용자 설정의 `Match exec`가 실행되지 않고, 대상 이름은 고정값이다. 이 확인은 실행 파일이 옵션 이름을 아는지만 보고, 그 판단은 설정 파일보다 먼저 명령행 `-o`를 처리할 때 끝나므로(`ssh.c:1039`–`:1046`, 8.6p1 `:1031`–`:1036`이 거기서 255로 끝낸다) 대상과 사용자 설정이 필요 없다. 그래서 설정 오류로 판정이 미뤄지는 일은 없다. 그 밖의 이유로 끝나면 정하지 않고 "앎"으로 보고, 같은 오류는 실제 명령이 낸다. 결과는 찾은 실행 파일의 경로마다 기억하고, `PATH`에서 고른 파일이 바뀌면 다시 정한다. exec·포트 발견은 그 판정대로 처음부터 옵션을 넣거나 빼고 다시 실행하지 않는다. 그래서 앱 명령은 한 번만 실행된다. 그 판에는 `SessionType`도 `Match sessiontype`도 없으므로 옵션 없이 실행해도 세션 종류가 바뀌지 않는다. 실제 호출에 넘기는 사용자 설정(대상 옵션·`ssh.overrides`·설정 파일)의 `SessionType`은 OpenSSH가 해석하게 둔다. 그 판은 그 이름을 모르므로 오류로 끝내고, 앞에서 적용된 `IgnoreUnknown`이 그 이름을 덮으면 건너뛴다(8.6p1 `readconf.c:785`–`:787`). 터널의 `ForkAfterAuthentication` 재시도(`tunnel.rs:176`–`:188`, `listener.rs:53` `is_fork_option_refusal`)는 지금 코드 그대로 둔다.
- `with_verify`일 때만 3이 먼저 들어가므로 대상 옵션·`ssh.overrides`·설정 파일이 `StrictHostKeyChecking`을 약하게 만들 수 없다. `with_verify`가 없으면 시스템 백엔드는 호스트 키 옵션을 하나도 넣지 않는다(`lib.rs:117`·`:146`, 시험 `:860` `without_verify_no_host_key_option_is_added`). 그때는 대상 옵션·설정 파일·OpenSSH 기본(`ask`)이 정하고, 대상 옵션의 `StrictHostKeyChecking=no`는 `ssh`에 그대로 간다. 이것은 지금의 데스크톱 동작이고 대상 옵션은 소비자(tasty) 사용자의 값이라 바꾸지 않는다.
- 라이브러리 설정값으로는 호스트 키 확인을 약하게 만들 수 없다(사용자 결정 "호스트 키 확인을 건너뛰는 설정값은 두지 않는다"). 그래서 `SshSettings::set("ssh.overrides", …)`는 두 백엔드 모두에서 `SettingsError`로 거절한다: [지시어 분류](#지시어-분류)의 "보안 관련 미구현"과 "모르는 지시어"에 드는 항목(예: `ProxyCommand=…`), 그리고 호스트 키 확인을 끄는 값(`StrictHostKeyChecking=no`·`off`, `NoHostAuthenticationForLocalhost=yes`). 뒤의 둘은 파일·대상 옵션에서 읽으면 표시만 하는 값이지만(아래), 라이브러리 설정값으로는 받지 않는다. 설정값이 확인을 끄는 길이 되지 않게 하기 위해서다.
- 내장 백엔드는 설정 파일·대상 옵션의 `StrictHostKeyChecking no`·`off`와 `NoHostAuthenticationForLocalhost yes`를 적용하지 않고 호스트 키 확인을 그대로 한다. 그 값은 적히지 않은 것처럼 다루므로 확인 모드는 다음 값(앞의 3이 있으면 `accept-new`, 없으면 그다음 출처나 OpenSSH 기본 `ask`)이 정한다. `ConfigNote`(`shown`)에 "호스트 키 확인을 끄는 값은 적용하지 않고 확인을 계속한다"를 남긴다. 같은 대상 옵션으로 시스템 백엔드는 확인 없이 붙고 내장 백엔드는 확인하는 차이가 남는다. 두 백엔드가 갈리는 경우로 둔다.
- 누적 지시어(`IdentityFile`·`CertificateFile`·`SendEnv`·`SetEnv`·`LocalForward` 등)는 첫 값 우선이 아니라 순서대로 쌓인다(OpenSSH와 같다).
- `ConnectionAttempts`·`NumberOfPasswordPrompts`처럼 설정값으로 두지 않은 동작값은 지시어로만 정해지고, 없으면 OpenSSH 기본값(`ConnectionAttempts` 1 `readconf.c:2922`, `NumberOfPasswordPrompts` 3 `:2924`)이다. 둘은 아래 `ResolvedTarget.sources` 목록 밖이라 연결 정보의 `resolved.sources`에는 나오지 않는다. 라이브러리 설정값 규칙의 "조회"를 이 둘에 어떻게 채울지는 [후속 작업과 미정](#후속-작업과-미정)의 "설정값이 아닌 동작값의 조회"다.
- 점프 호스트: OpenSSH는 `ProxyJump`를 `ssh -l <사용자> -p <포트> -J <나머지> -F <설정 파일> -W '[%h]:%p' <홉>` 명령으로 바꾼다(`ssh.c:1278`–`:1324`). 홉에는 `-o` 옵션이 하나도 넘어가지 않는다. 내장 백엔드도 같다: 홉은 같은 설정 파일로 자기 `Host`·`Match`를 해석하고, `ProxyJump` 문자열의 사용자·포트만 받는다. 위 1~8은 최종 목적지에만 적용된다. 홉의 해석 결과는 `resolved.proxy_jump`에 홉마다 있다. 같은 홉·포트·사용자가 목적지와 같으면 OpenSSH처럼 `jumphost loop`로 끝낸다(`ssh.c:1289`–`:1291`).

## 터널·exec·SFTP

### 용도마다 해석하고, 같을 때만 세션을 나눈다

OpenSSH의 `Match command`·`Match sessiontype`은 용도와 명령에 따라 호스트·사용자·키를 바꿀 수 있다(`readconf.c:850`–`:869`). 그래서 해석은 연결 전에 용도마다 한다.

세션 종류는 두 백엔드 모두 용도가 첫 값으로 정한다. OpenSSH에서 `-N`·`-s`는 명령행에서 `SessionType`을 정하므로(`ssh.c:1030`–`:1033`·`:1048`–`:1051`) 설정 파일의 `SessionType`이 이기지 못하지만, 명령만 준 `ssh <대상> <명령>`에서는 설정 파일의 `SessionType none`이 이겨 명령을 실행하지 않고 `Match sessiontype none`이 참이 된다. 그래서 시스템 백엔드는 세 용도의 세션 종류(`-N`, `-s`, `-o SessionType=default`)를 대상 옵션과 `ssh.overrides`보다 앞에 넣고([옵션 적용 순서](#옵션-적용-순서) 4), 내장 백엔드도 같은 첫 값으로 해석한다. 그러면 `Match sessiontype`의 판정 값이 아래 표와 같다.

| 용도 | `sessiontype` | `command`(Match의 판정 값) | OpenSSH의 같은 명령 |
|---|---|---|---|
| 터널 | `none` | 빈 값 | `ssh -N <대상 옵션> -L … <대상>` |
| 포트 발견 | `exec` | 발견 명령 문자열 | `ssh -o SessionType=default <대상 옵션> <대상> <명령>` |
| exec | `exec` | 앱이 준 명령 문자열 | `ssh -o SessionType=default <대상 옵션> <대상> -- <명령>` |
| SFTP | `subsystem` | `sftp` | `ssh -s <대상 옵션> <대상> sftp` |

- exec의 명령은 비어 있으면 안 된다. 빈 명령은 OpenSSH에서 셸 세션(`sessiontype` `shell`)이 되지만 이 API는 대화형 셸을 열지 않으므로 `exec`가 연결 전에 `ExecError::EmptyCommand`로 거절한다.
- 내장 백엔드의 세션 키는 해석 결과 가운데 연결을 정하는 값의 묶음이다: 실제 호스트·포트·사용자, `ProxyJump` 홉별 해석 결과, 호스트 키 판정 값(`HostKeyAlias`·known_hosts 파일·`StrictHostKeyChecking`·`CheckHostIP`·`RevokedHostKeys`·`RequiredRSASize`), 알고리즘 목록, 인증 값(`IdentityFile`·`CertificateFile`·`IdentitiesOnly`·`IdentityAgent`·`PreferredAuthentications`와 방법별 켜기, 대상에 준 메모리 키의 공개키 지문 목록([android 요구](#android-요구의-자리) #2)), `BindAddress`·`AddressFamily`·`ConnectTimeout`·`ServerAlive*`·`RekeyLimit`. 키가 같은 열린 세션이 있으면 그 위에 채널을 연다. 다르면 새 세션을 연다. 메모리 키가 다른 두 대상은 서버·사용자가 같아도 세션을 나누지 않는다(한 키의 권한으로 다른 키의 호출이 돌지 않게).
- 채널마다 다른 값(`SendEnv`·`SetEnv`·`RemoteCommand`·`RequestTTY`)은 세션 키에 들지 않고 그 채널에만 쓴다.
- 그래서 터널과 exec가 `Match command`로 다른 키를 고르는 설정이면 두 세션이 생긴다. 같은 값을 고르면 세션 하나다. transport의 공유 키(대상 레코드)는 세션 키와 따로이고, 같은 공유 키 아래에 세션이 둘일 수 있다.
- 세션은 그 위 핸들(터널·`SshExec`·`SftpClient`)이 하나라도 살아 있는 동안 유지되고, 마지막 핸들이 닫히면 닫힌다. 세션을 붙들어 두는 시간은 두지 않는다(필요하면 android 요구와 함께 정한다, [후속 작업](#후속-작업과-미정)).

### 포트 발견과 터널의 대상 일치

용도마다 해석하므로 포트 발견과 터널이 `Match command`·`Match sessiontype`으로 다른 대상에 갈 수 있다. 발견한 레코드에는 토큰이 있고, handshake v2는 상대 기기 서명을 확인하기 전에 첫 메시지로 토큰을 보낸다([handshake v2](handshake.md)). 그래서 레코드는 발견과 같은 유효 대상으로 가는 터널에만 쓴다(android 요구 #7+).

- 비교하는 값: 실제 호스트·포트·사용자와 `ProxyJump` 홉마다의 같은 세 값(시스템 백엔드는 아래처럼 홉 명세 문자열까지). 세션 키의 첫 두 묶음과 같다(위).
- 비교하는 자리: 발견 결과로 터널을 만들거나 공유 터널을 다시 쓰고 레코드의 토큰을 넣는 transport 경로(`tasty-net-transport/src/ssh.rs`). 터널을 열거나 다시 쓰기 전, 토큰을 보내기 전에 비교한다. 다시 쓰는 공유 터널은 그 터널을 열 때의 해석 결과와 비교한다. 연결 전 설정 조회(`inspect`)의 결과로는 비교하지 않는다. 실제로 쓴 해석이 아니기 때문이다.
- 내장 백엔드는 두 용도의 해석 결과를 그대로 비교한다. 홉마다의 값도 이 레포가 해석하므로 홉별로 비교한다.
- 시스템 백엔드(director 2937): 용도마다 실제 자식과 같은 인자로 `ssh -G`를 돌려 그 해석의 `hostname`·`port`·`user`와 `proxyjump`를 비교한다. 다르면 토큰을 보내기 전에 `TargetMismatch`로 끝낸다. `proxyjump`는 설정에 적힌 홉 명세를 다시 짠 문자열이다(`readconf.c:3943`–`:3965`). 각 홉의 설정은 그 홉을 여는 `ssh` 프로세스가 따로 해석하므로(`ssh.c:1307`–`:1325`) 최종 목적지의 `ssh -G`(`ssh.c:1612`–`:1615`)로는 홉마다의 유효 호스트·포트·사용자를 얻지 못한다. 그래서 시스템 백엔드의 홉 비교는 홉 명세 문자열 비교까지다.
- 시스템 백엔드의 한계: 사용자 설정의 `Match exec` 명령이 `ssh -G` 때 한 번 더 실행된다. 두 `ssh -G`와 실제 자식 사이에 설정 파일이 바뀌면 판정이 늦을 수 있다(바뀐 설정은 그다음 시도에서 걸린다).
- 다르면 연결 시도는 `ConnectError::TargetMismatch`로 끝난다(transport의 새 변형, `ConnectError`는 `#[non_exhaustive]`). 설정이 같으면 다시 걸어도 결과가 같으므로 재시도하지 않는다: `HandshakeError::is_retryable`이 `Connect`에 싸인 이 변형도 `Cancelled`·`PeerTooOld`처럼 false로 본다(`reject.rs:259`). FFI는 같은 이름의 변형으로 넘긴다([모바일 바인딩 층](mobile.md#android-요구의-자리) #7+).
- 연결 목록 원인은 `Cause::TargetMismatch`(`target_mismatch`)이고, 그 시도는 다시 걸지 않는다. 비교에 쓰는 `ssh -G`는 터널을 열기 전에 포트 발견용 인자로 한 번, 터널용 인자로 한 번 돈다(`SshConnector::discovery_target`, `SshConnector::tunnel`의 `expected`). 그 `ssh -G`가 실패하면 시도는 `ConnectError::Tunnel(TunnelError::Backend)`로 끝난다.
- 내장 백엔드의 비교는 아직 없다. `discovery_target`이 내장 백엔드에서 `None`을 돌려주면 터널은 비교 없이 열린다. 내장 쪽 #7+는 exec·SFTP와 같은 다음 커밋 묶음이 넣는다.

### 내장 백엔드: 세션 하나에 채널 여럿

| 용도 | SSH 채널 | russh 0.65.4 |
|---|---|---|
| 터널 | 로컬 루프백 포워딩 종단(`127.0.0.1:<포트>`)에 온 TCP 연결마다 `direct-tcpip` | `channel_open_direct_tcpip` |
| 포트 발견 | `session` 채널 + `exec`(지금과 같은 원격 명령), stdout·stderr·종료 상태를 지금의 분류 규칙(`discovery.*`)으로 읽는다 | `Channel::exec`(`channels/mod.rs:237`·`:542`) |
| exec | `session` 채널 + `exec`. 표준 입력 쓰기, stdout·stderr 읽기, `exit-status`·`exit-signal` | 같다, `ChannelMsg::ExitStatus`(`:95`) |
| SFTP | `session` 채널 + `subsystem sftp`, 그 채널 스트림 위에 russh-sftp `SftpSession::new_with_config` | `Channel::request_subsystem`(`:251`·`:552`), `Channel::into_stream`(`:663`) |

- 터널의 수명·공유 키·`segments.local_forward` 구간은 두 백엔드가 같다. 루프백 포트를 거치지 않는 프로세스 안 스트림은 이 계약 밖의 후속 최적화다.
- 세션이 끊기면 그 위 채널이 모두 끝난다. 터널은 `TunnelExited`, 실행 중인 exec는 `ExitStatus::SessionLost`, SFTP는 `SftpError::SessionLost`다. transport 재연결은 새 세션을 연다(`session_id`가 바뀐다).
- 서버가 채널 열기를 거절하면(`SSH_MSG_CHANNEL_OPEN_FAILURE`, 예: sshd `MaxSessions`) 그 채널만 `ChannelOpenRefused{code}`로 실패하고 세션은 남는다. russh 0.65.4는 채널 열기 오류에 사유 코드만 싣는다(`Error::ChannelOpenFailure(reason)`, `client/mod.rs:767`). 서버의 설명 문구는 `Handler::channel_open_failure(channel, reason, description, …)`(`:2942`)로 따로 오는데, 세션 루프가 호출자에게 실패를 먼저 보낸 뒤 이 콜백을 부르고(`client/encrypted.rs:598`–`:605`) 실패한 호출자는 채널 ID를 받지 못한다. 그래서 설명 문구를 그 오류에 붙인다는 계약은 두지 않는다. 붙일 방법(요청 순서로 맞추기 등)이 있는지는 [후속 작업](#후속-작업과-미정)이 정한다.
- 리스너 판정: 시스템 백엔드는 로컬 연결 끊김과 ssh stderr의 `open failed: connect failed` 줄이 함께 있을 때만 `RemoteListenerAbsent`로 본다(`tasty-net-ssh/src/listener.rs:110`–`:114`). 내장 백엔드는 `direct-tcpip` 열기 실패의 사유 코드가 2(`SSH_OPEN_CONNECT_FAILED`)일 때만 같은 판정을 내고, 그 로컬 연결을 닫는다. 코드 1(`ADMINISTRATIVELY_PROHIBITED`)·3(`UNKNOWN_CHANNEL_TYPE`)·4(`RESOURCE_SHORTAGE`)는 리스너 없음이 아니라 `ChannelOpenRefused`로 handshake의 연결 실패가 된다.

### 출력 상한과 세션 수신 루프

russh 0.65.4는 받은 데이터만큼 수신 창을 먼저 늘리고(`client/encrypted.rs:619`·`:646`) 그다음 채널 큐에 넣는다(`:629`·`:657`, `send(…).await`). 큐 크기는 `channel_buffer_size`다(`client/mod.rs:846`). 그래서 앱이 읽지 않아도 창은 줄지 않고, 큐가 차면 세션 전체의 수신 루프가 기다린다. 같은 세션의 터널·SFTP도 함께 멈춘다. 수신 창으로 채널 하나만 늦추는 계약은 이 판에서 지킬 수 없다.

- 계약: 내장 백엔드는 채널마다 russh 큐를 늘 바로 비우는 작업을 하나 둔다(앱이 읽든 안 읽든). 비운 데이터는 채널마다의 버퍼(stdout·stderr 각각 `exec.output_buffer`)에 쌓는다. 그래서 russh 큐가 차서 세션 수신 루프가 멈추는 일이 없다.
- 한 스트림에 앱이 읽지 않은 데이터가 상한을 넘으면 그 exec만 끝낸다: 채널을 닫고(`SIGTERM`을 보내는 `signal` 요청은 하지 않는다. 서버마다 다르다) `wait`가 `ExecError::OutputLimitExceeded{stream}`을 돌려준다. 이미 쌓인 데이터는 버리지 않는다. 같은 세션의 다른 exec·SFTP 채널은 영향을 받지 않는다. 정확히 상한만큼의 출력이 성공인지, 남은 바이트와 오류를 읽기 쪽(`Read`의 `io::Error`)에서 어떤 순서로 보는지는 [후속 작업](#후속-작업과-미정)이 정한다.
- 터널 채널(`direct-tcpip`)은 받은 데이터를 로컬 TCP 연결에 쓴다. 로컬 쪽이 읽지 않으면 그 채널의 russh 큐가 차고 같은 이유로 세션 수신 루프 전체가 멈춘다. 로컬 쪽은 이 레포의 링크 읽기라 보통 늘 읽지만, 터널에는 출력 상한 같은 끊기가 없다. 알려진 한계로 둔다.
- 시스템 백엔드도 같은 계약이다: 자식의 stdout·stderr 파이프를 늘 읽는 스레드가 같은 상한의 버퍼에 쌓고, 넘치면 자식을 끝내고 같은 오류를 낸다. 두 백엔드에서 결과가 같다.
- `wait`는 핸들을 빌린다(`&mut self`). 종료 상태가 오면 돌려주고, 읽지 않은 출력은 버퍼에 남아 그 뒤에도 읽을 수 있다. 출력과 상태를 한 번에 받는 `wait_with_output(self)`도 둔다.
- SFTP는 요청·응답 방식이라 이 문제가 없다. 응답을 기다리는 요청 수가 `sftp.max_concurrent_reads`·`writes`로 묶인다.

### 시스템 ssh 백엔드: 기능마다 자식 하나

| 용도 | 자식 명령 | 결과 |
|---|---|---|
| 터널 | `ssh <1~3> -N <4의 -o> <5~8> -L … <대상>`. 지금 명령에서 `-N`의 자리만 대상 옵션 앞으로 옮긴다 | 지금과 같다(대상 옵션에 `SessionType`이 있던 경우만 실패하지 않게 된다) |
| 포트 발견 | `ssh <1~3> -o SessionType=default <4의 나머지> <5~8> <대상> <명령>`. 지금 명령에 세션 공유 끄기 두 옵션과 세션 종류 고정이 더해진다. 8.7 미만으로 판정된 실행 파일이면 `SessionType` 없이 실행 | 지금과 같다 |
| exec | `ssh <1~3> -o SessionType=default <4의 나머지> <5~8> <대상> -- <명령>`. 표준 입출력은 자식의 파이프. 8.7 미만 판정은 포트 발견과 같다(다시 실행하지 않는다) | 자식의 종료 코드가 0~254면 `ExitStatus::Code(n)`. 255는 원격 명령의 255와 ssh 자체 오류를 가를 수 없어 `ExitStatus::Ambiguous255`(stderr를 함께 준다). OpenSSH 클라이언트는 `exit-signal`을 처리하지 않아 신호 종료도 255로 끝나므로 신호 이름을 주지 못한다 |
| SFTP | `ssh <1~3> -s <4의 나머지> <5~8> -E <임시 파일> <대상> sftp`(`-E`는 아래 오류 분류). 자식의 stdin·stdout을 tokio 쪽 스트림으로 잇고(파이프를 읽고 쓰는 스레드 둘, tokio `process` feature는 쓰지 않는다) 그 위에 russh-sftp `SftpSession::new_with_config(stream, config)` | 내장과 같은 `SftpClient` |

- 위 표의 번호는 [옵션 적용 순서](#옵션-적용-순서)의 번호다. 4의 세션 종류와 세션 공유 끄기 `ControlMaster=no`·`ControlPath=none`은 대상 옵션 앞에 넣는다. 그래서 사용자 설정의 `ControlMaster auto`·`ControlPath`가 있어도 기존 다중화 소켓을 쓰지 않는다. 기존 터널 코드가 두 옵션을 함께 두는 근거와 같다(`tunnel.rs:87`–`:100`: 다중화된 연결은 이 자식이 아니라 마스터 프로세스의 것이 된다). 지금의 포트 발견 명령에는 두 옵션이 없다(`discovery.rs`에 `Control` 0건, 대조: `tunnel.rs`의 `TUNNEL_FORCED_OPTIONS`(`:95`)에는 둘 다 있다). 내장 백엔드 작업이 더한다.
- 그래서 기능마다 SSH 세션과 인증이 따로다. 키에 passphrase가 있으면 기능마다 묻는다(agent가 있으면 묻지 않는다). 자식이 끝나면 세션도 끝난다. 내장 백엔드와 다른 점으로 둔다.
- 4의 나머지 터널 고정 옵션(`ExitOnForwardFailure`·`ClearAllForwardings`·fork 금지)은 터널 자식에만 붙인다.
- russh-sftp는 russh에 의존하지 않고 아무 `AsyncRead + AsyncWrite` 스트림을 받는다(아래 [의존 사실](#의존-사실)). 그래서 두 백엔드가 같은 SFTP 구현과 같은 설정값을 쓴다: 두 백엔드 모두 `SftpSession::new`(기본 `Config`를 쓴다, `client/session.rs:39`–`:44`)가 아니라 `new_with_config`에 `sftp.*`로 만든 `Config`를 준다. 시스템 백엔드의 SFTP는 `system-ssh`와 `embedded-ssh`가 함께 켜진 빌드에서만 russh-sftp가 들어온다. `system-ssh`만 켠 1단계 빌드에는 SFTP가 없다. 두 백엔드를 함께 켠 빌드(기본 빌드로는 [내장 폴백 활성 기준](done-criteria.md#내장-폴백-활성-기준) 뒤)의 데스크톱 소비자는 두 백엔드에서 SFTP를 쓴다. 그 전의 기본 빌드(`system-ssh`만)에는 SFTP가 없다. 그래서 `SftpClient`는 `embedded-ssh`가 켜진 빌드에만 있다([공통 항목과 백엔드 전용 항목](#공통-항목과-백엔드-전용-항목)).

### 공개 API (`tasty-net-ssh`)

```text
SshConnector::exec(&self, target: &SshTarget, command: &str, options: ExecOptions) -> Result<SshExec, ExecError>
SshExec::stdin(&mut self) -> &mut dyn Write;  stdout()/stderr() -> &mut dyn Read
SshExec::wait(&mut self, timeout: Option<Duration>) -> Result<ExitStatus, ExecError>
SshExec::wait_with_output(self, timeout: Option<Duration>) -> Result<ExecOutput, ExecError>
SshExec::cancel(&self)                        // 내장: 채널 닫기, 시스템: 자식 종료
enum ExitStatus { Code(u32), Signal { name, core_dumped, message }, Ambiguous255 { stderr_tail }, SessionLost, NoStatus }
SshConnector::sftp(&self, target: &SshTarget) -> Result<SftpClient, SftpError>   // embedded-ssh 빌드
SftpClient: open / create / read / read_at(offset, len) / write / read_dir / metadata / symlink_metadata / read_link / set_metadata / rename / remove_file / remove_dir / create_dir / canonicalize / close
```

- 용도마다 해석하므로 세션을 먼저 여는 `session()` API는 두지 않는다. `exec`·`sftp`가 해석하고 세션 키로 열린 세션을 찾거나 연다.
- 동기 API다. 막히는 호출은 내장 백엔드의 소유 런타임에서 돌고 호출자는 결과를 기다린다([전송 추상화](transport.md#동기비동기-경계)).
- `ExitStatus::NoStatus`는 서버가 `exit-status`·`exit-signal` 없이 채널을 닫은 경우다(RFC 4254 §6.10은 보내는 것을 SHOULD로 둔다).
- `ExecOptions`: 환경 변수(`SendEnv`·`SetEnv`와 같은 `env` 요청), 표준 입력 닫기. 출력 상한은 설정값 `exec.output_buffer`다.
- 바인딩 API(Kotlin·Swift에 무엇을 낼지)는 tasty-android의 요구 목록을 받아 정한다([모바일 바인딩 층](mobile.md#바인딩-층)).

### SSH 세션 목록

android 요구 #6b(attach 없는 SSH 세션의 수명·끊김을 보기)를 받는 자리다. 경계를 이렇게 정한다.

- 목록은 `tasty-net-ssh`의 `SshConnector`가 갖는다. exec·SFTP는 `SshConnector::exec`·`sftp`가 transport를 거치지 않고 열고, ssh는 transport에 의존하지 않기 때문이다([크레이트 구성](crates.md#의존-방향)). 그래서 ssh에서 transport로 가는 경로를 새로 두지 않는다.
- transport의 [연결 목록](transport.md#연결-목록)은 바꾸지 않는다. 단위는 그대로 "물리 연결(Tasty 링크) 하나 = 항목 하나"이고, SSH를 띄운 쪽 항목은 터널 하나다. 그 항목의 `ssh.session_id`가 세션 목록의 같은 세션을 가리킨다.
- 세션 목록의 단위는 커넥터가 연 SSH 세션 하나다: 내장은 SSH 세션 하나(터널·exec·SFTP 채널을 함께 가질 수 있다), 시스템은 `ssh` 자식 하나(기능마다 따로다). 그래서 터널이 있는 내장 세션은 연결 목록에는 터널 항목으로, 세션 목록에는 세션 항목으로 두 곳에 나온다. 둘은 보는 단위가 다르다.
- 모양은 연결 목록의 관례를 따른다: `SshConnector::sessions()`(지금 열린 세션과 보관 상한 안의 닫힌 세션), `SshConnector::session_events()`(`next(timeout)`, `Lagged{dropped}`), 항목마다 세션 번호·백엔드·해석한 호스트·포트·사용자·그 위 핸들 수(터널·exec·SFTP)·상태·닫힌 사유·`is_retryable()`. 필드 표와 보관 상한은 [후속 작업](#후속-작업과-미정)이 정한다.
- FFI는 transport의 연결 목록과 이 세션 목록을 둘 다 의존하므로, 앱에 하나의 이벤트 흐름으로 합쳐 낼 수 있다(합치는 것은 ffi의 몫, [모바일 바인딩 층](mobile.md#android-요구의-자리)).

### 설정 조회

사용자 결정(백엔드와 선택 이유가 설정 조회에도 나온다, 읽었지만 지원하지 않는 지시어를 설정 조회에 보인다)을 이 경로로 지킨다.

```text
SshConnector::inspect(&self, target: Option<&SshTarget>, purpose: SshPurpose) -> SshInspection
enum SshPurpose { Tunnel, Discovery, Exec { command }, Sftp }
struct SshInspection {
    settings: Vec<Setting>,                              // {key, value, source}
    backend: Result<SelectedBackend, SshBackendError>,   // 대상이 없으면 런처 프로그램 기준. 선택 규칙의 결과 그대로
    resolver: ResolverState,                             // embedded | embedded_preview | not_built. 대상이 없어도 늘 있다
    files: Vec<ConfigFileReport>,                        // 읽은 파일마다 {path, loaded | missing | refused{line, keyword}}
    file_notes: Vec<ConfigNote>,                         // 파일 단위 거부(모르는 지시어)와 OpenSSH가 버린 지시어
    target: Option<TargetInspection>,                    // 대상을 주면: {resolved, notes, refusal}
}
```

- 연결하지 않고 다른 프로그램을 실행하지 않는다. 설정 파일과 known_hosts를 읽기만 한다.
- 대상을 주면 그 용도로 해석한 결과(`resolved`, 대상 단위 `ConfigNote`, 거부 사유)를 싣는다. 해석이 거부되면 `refusal`에 지시어·파일·줄이 있고 `resolved`는 거부 전까지의 값이다.
- 빌드 조합마다 싣는 것이 다르다. 해석기(`tasty-net-ssh-config`)는 `embedded-ssh`로만 들어오기 때문이다.

  | 빌드 | `settings`·`backend`·`resolver` | `files`·`file_notes`·`target` |
  |---|---|---|
  | `embedded-ssh`가 있음(모바일, 내장 폴백 활성 기준 뒤의 기본) | 싣는다. `resolver`는 내장을 고르면 `embedded`, 시스템을 고르거나 선택이 실패하면(`backend`가 `Err`) `embedded_preview` | 싣는다. 시스템 백엔드를 고른 경우에도 내장 해석기로 같은 진단을 싣고, 실제 연결은 `ssh`가 하므로 `target.resolved`는 참고 값이다 |
  | `system-ssh`만(1단계와 내장 폴백 활성 기준 전의 기본) | 싣는다. `backend`는 [선택 규칙](#선택-규칙)의 결과 그대로다: 찾으면 `Ok`(시스템, 이유와 찾은 경로), 못 찾으면 `Err(NotFound{searched})`. `resolver`는 `not_built` | 비어 있다(`files`·`file_notes`는 빈 목록, `target`은 대상을 주어도 `None`). 지시어 분류는 내장 백엔드의 것이라 이 빌드에는 표시할 미지원 지시어가 없다. 시스템 `ssh -G`를 불러 진단을 채우지 않는다(`inspect`는 다른 프로그램을 실행하지 않는다) |
- FFI는 이 구조를 그대로 옮겨 낸다([모바일 바인딩 층](mobile.md#바인딩-층)).

## OpenSSH 해석 이식 크레이트

새 크레이트 `tasty-net-ssh-config`(`crates/tasty-net-ssh-config`)가 OpenSSH의 설정 해석과 known_hosts 해석을 Rust로 옮긴 코드를 갖는다.

### 기준 판

| 항목 | 값 |
|---|---|
| 판 | OpenSSH portable 10.6p2 |
| 태그 | `V_10_6_P2`(태그 객체 `e9c0339ed598bd96aa814be2c776b6c3c2357ab4`) |
| 커밋 | `d449e662b6b8b8cb20e852d8084c269af1c5e8f1`(커밋 시각 2026-11-05 +1100, `version.h`: `SSH_VERSION` `OpenSSH_10.6`, `SSH_PORTABLE` `p2`, `SSH_RELEASE` `OpenSSH_10.6p2`) |
| 저장소 | `https://github.com/openssh/openssh-portable` |

- 기준 판을 올리는 일은 그 판의 원본 차이를 이식 크레이트에 옮기고, 비교 시험을 그 판의 클라이언트로 다시 돌리는 일이다. 근거 보고서는 master `8038f615`의 줄 번호를 썼다. 이 문서의 줄 번호는 위 커밋 기준이다.

### 옮기는 원본

| 원본 파일(줄 수) | 옮기는 것 |
|---|---|
| `readconf.c`(3968), `readconf.h`(268) | 키워드 표 `keywords[]`(`:178`–`:325`), 줄 해석 `process_config_line_depth`(`:1143`), `Match` 판정 `match_cfg_line`(`:700`), 파일 읽기 `read_config_file_depth`(`:2631`, 상한 `READCONF_MAX_DEPTH` `:2629`), `initialize_options`(`:2714`), `fill_default_options`(`:2860`), `parse_forward`(`:3299`), `parse_jump`(`:3474`), `dump_client_config`(`:3739`, 비교 시험의 출력 형식) |
| `addrmatch.c`(166)·`addr.c`(510)의 해당 함수 | `Match localnetwork` 목록 검사: `addr_match_cidr_list`(`:106`, 주소 없이 부를 때의 항목 검사), `addr_pton_cidr`(`addr.c:460`)·`masklen_valid`(`:50`)·`addr_host_is_all0s`(`:334`). `addr_pton`의 `getaddrinfo(AI_NUMERICHOST)`는 이 크레이트의 `numeric_host`(아래 라이선스 절)가 대신한다 |
| `match.c`(406) | `match_pattern`(`:82`), `match_pattern_list`(`:161`), `match_hostname`(`:233`), `match_host_and_ip`(`:250`), `match_user`(`:275`), `match_list`(`:313`), `match_filter_denylist`·`allowlist`(`:393`·`:403`) |
| `hostfile.c`(982), `hostfile.h`(123) | `extract_salt`(`:69`), `host_hash`(`:116`), `hostfile_read_key`(`:158`), `check_markers`(`:181`), `load_hostkeys_file`(`:266`), `check_key_not_revoked`(`:316`), `check_hostkeys_by_key_or_type`(`:350`), `check_key_in_hostkeys`(`:408`), `lookup_key_in_hostkeys_by_type`(`:417`), `lookup_marker_in_hostkeys`(`:425`), `format_host_entry`(`:437`), `write_host_entry`(`:477`), `add_host_to_hostfile`(`:542`), `match_maybe_hashed`(`:770`), `hostkeys_foreach_file`(`:790`), `hostkeys_foreach`(`:966`). `format_host_entry`·`write_host_entry`는 `add_host_to_hostfile`이, `match_maybe_hashed`·`hostfile_read_key`는 `hostkeys_foreach_file`이 부르는 보조 함수라 판정 순서와 기록 형식을 같게 하려면 함께 옮긴다. 이식 2단계는 읽기와 판정 쪽(`extract_salt`~`check_hostkeys_by_key_or_type`, `match_maybe_hashed`, `hostkeys_foreach_file`의 `HKF_WANT_MATCH` 갈래)을 옮겼다. 기록 쪽(`format_host_entry`·`write_host_entry`·`add_host_to_hostfile`)은 `accept-new` 기록과 함께 남았다([후속 작업과 미정](#후속-작업과-미정)) |
| `sshkey.c`의 키 읽기, `ssh-ed25519.c`·`ssh-ed25519-sk.c`·`ssh-mldsa-eddsa.c`·`ssh-ecdsa.c`·`ssh-ecdsa-sk.c`·`ssh-rsa.c`의 공개키 읽기, `sshbuf-getput-basic.c`·`sshbuf-getput-crypto.c`·`sshbuf-misc.c`, `sshbuf.h`·`sshkey.h`·`ssh2.h`·`crypto_api.h`의 상수 | known_hosts 줄의 키를 `ssh`처럼 읽고 비교하는 부분: `peek_type_nid`(`sshkey.c:1277`)·`sshkey_read`(`:1298`)·`sshkey_from_blob_internal`(`:2004`)·인증서 본문 `cert_parse`(`:1854`, CA 서명 확인 `sshkey_verify` 앞까지)·`sshkey_equal_public`·`sshkey_equal`(`:821`·`:834`), 형식마다 `deserialize_public`, `sshbuf_get_string_direct`·`sshbuf_get_cstring`·`sshbuf_get_bignum2_bytes_direct`, `get_ec`의 점 검사, `sshbuf_b64tod`(`sshbuf-misc.c:183`)가 푼 blob의 크기 상한 `SSHBUF_SIZE_MAX`. 줄 범위는 크레이트 README의 표다 |
| `misc.c`의 `put_host_port`(`:801`), `sshconnect.c`의 `get_hostfile_hostname_ipaddr`(`:648`)와 `check_host_key`의 포트 없는 재조회(`:1181`–`:1193`) | 호스트 키를 찾는 이름(`HostKeyAlias`, 아니면 `[호스트]:포트`, 포트 0·22는 호스트만)과, 그 이름이 `HOST_NEW`일 때 다시 보는 포트 없는 이름 |
| `misc.c`(3352)의 해당 함수 | `a2port`(`:592`), `convtime`(`:746`), `parse_user_host_path`(`:914`), `tilde_expand`(`:1276`)·`tilde_expand_filename`(`:1344`), `dollar_expand`(`:1477`), `percent_expand`(`:1496`)·`percent_dollar_expand`(`:1515`), `argv_split`(`:2143`), `valid_env_name`(`:2532`) |
| `kex-names.c`(334), `myproposal.h`(124) | `kex_assemble_names`(`:241`, `+`·`-`·`^` 연산), 기본 알고리즘 목록 |
| `sshconnect.c`(1809)·`sshconnect2.c`(2508)의 판정 부분 | `check_host_key`(`:984`)의 known_hosts 판정 순서, `verify_host_key`(`:1499`)의 `RevokedHostKeys` 처리(`:1544`–`:1563`), `order_hostkeyalgs`(`sshconnect2.c:121`–`:216`, known_hosts에 있는 키 형식을 앞으로), RSA 크기 검사 자리(`sshconnect2.c:96`·`:1593`·`:1847`) |
| `ssh.c`(2510)의 설정 단계 | `tilde_expand_paths`(`:189`–`:199`), `default_client_percent_expand`(`:207`–`:213`), `is_addr`(`:293`–`:315`)·`resolve_addr`(`:324`–`:367`, 숫자 주소만), `process_config_files`(`:564`–`:594`, 사용자·시스템 설정을 읽는 순서와 `final` 두 번째 읽기), `main`의 설정 단계(`:1186`–`:1612`: 첫 읽기 `:1187`, `HostName`의 `%h` 펼침 `:1195`–`:1203`, 호스트 소문자화 `:1206`–`:1207`, CNAME 따라가기 `:1229`–`:1240`, `final` 다시 읽기 `:1246`–`:1265`, `fill_default_options` `:1267`, 기본 사용자, `ProxyJump` → 홉 명령과 `jumphost loop` `:1278`–`:1324`, `ConnectionAttempts` 검사 `:1360`, `RemoteCommand` 충돌 `:1363`–`:1364`, `RemoteCommand`와 경로 지시어의 `~`·`%` 펼침 `:1457`–`:1611`, `ssh -G` 종료 `:1612`–`:1615`), 그 뒤 연결 단계의 `load_public_identity_files`(호출 `:1646`, 함수 `:2355`–`:2497`, `IdentityFile`·`CertificateFile`의 펼침과 순서)와 known_hosts 파일의 `~` 펼침(`:1791`–`:1794`) |
| `openbsd-compat/fmt_scaled.c`(324)·`vis.c`(253)·`glob.c`(1077)·`charclass.h`(31)의 해당 함수 | `scan_scaled`(`:76`, `RekeyLimit`), `vis`(`:62`, `EscapeChar`의 `ssh -G` 출력), `glob`(`:180`)·`glob0`(`:473`)·`glob1`~`glob3`(`:612`~`:698`)·`match`(`:904`)·`g_charclass`(`:439`)와 `cclasses[]`(`Include`) |
| `version.h`(6) | `SSH_RELEASE`(`Match version`의 판정 값) |
| `cipher.c`(473), `mac.c`(255), `sshkey.c`(3732)의 이름 표 | `ciphers[]`·`cipher_alg_list`·`ciphers_valid`, `macs[]`·`mac_alg_list`·`mac_valid`, `keyimpls[]` 이름·`sshkey_alg_list`·`sshkey_names_valid2`(알고리즘 지시어의 값 검사와 `+`·`-`·`^`의 전체 목록) |
| `log.c`(656), `digest-openssl.c`(207)의 이름 표 | `log_levels[]`·`log_facilities[]`와 이름 변환(`LogLevel`·`SyslogFacility`), `digests[]` 이름(`FingerprintHash`) |
| `pathnames.h`(194), `sshconnect.h`(108), `defines.h`(1026) | 기본 경로(`_PATH_SSH_USER_DIR`, 기본 키 파일, known_hosts), `%` 토큰 목록 `DEFAULT_CLIENT_PERCENT_EXPAND_ARGS`, `IPTOS_*` 값 |

- 옮기는 것은 해석과 판정이다. 소켓·암호·서명 검증·프로세스 실행은 옮기지 않는다(그 자리는 russh와 거부 규칙이다).
- 서비스 데이터베이스는 읽지 않는다. `a2port`(`misc.c:592`–`:604`)는 숫자 포트만 받는다. 원본은 `getservbyname`으로 서비스 이름(`Port ssh`)도 받지만, 이 크레이트는 그 줄을 설정 오류(`ConfigError`, 키워드 `port`)로 거부한다.
- 숫자 주소는 실행 OS의 C 라이브러리가 `getaddrinfo(AI_NUMERICHOST)`로 숫자로 읽는 형식을 그 라이브러리처럼 읽는다(director 3106). 기준판은 실행 OS의 libc를 쓰므로, 다른 규칙으로 읽으면 내장 백엔드가 같은 OS의 시스템 ssh와 다른 주소로 붙는다(`010.0.0.1`이 glibc에서는 `8.0.0.1`, macOS에서는 `10.0.0.1`). 라이브러리마다 다른 점은 다음과 같다.
  - glibc(Linux): `inet_aton` 형식(`a.b.c.d`·`a.b.c`·`a.b`·`a`, 조각마다 10진·`0x` 16진·`0` 8진, `__inet_aton_exact`)을 받는다. `AddressFamily inet`에서 IPv4 매핑 IPv6(`::ffff:a.b.c.d`)를 IPv4로 바꾼다. IPv6 그룹은 16진 네 자리까지, 점 표기 꼬리는 앞자리 0을 받지 않는다. 점 표기 꼬리의 첫 조각은 먼저 그룹 숫자로 세므로 네 자리를 넘으면 숫자가 아니다(bionic도 같다, `::ffff:00001.0.0.1`).
  - bionic(Android): `inet_pton`만 받는다(`explore_numeric`의 `inet_aton` 갈래는 `#if 0`). 네 조각 10진만 숫자이고 앞자리 0은 10진으로 읽는다(`010.0.0.1`→`10.0.0.1`). `127.1`·`0x7F000001`은 숫자가 아니라 적힌 그대로다. 요청한 패밀리만 읽으므로 `AddressFamily inet`의 IPv4 매핑 IPv6는 숫자가 아니다.
  - Darwin(macOS·iOS, Libinfo `_gai_numerichost`): 먼저 `inet_pton(AF_INET)`(앞자리 0은 10진), 안 되면 `_inet_aton_check(strict)`로 짧은 형식·16진·8진을 받는다(`0300.0.0.1`은 10진으로 255를 넘어 8진 `192.0.0.1`). `inet_aton` 쪽은 조각을 64비트로 읽어 넘치면 감기고, 한 조각 주소는 아래 32비트를 쓰고, `.` 앞의 숫자 없는 `0x`를 0으로 본다. 한 패밀리를 요청하면 다른 패밀리 주소는 숫자가 아니다(`AddressFamily inet`의 IPv4 매핑 IPv6 포함). IPv6 그룹은 값이 16비트 안이면 자리 수를 보지 않고(`00001::`), 점 표기 꼬리의 앞자리 0은 10진이다.
  - 그 밖의 OS(Windows, musl, BSD 계열): 이 크레이트에 규칙이 없다. 세 라이브러리가 같게 읽는 이름은 그 결과를 쓰고, 다르게 읽는 이름은 `Undeterminable`(키워드 `hostname`, 세 라이브러리의 결과를 적은 사유)로 거부한다. OpenSSH portable은 Windows에서 빌드되지 않는다(Windows의 ssh는 Microsoft의 Win32-OpenSSH 갈래다). 그 갈래와 ws2_32의 규칙은 확인하지 않았으므로 갈리는 형식은 거부하고, 셋이 같게 읽는 형식은 Windows도 같게 읽는다고 본다.
  - 표기(`getnameinfo(NI_NUMERICHOST)`, `inet_ntop`)는 세 라이브러리가 같다. Apple `inet_ntop6`의 "앞 일곱 워드가 0" 갈래는 그 워드가 0 묶음 안이라 닿지 않는다.
  - 예외: Darwin은 `%`가 없어도 링크 로컬 IPv6(`fe80::/10`)의 둘째 워드를 인터페이스 번호로 떼어 낸다(Libinfo `si_addrinfo_v6`). `getnameinfo`는 링크 로컬과 node·link-local 멀티캐스트 주소에서 그 번호의 인터페이스 이름을 붙여 쓰고(`fe80:1::1`→`fe80::1%lo0`), 그 인터페이스가 없으면 실패한다. 멀티캐스트의 구분은 xnu `in6.h`의 사용자 공간 매크로를 따른다. `IN6_IS_ADDR_MC_NODELOCAL`은 scope 1이면 flags를 보지 않고, `IN6_IS_ADDR_MC_LINKLOCAL`은 scope 2이면서 flags가 유니캐스트 접두사 기반(0x30)이 아닐 때다. 그래서 `ff32:40:fe80::1234`는 모든 OS에서 적힌 그대로 읽는다. 결과가 인터페이스에 따라 달라지므로, Darwin에서 둘째 워드가 0이 아닌 그 주소는 `Undeterminable`로 거부한다. 규칙 없는 OS에서도 세 결과가 같다고 보지 않고 거부한다.
  - `Match localnetwork`의 주소 목록(`addr_match_cidr_list`, `addr_pton_cidr`)도 같은 규칙으로 판정한다. 기준판처럼 주소를 읽기 전에 정해지는 오류(빈 항목, 길이, 글자, 마스크 숫자)는 모든 OS에서 목록 오류(`ConfigError`)다. 주소를 읽은 뒤의 판정(마스크 길이, 호스트 비트)은 라이브러리마다 하고, 규칙 없는 OS에서는 세 라이브러리가 모두 무효면 목록 오류, 모두 유효면 유효다. Darwin의 위 예외 주소는 `si_addrinfo_v6`가 둘째 워드를 지운 주소로 판정한다(`addr_pton`은 주소와 scope 번호만 옮기므로 인터페이스가 없어도 정해진다, `fe80:1::/16`은 Darwin에서 유효하고 glibc·bionic에서 무효). 멀티캐스트 주소는 둘째 워드를 지우지 않는다. 라이브러리마다 유효·무효가 갈리는 목록은 그 OS의 ssh가 Match 결과와 상관없이 목록 오류로 멈출 수 있으므로, 다른 조건이 거짓이어도 `Undeterminable`(키워드 `match localnetwork`, 사유에 라이브러리별 결과)로 거부한다. 목록은 유효하지만 라이브러리마다 주소가 갈리면(`010.0.0.0/8`) 블록 적용의 판정 불가 사유에 라이브러리별 결과를 덧붙인다.
  - scope가 붙은 IPv6(`fe80::1%eth0`, `%0001`)는 숫자 주소로 보지 않고 적힌 그대로 둔다(소문자로 바꾸지 않는다). 기준판은 OS 인터페이스 표로 scope를 이름으로 바꾼다(`%0001`→`%lo`). 다룰지는 [후속 작업과 미정](#후속-작업과-미정)의 "IPv6 scope의 정규화"다.
  - 시험: 사례마다 glibc·bionic·Darwin·규칙 없는 OS의 결과를 적은 OS별 기대값 표(`src/resolve.rs`의 `NUMERIC_FORMS`)를 두고, 세 라이브러리 열은 모든 OS에서, 실행 OS의 열은 그 OS에서 단언한다. 기준 클라이언트 대조 fixture에는 네 열이 모두 같은 사례만 둔다.
  - 기기 확인: 3faef7b의 bionic 열을 Android 기기 API 29·37의 실제 bionic `getaddrinfo(AI_NUMERICHOST)`·`getnameinfo(NI_NUMERICHOST)`로 확인했다. `NUMERIC_FORMS`는 62개 중 61개, `Match localnetwork` 목록 표는 14개 모두 일치했다. 어긋난 1개 `fe80::1%lo`는 bionic이 `%lo`를 scope로 받아 숫자로 읽은 것이고, 위 scope 행대로 이 크레이트가 일부러 적힌 그대로 두는 사례다(glibc에서도 같은 1개만 어긋난다).
- 키 비교는 형식과 공개 필드로 한다(`sshkey_equal`·`sshkey_equal_public`). known_hosts 줄의 base64를 풀어 blob의 구조(형식 이름, 형식마다 공개 필드의 길이와 형식, 남는 바이트 없음, 줄의 형식과 blob 형식이 같음, ECDSA 곡선 이름)를 `sshkey_read`처럼 검사하고, 서버가 보낸 blob도 같은 방법으로 읽어 비교한다. 푼 blob이 `SSHBUF_SIZE_MAX`(128 MiB)보다 크면 키가 아니다. RSA의 `mpint`는 앞 0 바이트를 떼고 비교하므로 blob 바이트가 달라도 같은 키는 같다. 인증서는 본문을 `cert_parse`처럼 읽는다: 일련번호·종류·키 ID·주체 이름(256개까지)·유효 기간·중요 옵션과 확장(문자열 쌍)·예약 필드·CA 키(인증서가 아닌 키)·서명(앞의 서명 형식 이름)이 모자라거나 잘렸거나 뒤에 바이트가 남으면 키가 아니고, 종류는 사용자(1)·호스트(2)만 받는다. 키 형식 크레이트에 의존하지 않는다. 의존은 `hmac`·`sha1`(해시 호스트 `|1|`), `base64`다.
- 암호 연산이 드는 검사 둘은 하지 않는다(새 의존 없이는 할 수 없다): ECDSA 점이 곡선 위에 있는지(`EC_POINT_oct2point`·`sshkey_ec_validate_public`)와 인증서의 CA 서명(`cert_parse`의 `sshkey_verify`, `sshkey.c:1976`)이다. 그런 키는 이 크레이트가 키로 읽고 기준판은 무효로 본다. 판정이 바뀌는 방향은 다음과 같다.
  - known_hosts 줄에 있을 때는 더 거절하는 쪽이다. 기준판은 그 줄을 건너뛴다(`hostfile_read_key` 실패, `hostfile.c:157`–`:178`). 표식 없는 줄이면 이 크레이트는 그 줄을 다른 키로 보아 기준판의 `HOST_NEW` 대신 `Mismatch`를 낸다. 인증서 줄은 평문 키와 형식이 달라 `Match`의 근거가 되지 않는다. `@revoked` 줄이면 그 공개키를 거절한다.
  - 서버가 보낸 blob이 그런 키일 때는 통과로 새는 경우가 있다. 기준판은 키 교환에서 그 blob을 키로 읽지 못해 판정 전에 끊는다(`sshkey_fromb_allowlist`, `kexgen.c:168`). `check`는 곡선 밖 점이 표식 없는 줄과 바이트가 같으면 `Match`를 낸다. CA 서명이 틀린 인증서는 폐기 확인 뒤 평문 키로 판정하므로, 그 평문 키가 표식 없는 줄에 있으면 `Match`를 낸다.
  - 그래서 `check`는 전제를 둔다: 서버 키 blob은 전송 계층(russh)이 이미 키로 읽고 서명을 확인한 것이어야 한다. `check`는 곡선 검사와 인증서 서명 확인을 하지 않는다. 남은 두 경우는 [후속 작업과 미정](#후속-작업과-미정)의 "곡선 검사·CA 서명"이다.
- 다른 프로그램을 실행하지 않는다. `Match exec`·`ProxyCommand`·`KnownHostsCommand`는 해석만 하고, 적용되면 거부 결과를 돌려준다. `LocalCommand`는 해석만 하고 표시한다.

### 결과 API

```text
ConfigSet::load(inputs: ConfigInputs) -> Result<ConfigSet, ConfigError>   // 맨 위 설정 파일 읽기
ConfigSet::resolve(&self, host: &str, ctx: &ResolveContext) -> Resolution  // Include는 여기서 읽는다
Resolution { options: Options, host: String, sources: Vec<ValueSource>, notes: Vec<ConfigNote>, refusal: Option<Refusal>, host_key_checking: StrictHostKey, host_key_checking_origin: Origin, identity_files: Vec<KeyFile>, certificate_files: Vec<KeyFile>, known_hosts_files: Vec<PathBuf> }
Resolution::host_key_query(&self) -> HostKeyQuery
KeyFile { written: String, path: Option<String>, provided_by: ProvidedBy, origin: Origin }
HostKeyQuery { name: String, without_port: Option<String> }   // HostKeyQuery::for_host(host, port, alias), HostKeyQuery::for_address(addr, port)
KnownHosts::load(files: &[PathBuf]) -> KnownHosts
KnownHosts::entries(&self, host: &HostKeyQuery) -> Vec<HostKeyEntry>
KnownHosts::check(&self, host: &HostKeyQuery, key_blob: &[u8]) -> HostKeyVerdict
KnownHosts::check_with_address(&self, host: &HostKeyQuery, address: Option<&str>, key_blob: &[u8]) -> HostKeyVerdict   // 포트 없는 주소(평문 키로 판정)가 Mismatch·Revoked면 재조회의 Match를 쓰지 않는다
enum HostKeyVerdict { Match { file, line }, Mismatch { file, line }, Revoked { file, line }, NotFound, DistrustedCaLine { file, line } }
```

- `Resolution`은 `#[non_exhaustive]`다. 크레이트만 만들고 필드가 더해질 수 있으므로, 크레이트 밖에서는 구조체 리터럴로 만들거나 전체 패턴으로 풀지 않고 필드를 읽는다. 같은 이유로 `KeyFile`·`HostKeyEntry`·`LineKey`와 `HostKeyVerdict`도 `#[non_exhaustive]`다(`HostKeyVerdict`는 변형이 더해질 수 있어 `match`에 `_` 갈래가 든다). 호출자가 만드는 `HostKeyQuery`와 `Marker`, 필드가 비공개인 `KnownHosts`는 그대로다.
- `sources`는 적힌 값마다 그 출처를 순서대로 싣는다. 대상(`[user@]host`, `ssh://user@host:port`)이 정한 사용자·포트의 출처는 `destination`이다. `host_key_checking`은 이 크레이트가 하는 확인 모드(확인을 끄는 값을 건너뛴 첫 값)이고, `host_key_checking_origin`이 그 값의 줄이다(없으면 `default`). `StrictHostKeyChecking`의 첫 출처(`source_of`)는 건너뛴 값일 수 있다.
- `ResolveContext`는 명령행에 해당하는 값(위 순서 1~8), 로컬 사용자, 홈, `ssh/` 폴더, 로컬 uid·호스트 이름, `${}`에 쓰는 환경 값, 원격 명령을 갖는다. `final` 두 번째 읽기는 `resolve`가 OpenSSH처럼 안에서 한다(`ssh.c:1246`–`:1265`).
- `Include`는 `resolve`가 읽는다. OpenSSH는 `Include` 인자의 `%` 토큰과 `${}`를 대상마다 펼치고(`readconf.c:2086`의 `expand_match_exec_or_include_path`), 그 줄에 닿은 블록의 적용 여부에 따라 `NEVERMATCH`로 읽으므로 대상 없이 미리 펼칠 수 없다.
- 파일 읽기: `load`는 맨 위 파일을 열고, 같은 핸들의 메타데이터로 권한(모드)을 본 뒤 통과한 파일만 그 자리에서 끝까지 읽어 내용을 저장한다(FIFO·파이프도 읽는다). 모드로 거부된 파일은 읽지 않고, 그 거부는 `resolve`의 `Refusal`로 낸다. 소유자는 로컬 uid를 받는 `resolve`가 `load`가 저장한 같은 핸들의 메타데이터로 판정한다. 그래서 모드는 통과하고 소유자만 다른 사용자 기본 파일은 `load`가 읽고 `resolve`가 `Refusal`로 거부한다(내용은 쓰지 않는다). 그 파일이 FIFO면 `load`는 쓰는 쪽이 닫을 때까지 기다린다. 기준판은 `fstat` 직후 소유자와 모드를 함께 보고 줄을 읽기 전에 거부한다(`readconf.c:2644`–`:2664`). `load`에서 소유자까지 볼지는 [후속 작업과 미정](#후속-작업과-미정)의 "설정 파일 소유자의 판정 시점"이다. `Include` 파일은 `resolve`가 같은 순서(열기 → 판정 → 통과하면 읽기)로 읽는다.
- `Refusal`이 있으면 내장 백엔드는 네트워크에 나가기 전에 `SshBackendError::ConfigRefused`로 끝낸다.
- `identity_files`는 `ssh`가 인증 전에 읽는 `IdentityFile` 목록이다(`load_public_identity_files`, `ssh.c:2354`–`:2497`). 순서는 `-i`, `-o`, 설정 파일 줄 순서, 그다음 `fill_default_options`가 채운 기본 키 파일이다. 항목마다 적힌 값(`written`, `ssh -G`가 보이는 값), `~`·`%` 토큰·`${}`를 펼친 경로(`path`, 홈을 모르면 없음), 준 쪽(`provided_by`, `0`·`1`·`2`), 적힌 자리(`origin`: 파일 줄, 명령행 라벨, 기본 키 파일이면 `default`)를 싣는다. `none`(대소문자 무관)으로 적힌 항목은 빠진다(`options.identity_files`에는 남는다). `certificate_files`가 비면 `ssh`는 항목마다 `<경로>-cert`도 열어 인증서면 다음에 내민다(`:2425`–`:2451`). 이 크레이트는 파일을 열지 않으므로 그 후보는 내장 백엔드가 만든다. `certificate_files`는 같은 펼침이고 `none`을 빼지 않는다. 이 펼침은 `ssh -G`가 끝난 뒤의 연결 단계라(`ssh.c:1612`–`:1615` 다음 `:1646`) 해석의 마지막에 한다. 펼침이 실패하면(값 없는 `${}` 등) `ssh`가 인증 전에 멈추므로 해석을 `Refusal`(그 지시어와 적힌 자리)로 거부한다. 이때도 `ssh -G` 출력(`options`, `dump_client_config`)은 기준판과 같다. 거부된 해석의 `identity_files`·`certificate_files`는 비어 있다(앞 항목만 펼쳐진 목록을 싣지 않는다). `IdentitiesOnly`는 `options.identities_only`와 `source_of("identitiesonly")`다.
- `known_hosts_files`는 `UserKnownHostsFile` 다음 `GlobalKnownHostsFile`이고 `KnownHosts::load`에 그대로 준다. 사용자 파일은 `ssh -G` 단계에서 `~`, 그다음 `%` 토큰·`${}`로 펼친 값(`options.user_hostfiles`, `ssh.c:1535`–`:1546`)이고, 연결 단계에서 두 목록 모두 `~`를 한 번 더 펼친다(`tilde_expand_paths`, `ssh.c:1791`–`:1794`). 그래서 `${KH_PATH}`가 `~/kh`이면 조회 파일은 홈 아래 `kh`다. 이 펼침이 거부되면(다른 사용자의 `~user`) 그 지시어와 적힌 자리를 `Refusal`에 싣는다. 열지 못하는 파일은 건너뛴다(`load_hostkeys`). 파일은 `getline`처럼 줄 단위로 읽고(`hostfile.c:806`), 중간에 읽기 오류가 나면 그 앞의 줄은 남기고 거기서 그 파일을 끝낸다.
- OS마다 갈리는 읽기 오류 처리는 어느 쪽보다도 넓게 통과하지 않는 쪽으로 정했다. 오류가 자른 개행 없는 조각은 glibc `getline`이 돌려주어 `ssh`가 한 줄로 읽고, Darwin `getdelim`은 -1을 돌려주어 `ssh`가 읽지 않는다. 그래서 그 조각은 거절 쪽 근거로만 쓴다: 그 조각의 `@revoked` 줄과 다른 키(`Mismatch`)는 쓰고(glibc와 같거나 더 거절), 그 조각에만 있는 같은 키로는 재조회를 포함해 `Match`를 내지 않는다(Darwin과 같다). `Interrupted`(`EINTR`)도 읽기 오류로 보고 재시도하지 않고 그 자리에서 멈춘다(glibc는 재충전 실패에 오류 플래그를 세우고 더 읽지 않는다). 재조회의 주소 판정은 서버 blob이 인증서여도 평문 키로만 하고, 주소 쪽 폐기 확인에도 CA 키를 쓰지 않는다(아래 `check_with_address`): 이 크레이트는 인증서의 유효기간을 보지 않으므로, 유효한 인증서에서는 기준판보다 더 거절하고 유효기간이 끝난 인증서에서는 기준판과 같다.
- 경로 펼침(`ControlPath`·`IdentityAgent`·`ForwardAgent`·`RevokedHostKeys`·`UserKnownHostsFile`·`GlobalKnownHostsFile`·`IdentityFile`·`CertificateFile`)이 거부되면 `Refusal`의 `keyword`와 `origin`은 그 지시어와 그 값이 적힌 첫 자리(파일 줄, 명령행 라벨, 기본값이면 `default`)다.
- `host_key_query`는 `ssh`가 호스트 키를 찾는 이름이다(`HostKeyAlias`, 아니면 `put_host_port(호스트, 포트)`). 별칭이 없고 포트가 0·22가 아니면 `without_port`에 포트 없는 호스트를 싣는다. `check`는 `name`의 판정이 `HOST_NEW`(`NotFound`·`DistrustedCaLine`)이면 `without_port`로 한 번 더 판정해 `Match`일 때만 그 결과를 쓴다. 기준판 `check_host_key`의 읽기 전용 재조회(`sshconnect.c:1181`–`:1193`)이고 아무것도 기록하지 않는다. 재조회의 `Mismatch`·`Revoked`는 쓰지 않는다(기준판에서 재조회가 실패하면 원래의 `HOST_NEW`로 간다). `CheckHostIP`의 주소 판정은 `HostKeyQuery::for_address`로 따로 하고 재조회하지 않는다. 다만 기준판의 재조회는 `check_host_key` 전체를 포트 0으로 다시 부르는 것이라 주소도 포트 없이 본다(`sshconnect.c:1036`–`:1037`, `:1058`–`:1068`). 그래서 `CheckHostIP`가 적용되면 `check_with_address(호스트 조회, Some(주소), blob)`를 쓴다. 재조회의 `Match`는 포트 없는 주소의 판정이 `NotFound`·`DistrustedCaLine`·`Match`일 때만 쓰고, `Mismatch`·`Revoked`이면 첫 판정(`HOST_NEW`) 그대로다. 주소의 `Revoked`도 실패로 보는 것은 그 폐기 줄이 주소의 다른 키를 가릴 수 있어서다(Darwin `ssh`가 읽지 않는 잘린 `@revoked` 줄, 평문 키에는 없는 CA 키의 `@revoked` 줄). 기준판보다 더 거절하는 경우는 주소가 `HOST_CHANGED`인데 `StrictHostKeyChecking no`라 기준판이 경고만 하고 통과하는 경우(`:1429`–`:1454`)와 주소에 서버 평문 키의 완전한 `@revoked` 줄만 있어 기준판이 통과하는 경우다. 주소 판정은 평문 키로만 한다. 인증서여도 주소 쪽 폐기 확인에 CA 키를 쓰지 않고(평문 키의 폐기 의미, `hostfile.c:315`–`:332`), 호스트 이름 쪽의 CA 키 폐기는 그대로다. 기준판은 인증서로 시작한 호출에서는 주소 줄을 읽지 않는다(`:1058`). 하지만 인증서를 평문 키로 내려 보내면(맞는 CA가 없거나 유효기간 밖, `:1473`–`:1486`), 그 뒤의 재조회는 평문 키로 시작해 주소를 본다. 이 크레이트는 인증서의 유효기간을 보지 않으므로 더 거절하는 쪽을 골랐다. 유효한 인증서에서는 기준판보다 더 거절하고, 유효기간이 끝난 인증서에서는 기준판과 같다. `check`는 주소 없는 `check_with_address`다. `entries`는 그 이름 하나를 가리키는 줄(파일·줄 번호·표식, `ssh-keygen -F`가 보는 키 형식 이름, `ssh`가 읽는 키)을 낸다. 호스트 패턴은 파일의 바이트 그대로 비교한다(`match_hostname`).

### 라이선스

- `Cargo.toml`: `license = "BSD-2-Clause AND BSD-3-Clause AND ISC AND SSH-short"`(director 2881, `BSD-3-Clause`는 아래 `openbsd-compat` 파일 때문에 더함). 크레이트 폴더에 `LICENSE-OpenSSH`(원본 `LICENCE` 전문, 412줄)를 둔다.
- 표기는 옮기는 파일의 실제 조항을 따른다(기준 커밋의 머리 고지):

  | 조항 | 파일 | SPDX |
  |---|---|---|
  | Tatu Ylonen 조항만 | `readconf.c`, `readconf.h`, `sshconnect.c`, `hostfile.h` | `SSH-short` |
  | Tatu Ylonen 조항과 BSD | `ssh.c`, `hostfile.c`, `match.c` | `SSH-short`, `BSD-2-Clause` |
  | ISC | `misc.c`, `addrmatch.c`, `addr.c` | `ISC` |
  | Tatu Ylonen 조항만 | `pathnames.h` | `SSH-short` |
  | Tatu Ylonen 조항과 BSD | `cipher.c`, `log.c` | `SSH-short`, `BSD-2-Clause` |
  | ISC | `digest-openssl.c` | `ISC` |
  | BSD | `kex-names.c`, `myproposal.h`, `sshconnect2.c`, `mac.c`, `sshkey.c`, `sshconnect.h`, `defines.h` | `BSD-2-Clause` |
  | ISC(이식 2단계, known_hosts 키 읽기) | `ssh-ed25519.c`, `ssh-ed25519-sk.c`, `ssh-mldsa-eddsa.c`, `ssh-rsa.c`, `sshbuf-getput-basic.c`, `sshbuf-getput-crypto.c`, `sshbuf-misc.c`, `sshbuf.h` | `ISC` |
  | BSD(이식 2단계, known_hosts 키 읽기) | `ssh-ecdsa.c`, `ssh-ecdsa-sk.c`, `sshkey.h`, `ssh2.h` | `BSD-2-Clause` |
  | 3조항 BSD | `openbsd-compat/fmt_scaled.c`(Ian F. Darwin), `openbsd-compat/vis.c`·`glob.c`(Regents of the University of California) | `BSD-3-Clause` |
  | 공개 도메인 | `openbsd-compat/charclass.h`(Todd C. Miller), `crypto_api.h`(Markus Friedl, 이식 2단계) | 없음(고지만 둔다) |

- `scan_scaled`(`RekeyLimit`, `src/scaled.rs`)와 `vis`(`EscapeChar` 출력, `options.rs`의 `vis_white`)는 원본(`openbsd-compat/fmt_scaled.c:75`–`:229`, `vis.c:61`–`:144`)의 구조·변수·경계 처리를 단계별로 따르므로 파생으로 표시하고 원본 고지를 둔다(director 3000). `glob(3)`(`Include`, `src/glob.rs`)는 Gate 4에서 `openbsd-compat/glob.c`(`glob`·`glob0`~`glob3`·`match`·`g_charclass`)와 `charclass.h`를 그대로 옮겨 다시 썼으므로 같은 기준으로 파생이고 두 파일의 고지를 둔다(glob.c 3조항 BSD, charclass.h 공개 도메인).
- 숫자 주소의 판정과 표기(`misc.rs`의 `numeric_host`·`numeric_text`, `getaddrinfo(AI_NUMERICHOST)`·`getnameinfo(NI_NUMERICHOST)` 대신)는 옮긴 원본이 없다. glibc·bionic·Apple Libinfo·Libc 소스에서 읽은 규칙(위 [옮기는 원본](#옮기는-원본)의 숫자 주소)을 이 크레이트가 새로 쓴 코드다. 원본의 구조와 변수를 따르지 않으므로(조각으로 나눠 읽는다) director 3000 기준의 파생이 아니다.

- Tatu Ylonen 조항은 파생물을 그렇게 표시하라고 요구한다("Any derived versions of this software must be clearly marked as such"). 옮긴 파일마다 원본 파일의 저작권 고지를 그대로 두고 `derived from OpenSSH, modified`를 적는 것으로 이 요구를 지킨다. 원본 파일과 함수를 모듈 문서에 적는다.
- 나머지 크레이트는 MIT 그대로다. `deny.toml`의 허용 목록에 `BSD-2-Clause`와 `ISC`는 이미 있고, `SSH-short`는 이식 크레이트 작업이 더한다. cargo-deny가 `SSH-short`를 받지 못하면(라이선스 식을 해석하지 못하거나 허용 목록에 넣지 못하면) 그 작업은 다른 표기나 예외 설정으로 돌아가지 않고 보고한다.
- Tasty·Android 앱의 라이선스 고지에 OpenSSH 고지를 넣는 일은 앱 쪽 전환 작업이다.

## 내장 백엔드가 읽는 파일

OpenSSH와 같은 형식을 같은 뜻으로 읽는다(이식 크레이트). 위치 기본값도 OpenSSH와 같다.

| 파일 | 기본 위치 | 형식 |
|---|---|---|
| 사용자 설정 | `<ssh 폴더>/config` | `ssh_config` |
| 시스템 설정 | Unix `/etc/ssh/ssh_config`, Windows `%PROGRAMDATA%\ssh\ssh_config` | 같다. 사용자 설정 다음에 읽는다 |
| 사용자 known_hosts | `<ssh 폴더>/known_hosts`, `known_hosts2` | 평문·와일드카드·부정·`[호스트]:포트`·해시(`|1|`)·`@revoked`. `@cert-authority` 줄은 믿지 않는다([호스트 키 판정](#호스트-키-판정)) |
| 시스템 known_hosts | Unix `/etc/ssh/ssh_known_hosts`, `ssh_known_hosts2`, Windows `%PROGRAMDATA%\ssh\` 아래 | 같다 |
| 기본 키 파일 | `<ssh 폴더>/id_rsa`, `id_ecdsa`, `id_ecdsa_sk`, `id_ed25519`, `id_ed25519_sk`, `id_mldsa44_ed25519`(기준판 `readconf.c:2931`–`:2940`) | OpenSSH 개인키 형식, PEM(PKCS#1·SEC1·PKCS#8, 암호 걸린 PKCS#8과 PKCS#5 RSA). 읽기는 russh가 한다. PuTTY `ppk`처럼 OpenSSH가 읽지 못하는 형식은 읽지 않는다: russh `decode_secret_key`가 ppk를 기본으로 받으므로(`keys/format/mod.rs:45`) 그 함수를 부르기 전에 `PuTTY-User-Key-File-` 머리를 보고 건너뛰고 `ConfigNote`(`unsupported_key_format`)로 남긴다 |
| ssh-agent | `IdentityAgent`, 없으면 `SSH_AUTH_SOCK`. Windows는 `\\.\pipe\openssh-ssh-agent`와 Pageant | russh `AgentClient`. 소켓이 없으면 agent 인증을 건너뛰고 `ConfigNote`로 남긴다 |

- `<ssh 폴더>`는 설정값 `embedded.ssh_dir`이다. 기본은 `~/.ssh`(`~`는 `embedded.home`)다. 모바일은 바인딩 층이 앱 저장소의 `ssh/` 폴더를 `configured`로 넣는다(tasty 설정으로 덮어쓸 수 있다).
- `~` 펼침: 설정 파일 안의 `~/.ssh/` 로 시작하는 경로는 `<ssh 폴더>/`로, 그 밖의 `~/`는 `embedded.home`으로 펼친다. `embedded.ssh_dir`이 기본값이면 OpenSSH와 같다. 홈을 알 수 없고 설정도 없으면 `~`가 든 경로는 없는 파일로 다룬다: `Include`는 맞는 파일이 없는 것(GLOB_NOMATCH, 그 줄은 넘어간다), known_hosts 목록에서는 빠지고, `-i`는 건너뛴다. 소켓 경로도 없는 소켓이다: `ControlPath`와 `ForwardAgent`의 경로는 빠지고(둘 다 미지원이라 쓰지 않는다), `IdentityAgent`는 `none`이 되어 agent 인증을 건너뛴다(위 ssh-agent 행). 읽지 못하면 `ssh`가 멈추는 `RevokedHostKeys`(`sshconnect.c:1544`–`:1563`)만 판정 불가로 거부한다. 다른 사용자의 홈(`~user`)은 사용자 정보를 읽지 않으므로 `Include` 안에서도 판정 불가로 거부한다. 다만 펼친 `Include` 인자가 `PATH_MAX`에 닿으면 기준판 `glob()`이 홈을 찾기 전에 맞는 파일이 없다고 하므로(`glob.c:200`) 그 인자는 홈을 보지 않고 건너뛴다(`readconf.c:2116`–`:2120`). 어느 경우에도 작업 폴더의 `~` 폴더를 가리키지 않는다.
- 경로 구분자: 이식 크레이트의 절대 경로 판정(`path_absolute`, `misc.c:2668`–`:2672`)과 `Include`의 glob은 기준판(portable)대로 `/`만 본다. Windows에서도 같아서 `Include C:\...`·`C:/...`는 상대 경로가 되어 사용자 설정은 `~/.ssh/` 아래, 시스템 설정은 `/etc/ssh/` 아래로 바뀐다. Win32-OpenSSH 규칙을 따를지는 [후속 작업과 미정](#후속-작업과-미정)의 "Windows의 설정 경로 규칙"이다.

## 지시어 분류

OpenSSH 10.6p2의 `keywords[]`(`readconf.c:178`–`:325`)에는 132개 항목이 있고 이름은 128개다. 네 이름(`gssapiauthentication`·`gssapidelegatecredentials`·`pkcs11provider`·`smartcarddevice`)이 `#ifdef GSSAPI`·`#ifdef ENABLE_PKCS11`의 두 갈래로 두 번 나온다(한 번은 그 기능을 켠 빌드의 opcode, 한 번은 `oUnsupported`). 이 분류는 두 기능을 켠 갈래(실제 opcode)를 기준으로 한다. 배포판 OpenSSH가 보통 두 기능을 켜고 빌드되기 때문이다. 이식 크레이트는 128개 이름을 모두 알고 이름마다 아래 처리 하나를 갖는다. 별칭은 원래 지시어와 같은 처리다.

지시어 이름은 대소문자를 가리지 않는다(`readconf.c`가 소문자로 바꿔 찾는다). 값의 대소문자는 OpenSSH가 그 지시어에서 하는 그대로다(예: `yes`·`no` 같은 다중 값은 대소문자 무시, 경로·명령은 구별, `Host` 패턴은 대소문자를 구별해 비교: `match_pattern(host, arg)`(`readconf.c:1854`)이 첫 읽기에서는 명령행에 적힌 그대로의 호스트를, `final` 두 번째 읽기에서는 소문자로 바꾼 호스트(`ssh.c:1206`–`:1207`)를 패턴과 그대로 맞춘다. `Match host`·`originalhost`는 `match_hostname`(`match.c:232`–`:242`)이 호스트와 패턴을 소문자로 바꿔 비교한다).

| 처리 | 뜻 | `ConfigNote.action` |
|---|---|---|
| 지원 | OpenSSH와 같은 뜻으로 적용한다 | 없음 |
| 보안 관련 미구현 | 적용되면 그 연결을 네트워크에 나가기 전에 거부한다 | `refused` |
| 읽었지만 미지원 | 값을 읽고 적용하지 않는다. 연결은 계속한다 | `shown` |
| OpenSSH가 버린 지시어 | OpenSSH도 적용하지 않는다(`oDeprecated`·`oUnsupported`·`oIgnore`, `readconf.c:2579`–`:2589`) | `shown`(`oIgnore`는 `ignored`) |
| 모르는 지시어 | 파일 전체를 거부한다(아래) | `refused` / `ignored` |

거부와 표시를 가르는 기준(director 2881이 추가 결정 4의 "보안 관련"을 다듬었다): 묻는 것은 "그 값을 구현하지 않고 무시하면 무엇이 되는가"다.

- 거부: 무시하면 보안이 약해지는 값. 호스트 키 확인과 그 근거(known_hosts, 폐기 목록, DNS 지문), 알고리즘 허용 범위, 인증서 신뢰, 사용자가 요구한 인증 방식의 강도다. 또 무시하면 다른 호스트나 다른 경로로 연결되는 값(대상 이름 바꾸기, 프록시, 나가는 인터페이스, 블록 적용을 판정할 수 없는 `Match`)도 거부한다. 이 둘째 종류는 director 2881의 예(호스트 키·known_hosts·알고리즘·인증서) 밖으로 넓힌 것이다. 근거: 연결 상대나 경로가 바뀌면 사용자가 정한 대상을 확인한다는 호스트 키 확인이 뜻을 잃는다(바뀐 상대의 키를 그 상대로 확인할 뿐이다). 사용자가 정하지 않은 상대에게 인증 정보를 내밀게 되는 것도 같다.
- 표시하고 계속: 무시하면 그 기능을 쓰지 않을 뿐인 값. 제공하지 않는 인증 방법이나 키 소스를 켜는 값(GSSAPI, hostbased, PKCS#11, 보안 키 제공자)은 그 방법을 시도하지 않고 다음 인증 방법으로 넘어간다. OpenSSH도 그 방법이 실패하면 다음으로 넘어가므로 결과는 그 방법이 실패한 OpenSSH와 같다. 상대에게 기능을 넘기는 값(agent·X11·포워딩·자격 위임), 연결 뒤 로컬 명령, 표시·로그 값도 같다.
- 근거 사례: Fedora·RHEL 계열은 시스템 설정 `/etc/ssh/ssh_config.d/50-redhat.conf`에서 `GSSAPIAuthentication yes`를 켠다. 이것을 거부하면 그런 호스트에서 내장 백엔드의 모든 연결이 막힌다.

### 모르는 지시어

- OpenSSH는 모르는 이름을 만나면 그 줄이 어느 블록에 있든(적용되지 않는 `Host` 블록 안이어도) 오류로 센다. 이름 찾기(`parse_token`, `readconf.c:959`–`:973`)가 블록 적용 판정보다 먼저이기 때문이다. 그 파일을 다 읽은 뒤 `terminating, N bad configuration options`로 끝낸다(`readconf.c:2672`–`:2681`). 적용되지 않는 블록 안의 `Include`도 OpenSSH는 "parse only"로 읽으므로(`readconf.c:2125`–`:2140`, `SSHCONF_NEVERMATCH`) 그 파일의 모르는 이름도 같다. 내장 백엔드도 같다: 모르는 이름이 하나라도 있으면 그 파일을 읽는 모든 연결을 거부하고, 파일 단위 `ConfigNote`(`unknown_keyword`, 파일·줄)를 남긴다.
- 예외는 `IgnoreUnknown`뿐이다. OpenSSH는 그 줄을 읽는 때에 이미 정해진 `IgnoreUnknown` 값의 패턴과 맞는 모르는 이름만 무시한다(`oIgnoredUnknownOption`). `IgnoreUnknown` 자체는 첫 값 우선이고 적용되는 블록에서만 정해진다(`readconf.c:2211`–`:2213`, `parse_string`). 그래서 그 줄보다 뒤에 있거나 적용되지 않는 블록에 있는 `IgnoreUnknown`은 무시를 만들지 않는다. 내장 백엔드도 같은 순서로 판정한다.
- `IgnoreUnknown`은 아는 이름에는 효과가 없다. 그래서 보안 관련 미구현 지시어의 거부를 무를 수 없다.

### 아는 지시어의 판정 때

- 아는 지시어의 판정(지원·거부·표시)은 적용될 때만 한다. 적용되지 않는 `Host`·`Match` 블록의 지시어는 거부하지 않는다.
- 판정은 [옵션 적용 순서](#옵션-적용-순서)로 유효 값을 정한 뒤에 한다(예: `ProxyJump`에 밀려 쓰이지 않는 `ProxyCommand`는 거부하지 않는다).
- 표시는 설정 파일·대상 옵션·`ssh.overrides`에 그 지시어가 적혀 적용된 경우만 남긴다. OpenSSH 기본값을 쓴 지시어에는 노트를 남기지 않는다(예: 적지 않은 `PubkeyAuthentication`).

### 지원

| 지시어(별칭) | 비고 |
|---|---|
| `Host`, `Match`(`all`·`canonical`·`final`·`host`·`originalhost`·`user`·`localuser`·`tagged`·`version`·`sessiontype`·`command`, 부정 `!`), `Include`, `Tag`, `IgnoreUnknown` | `final`은 OpenSSH처럼 설정을 한 번 더 읽는다(`ssh.c:1252`–`:1265`). `sessiontype`·`command`는 [용도마다 해석](#용도마다-해석하고-같을-때만-세션을-나눈다)의 표대로 판정한다. `version`은 `SSH_RELEASE` 값 `OpenSSH_10.6p2`와 패턴을 비교한다(`readconf.c:837`–`:841`, `version.h:3`–`:6`). `Include` 중첩 상한은 고정값([설정값](#설정값)) |
| `Hostname`, `User`, `Port`, `HostKeyAlias` | |
| `IdentityFile`(`IdentityFile2`), `IdentitiesOnly`, `CertificateFile`, `IdentityAgent` | 개인키 파일 권한(결정됨, conductor M4 Gate 4): 데스크톱 Unix(Android·iOS 제외)에서 OpenSSH `sshkey_perm_ok`(`authfile.c`)처럼 사용자가 소유한 파일의 그룹·기타 비트(`mode & 077`)가 있으면 그 키를 쓰지 않고 `ConfigNote`(`bad_key_permissions`)로 남긴다. 다른 사용자 소유 파일은 OpenSSH처럼 보지 않는다. 로컬 uid를 모르는 OS(macOS)에서는 소유자와 관계없이 본다(더 거절할 뿐 덜 거절하지 않는다). Windows·모바일은 판정하지 않는다. 판정과 읽기는 한 번 연 파일 핸들에서 한다(`authfile.c`가 연 fd에 `fstat`하고 그 fd로 읽는 것과 같다). 그 핸들의 메타데이터를 읽지 못하면 그 키를 쓰지 않고 같은 노트를 남긴다 |
| `PreferredAuthentications`, `PasswordAuthentication`, `KbdInteractiveAuthentication`(`ChallengeResponseAuthentication`·`SkeyAuthentication`·`TisAuthentication`), `KbdInteractiveDevices`, `NumberOfPasswordPrompts`, `BatchMode` | `BatchMode yes`면 `SshPrompt`를 부르지 않는다 |
| `PubkeyAuthentication`(`DSAAuthentication`) | `no`·`unbound`는 같은 뜻으로 지원한다. `yes`(기본)는 OpenSSH에서 서버가 주면 host-bound 인증을 쓰지만 russh 0.65.4에는 host-bound 인증이 없다(`publickey-hostbound` 검색 0건, 대조: 같은 소스에서 `ext-info-c` 1건). 그래서 `yes`는 unbound로 인증한다. 적어 둔 `yes`에만 `shown`을 남긴다. `host-bound`는 거부 |
| `StrictHostKeyChecking` | `yes`, `ask`(`SshPrompt`가 없거나 `BatchMode yes`면 `yes`), `accept-new`. 설정 파일·대상 옵션의 `no`·`off`는 적용하지 않고 표시한다(아래 표). `ssh.overrides`로는 받지 않는다([옵션 적용 순서](#옵션-적용-순서)) |
| `UserKnownHostsFile`, `GlobalKnownHostsFile`, `CheckHostIP`, `HashKnownHosts` | `accept-new`가 첫 `UserKnownHostsFile`의 폴더를 만들 때 Unix에서는 `0700`으로 만든다(OpenSSH `hostfile_create_user_ssh_dir`, 결정됨, conductor M4 Gate 4) |
| `RevokedHostKeys` | 공개키 목록 파일. 파일이 KRL 바이너리면 거부. 파일이 없거나 읽지 못하면 OpenSSH처럼 호스트 인증을 실패시킨다(`sshconnect.c:1544`–`:1563`) |
| `RequiredRSASize` | 기본 1024(`readconf.c:3032`–`:3033`, `SSH_RSA_MINIMUM_MODULUS_SIZE`). 그보다 작은 RSA 호스트 키는 연결을 끊고, 작은 RSA 사용자 키는 건너뛴다(`sshconnect2.c:96`·`:1593`·`:1847`) |
| `HostKeyAlgorithms`, `KexAlgorithms`, `Ciphers`, `MACs`, `PubkeyAcceptedAlgorithms`(`PubkeyAcceptedKeyTypes`), `CASignatureAlgorithms` | [알고리즘](#알고리즘) |
| `RekeyLimit` | russh `Limits`의 범위로 자른다([설정값](#설정값)) |
| `ConnectTimeout`, `ConnectionAttempts`, `ServerAliveInterval`, `ServerAliveCountMax`, `TCPKeepAlive`(`KeepAlive`), `AddressFamily`, `BindAddress` | |
| `ProxyJump` | 여러 단계와 `none`. 앞 단계 세션의 `direct-tcpip` 채널 위에 다음 세션을 연다. 단계마다 호스트 키를 확인한다. 홉의 옵션은 [옵션 적용 순서](#옵션-적용-순서) |
| `ProxyCommand none` | 프로세스를 시작하지 않는다. `none`이 아닌 값은 거부 |
| `SendEnv`, `SetEnv` | exec·포트 발견 채널의 `env` 요청 |
| `RemoteCommand` | 명령이 있는 용도(포트 발견·exec·SFTP)에 `RemoteCommand`(`none` 아님)도 적용되면 OpenSSH처럼 연결 전에 실패한다(`ssh.c:1363`–`:1364`, `Cannot execute command-line and remote command`). SFTP도 같다: `ssh -s <대상> sftp`는 `sftp`를 명령 버퍼에 넣는다(`ssh.c:1150`–`:1156`). 명령이 없는 터널(`-N`)에서는 OpenSSH도 실패하지 않고 실행하지도 않는다 |
| `CanonicalizeHostname no`, `CanonicalDomains`, `CanonicalizeFallbackLocal`, `CanonicalizeMaxDots`, `CanonicalizePermittedCNAMEs` | 뒤의 넷은 `CanonicalizeHostname`이 켜졌을 때만 이름을 바꾼다(켜는 값은 거부). `CanonicalizePermittedCNAMEs`가 있으면 OpenSSH는 프록시 없는 연결에서 이름을 먼저 풀지만 `CanonicalizeHostname no`면 CNAME을 따라가지 않는다(`ssh.c:1229`–`:1240`이 부르는 `check_follow_cname`의 `:383`). 풀지 못하면 OpenSSH는 255로 끝나고, 내장 백엔드는 같은 이름으로 연결하다 같은 이름 풀기 실패로 끝난다 |
| `NoHostAuthenticationForLocalhost no` | 설정 파일·대상 옵션의 `yes`는 적용하지 않고 표시한다. `ssh.overrides`로는 받지 않는다 |
| `VerifyHostKeyDNS no` | `yes`·`ask`는 거부 |
| `SessionType` | 두 백엔드 모두 용도가 첫 값으로 정하므로 설정 파일의 값은 이기지 못한다([용도마다 해석](#용도마다-해석하고-같을-때만-세션을-나눈다)). 설정에 적힌 값과 용도의 값이 다르면 `shown`을 남긴다 |
| `ChannelTimeout` | 1단계는 표시만, 2단계 구현(아래 끝 문장). 2단계에서 이 레포가 여는 채널(`session`: exec·포트 발견·SFTP, `direct-tcpip`: 터널)에 적용되는 양수 제한과 `global`은 지원한다. 종류 패턴의 비교와 `global`의 뜻은 기준판 규칙(`readconf.c`의 값 해석, `ssh.c:1693`–`:1706`의 `channel_add_timeout`)을 그대로 따르고, 세부는 [후속 작업](#후속-작업과-미정)이 정한다. `0`을 포함한 값 목록은 기준판의 첫 일치 규칙(`channels.c:382`–`:394` `lookup_timeout`)대로 해석한다(예: `session=0 session=5`면 세션에 제한이 없다). 이 레포가 여는 채널에 적용되지 않는 값은 표시만 한다. 거부가 아니라 지원인 이유: 무시하면 사용자가 닫도록 정한 채널이 열린 채 남아 제한이 약해지는데, 채널은 이 레포가 소유하므로 타이머로 같은 동작을 낼 수 있다. 터널 위 링크는 heartbeat가 오가므로 보통 걸리지 않는다. 시스템 백엔드는 OpenSSH가 한다. 1단계(내장 백엔드 첫 커밋 묶음)에는 타이머가 없어 적용된 값을 `ConfigNote`(`not_supported`, 지시어의 파일·줄)로 표시만 하고, 타이머는 2단계에서 exec·SFTP와 함께 구현한다([후속 작업](#후속-작업과-미정)) |
| `RefuseConnection` | OpenSSH처럼 그 메시지로 연결을 끝낸다 |

### 보안 관련 미구현 (적용되면 거부)

| 지시어·값 | 이유 |
|---|---|
| `VerifyHostKeyDNS yes`·`ask` | 호스트 키를 믿는 근거(DNS SSHFP)다. 무시하면 DNS 지문과 맞지 않는 키를 `accept-new`가 받을 수 있다 |
| `KnownHostsCommand` | 다른 프로그램을 실행해야 하고, 빠지면 `accept-new`가 다른 키를 받을 수 있다 |
| `RevokedHostKeys`가 KRL 파일 | 폐기 목록을 읽지 못하면 폐기된 키를 받을 수 있다 |
| 유효 값이 `ProxyCommand`(`none` 아님) | 다른 프로그램 실행이다. 유효 값만 판정한다. `ProxyJump`가 먼저 적용되면 `parse_jump`가 `ProxyCommand none`을 넣으므로(`readconf.c:3534`) 뒤에 온 `ProxyCommand`는 첫 값 우선으로 버려지고 거부하지 않는다. `ProxyCommand`가 먼저면 `ProxyJump`는 OpenSSH에서도 쓰이지 않는다(`parse_jump`의 `active &= proxy_command == NULL && jump_host == NULL`, `readconf.c:3493`). 두 값이 함께 남는 경우(`ssh.c:1300`–`:1303`의 `inconsistent options: ProxyCommand+ProxyJump`)는 설정 오류다 |
| `Match exec` | 다른 프로그램을 실행해야 블록 적용을 판정한다. 판정하지 못한 블록의 `Hostname`·`ProxyJump`가 빠지면 고르는 대상이 달라진다 |
| `Match localnetwork` | 블록 적용을 판정하지 못한다(같은 이유) |
| `CanonicalizeHostname yes`·`always` | 무시하면 OpenSSH와 다른 호스트로 연결한다 |
| `BindInterface` | 무시하면 사용자가 정한 인터페이스(예: VPN)가 아닌 경로로 나간다 |
| `PubkeyAuthentication host-bound` | 사용자가 요구한 인증 방식의 강도다. 무시하면 host-bound가 막는 서명 재사용을 막지 못한다 |
| 알고리즘 지시어의 교집합이 빔 | [알고리즘](#알고리즘) |
| 모르는 지시어 | 위 "모르는 지시어" |

### 읽었지만 미지원 (표시하고 계속)

| 지시어 | 내장 백엔드의 동작 |
|---|---|
| `LocalForward`, `RemoteForward`, `DynamicForward`, `ExitOnForwardFailure`, `GatewayPorts`, `ClearAllForwardings`, `PermitRemoteOpen` | 이 레포는 자기 터널만 연다. 시스템 백엔드는 OpenSSH대로 설정 파일의 `LocalForward` 등을 함께 연다(터널 고정 옵션 `ClearAllForwardings=no`가 첫 값으로 이긴다). 두 백엔드가 다른 점이다 |
| `ControlMaster`, `ControlPath`, `ControlPersist` | 세션을 프로세스 밖과 공유하지 않는다 |
| `ForwardAgent`, `AddKeysToAgent` | agent를 넘기지 않고 키를 넣지 않는다 |
| `ForwardX11`, `ForwardX11Trusted`, `ForwardX11Timeout`, `XAuthLocation`, `Tunnel`, `TunnelDevice`, `StreamLocalBindMask`, `StreamLocalBindUnlink` | 해당 기능이 없다 |
| `RequestTTY`, `StdinNull`, `ForkAfterAuthentication`, `EscapeChar`, `EnableEscapeCommandline`, `ObscureKeystrokeTiming` | 대화형 세션이 없다 |
| `PermitLocalCommand`, `LocalCommand` | 연결 뒤 로컬 명령을 실행하지 않는다. 무시해도 보안이 약해지지 않는다 |
| `GSSAPIAuthentication`, `GSSAPIDelegateCredentials` | GSSAPI 인증을 시도하지 않고 다음 인증 방법으로 넘어간다. russh에 GSSAPI 구현이 없다. `yes`일 때만 표시한다 |
| `HostbasedAuthentication` | hostbased 인증을 시도하지 않는다(`ssh-keysign` 프로그램이 필요하다). `yes`일 때만 표시한다 |
| `PKCS11Provider`(`SmartcardDevice`), `SecurityKeyProvider` | 그 키 소스의 키를 내밀지 않고 남은 키와 방법으로 인증한다. `none`이 아닌 값일 때만 표시한다. 보안 키(`*-sk`) 개인키 파일은 제공자가 없으므로 건너뛰고 `ConfigNote`(`unsupported_key_format`)로 남긴다 |
| `ProxyUseFdpass` | `ProxyCommand`가 거부되므로 쓰일 곳이 없다 |
| `Compression yes` | 압축 없이 연결한다(russh `flate2` feature를 켜지 않는다) |
| `HostbasedAcceptedAlgorithms`(`HostbasedKeyTypes`), `EnableSSHKeysign` | hostbased 인증을 하지 않으므로 쓰일 곳이 없다 |
| `StrictHostKeyChecking no`·`off`, `NoHostAuthenticationForLocalhost yes`(설정 파일·대상 옵션) | 호스트 키 확인을 건너뛰는 동작은 두지 않는다(사용자 결정). 무시하면 확인이 계속되므로 보안이 약해지지 않는다. 값이 적히지 않은 것처럼 다음 값으로 확인하고, 노트에 "호스트 키 확인을 끄는 값은 적용하지 않고 확인을 계속한다"를 남긴다 |
| `UpdateHostKeys` | known_hosts에 새 키를 더하지 않는다. 이 연결의 판정은 바뀌지 않는다 |
| `LogLevel`, `LogVerbose`, `SyslogFacility`, `VisualHostKey`, `FingerprintHash`, `WarnWeakCrypto`, `VersionAddendum`, `IPQoS` | 출력·표시·패킷 표시 값이다 |

### OpenSSH가 버린 지시어

- `oDeprecated`: `Cipher`, `FallBackToRsh`, `GlobalKnownHostsFile2`, `RhostsAuthentication`, `UserKnownHostsFile2`, `UseRoaming`, `UseRsh`, `UsePrivilegedPort`.
- `oUnsupported`: `AFSTokenPassing`, `KerberosAuthentication`, `KerberosTGTPassing`, `RSAAuthentication`, `RhostsRSAAuthentication`, `CompressionLevel`. GSSAPI·PKCS#11 없이 빌드한 OpenSSH에서 `oUnsupported`가 되는 네 이름은 위 분류의 기준 갈래에 따라 다룬다.
- `oIgnore`: `Protocol`.

## 호스트 키 판정

- 판정은 이식 크레이트의 `KnownHosts::check`가 한다. russh의 known_hosts 도우미(`check_known_hosts_path`)는 쓰지 않는다. 그 도우미는 `@revoked`를 무시하고 줄을 공백 하나로만 나눈다(근거 보고서).
- 순서는 OpenSSH `check_host_key`(`sshconnect.c:984`)와 `check_hostkeys_by_key_or_type`(`hostfile.c:349`–`:405`)와 같다.
  1. 그 호스트의 줄 가운데 표식 없는 줄을 차례로 본다. 키가 같으면 `HOST_OK`로 멈춘다.
  2. 같지 않은 키를 만나면 그 키의 형식과 관계없이 `HOST_CHANGED`로 기록하고 계속 본다(`hostfile.c:386`–`:397`). 그래서 known_hosts에 ed25519 키만 있는 호스트가 ecdsa 키를 내면 모르는 호스트가 아니라 바뀐 키다.
  3. 줄을 다 본 뒤 `@revoked` 목록에 그 키가 있으면 `HOST_REVOKED`다(`:398`–`:402`).
  4. 표식 없는 줄이 하나도 없을 때만 `HOST_NEW`(모르는 호스트)다.
  5. 4이고 `HostKeyAlias`가 없고 포트가 0·22가 아니면, 포트 없는 호스트 이름으로 1~4를 읽기 전용으로 다시 본다. 거기서 `HOST_OK`이고 `CheckHostIP`의 포트 없는 주소(평문 키로 판정)가 `HOST_CHANGED`도 폐기도 아니면 통과이고, 아니면 4 그대로다(`sshconnect.c:1181`–`:1193`, `KnownHosts::check_with_address`).
- 결과마다: `HOST_OK`는 통과, `HOST_CHANGED`·`HOST_REVOKED`는 거절(`accept-new`·`ask`에서도 기록하거나 묻지 않는다), `HOST_NEW`는 `StrictHostKeyChecking`에 따라 묻거나(`ask`) 기록하거나(`accept-new`) 거절한다(`yes`). `CheckHostIP yes`면 주소로도 같은 판정을 한다.
- 알고리즘 순서(`order_hostkeyalgs`)가 known_hosts에 있는 형식을 앞으로 당기므로 보통 서버는 아는 형식의 키를 낸다. 2는 서버가 그 형식을 내지 않게 바뀐 경우다.
- `@cert-authority` 줄은 1단계에서 믿지 않는다. 그 줄은 판정에서 통과 근거가 되지 않고, `DistrustedCaLine`으로 `ConfigNote`(`cert_authority_distrusted`)에 남는다. `DistrustedCaLine`은 4(`HOST_NEW`)이면서 그 호스트를 가리키는 `@cert-authority` 줄이 있을 때이고 자리는 그 첫 줄이다. 호스트 인증서 알고리즘(`*-cert-v01@openssh.com`)은 제안 목록에서 빼므로 서버는 평문 호스트 키를 낸다. 그래도 인증서가 오면 폐기 확인(인증서의 공개키) 뒤 평문 키로 1~4를 다시 본다. `ssh`가 맞는 CA가 없을 때 하는 재시도와 같다(`sshconnect.c:1474`–`:1486`). 폐기 확인은 기준판 `check_key_not_revoked`(`hostfile.c:315`–`:332`)처럼 인증서의 공개키와 CA 키(서명 키 갈래 `:325`–`:328`) 둘 다로 하고, 일치 판정보다 앞선다. 어느 쪽이든 걸린 첫 `@revoked` 줄이 `Revoked`의 자리다. CA 키는 인증서 본문에서 읽은 것이다. `@cert-authority` 줄은 위 1~4의 "표식 없는 줄"이 아니다.
- `RevokedHostKeys`와 `RequiredRSASize`는 위 [지원](#지원) 표대로 판정에 들어간다.
- `accept-new`의 기록은 OpenSSH `add_host_to_hostfile`(`hostfile.c:542`)과 같은 형식이고 `HashKnownHosts`를 따른다. 기록할 파일은 `UserKnownHostsFile`의 첫 파일이다.
- 기록 직전 판정: 기록은 프로세스 안 잠금 아래에서 known_hosts 파일을 다시 읽고, 연결 전과 같은 판정 함수(`Names::decide`)로 그 파일을 다시 판정한 뒤 그 판정을 따른다(결정됨, conductor M4 Gate 4 6회차: 재판정만의 별도 경로를 두지 않는다). 판정 순서는 연결 전과 같다. `CheckHostIP` 주소(포트가 붙은 이름)의 바뀐 키·폐기 줄을 먼저 보고 묻지 않고 `HOST_CHANGED`·`HOST_REVOKED`로 거절한다. 그다음 호스트 이름을 본다(포트 없는 이름 재조회, `@revoked`, `@cert-authority` 포함). 호스트가 맞으면 새 주소만 묻지 않고 기록한다(기준판도 통과한 호스트의 새 주소는 묻지 않고 더한다, `sshconnect.c:1153`–`:1167`). 바뀐 키·폐기는 거절이다. 나머지는 새 호스트로 보고 확인 모드에 맡기며, 받아들이면 호스트와 새 주소를 기록한다. 최신 판정이 연결 전 판정과 다르면 최신 판정을 따른다. 거절이면 거절하고, 새 호스트인데 확인 모드를 아직 거치지 않았으면 지금의 `StrictHostKeyChecking`을 적용한다(`yes`는 거절, `ask`는 질문, `accept-new`는 기록). 답한 뒤 잠금 아래에서 다시 판정한다. 기록 대상은 잠금 아래 판정이 고른 이름이다. `@cert-authority` 표시(`cert_authority_distrusted`) 같은 판정의 노트도 결과를 정한 판정(잠금 아래 판정이 있었으면 그 판정)의 것만 남긴다. 그래서 질문에 답한 뒤 파일이 바뀌어도 그 파일로 새로 시작한 연결과 노트가 같다(시험 `a_cert_authority_line_written_during_the_question_is_noted`, `a_key_saved_during_the_question_leaves_the_notes_of_a_known_host`). 그 사이 다른 연결이 같은 키를 적었으면 다시 쓰지 않는다. 그래서 연결 전에 포트 없는 이름으로 통과한 호스트도 잠금 아래에서는 최신 파일로 같은 재조회를 한다. 포트 없는 호스트 줄이 지워졌거나 포트 없는 주소의 키가 바뀌었거나 폐기됐으면, 새 연결과 같이 새 호스트가 된다. 포트가 붙은 주소에는 그 폐기 줄이 걸리지 않는다(시험 `a_known_name_removed_before_the_record_is_decided_by_the_mode`, `an_address_changed_with_the_name_removed_is_refused_before_the_mode`, `a_name_passed_without_its_port_is_judged_again_as_at_the_start`).

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
| `embedded.channel_open_timeout` | 10 s | 100 ms ~ 10 min | 채널 열기 응답 기한. `ssh.connect_timeout` 기본과 같은 값. 포트 발견이 기다림을 그만둔 열기는 같은 시간만큼 더 기다려, 늦게 열리면 닫고 그래도 답이 없으면 그 열기 작업을 멈춘다(세션이 끝나면 그 전에 함께 끝난다). 그래서 터널이 붙든 세션에서 답 없는 열기 작업이 쌓이지 않는다(시험 `an_open_the_server_never_answers_leaves_no_task_on_the_session`). 남는 것: 답 없는 열기는 russh 안에서 확정되지 않은 채널로 세션이 끝날 때까지 남고, 그 뒤에 확인이 오면 받는 쪽이 없어 그 채널은 세션이 끝날 때까지 열려 있다. russh 0.65.4에는 보낸 열기를 거두는 API가 없다(`client/encrypted.rs:525`–`:536`) |
| `embedded.prompt_timeout` | 5 min | 1 s ~ 1 h | `SshPrompt`의 답을 기다리는 시간. 사람이 passphrase를 치는 시간이다. 이 대기 동안 `ssh.connect_timeout`은 멈춘다. 기한이 겹쳐 있으면(포트 발견 기한 안의 핸드셰이크·인증 기한) 각 기한이 질문 시간 전체를 돌려받는다 |
| `embedded.runtime.worker_threads` | 1 | 1 ~ 16 | 몇 개의 SSH 세션에 워커 하나로 충분하다. 모바일 스레드 비용을 줄인다 |
| `embedded.window_size` | 2 MiB | 32 KiB ~ 64 MiB | russh `Config::window_size` 기본(2097152, `client/mod.rs:2790`) |
| `embedded.max_packet_size` | 32 KiB | 1 KiB ~ 65535 bytes | russh 기본(32768). russh는 65535보다 큰 값을 받으면 경고만 찍고 계속하므로(`client/mod.rs:1232`) 상한을 65535로 둔다(64 KiB = 65536은 그 경고에 걸린다) |
| `embedded.channel_buffer` | 100 | 1 ~ 10 000 | russh `channel_buffer_size` 기본 |
| `embedded.rekey.bytes` | 1 GiB | 1 MiB ~ 1 GiB | russh `Limits` 기본. `Limits::new`가 읽기·쓰기 한도 각각 1 GiB(`1 << 30`) 이하를 단언한다(`lib_inner.rs:274`). `RekeyLimit` 지시어가 이기고, 그 값이 1 GiB를 넘으면 1 GiB로 자르고 `ConfigNote`(`clamped`)로 남긴다 |
| `embedded.rekey.time` | 1 h | 1 min ~ 24 h | russh `Limits::rekey_time_limit` 기본 |
| `embedded.gex.min_bits`, `embedded.gex.preferred_bits`, `embedded.gex.max_bits` | 3072, 8192, 8192 | 각각 2048 ~ 8192이고 `min ≤ preferred ≤ max` | russh `GexParams` 기본. `GexParams::for_client_config`가 이 순서를 검사하므로(`client/mod.rs:2652`–`:2663`) `SshSettings::set`이 같은 검사를 하고 어긋나면 `SettingsError`로 거절한다 |
| `exec.output_buffer` | 1 MiB | 4 KiB ~ 64 MiB | 앱이 읽지 않은 stdout·stderr를 각각 담는 상한. 넘치면 그 exec만 `OutputLimitExceeded`로 끝낸다([출력 상한](#출력-상한과-세션-수신-루프)) |
| `sftp.max_packet` | 256 KiB | 32 KiB ~ 256 KiB | russh-sftp `Config::max_packet_len` 기본(262144) |
| `sftp.max_concurrent_reads`, `sftp.max_concurrent_writes` | 16, 16 | 1 ~ 64 | russh-sftp 기본 |
| `sftp.max_write_packet` | 32 KiB | 4 KiB ~ 256 KiB | russh-sftp 기본(32768) |
| `sftp.request_timeout` | 10 s | 1 s ~ 10 min | russh-sftp `request_timeout_secs` 기본. 1초 단위다 |

### 고정값

바꿀 수 없다. 조회에는 출처 `fixed`로 나온다.

| 키 | 값 | 근거 |
|---|---|---|
| `config.include_max_depth` | 16 | OpenSSH `READCONF_MAX_DEPTH`(`readconf.c:2629`). 바꾸면 같은 설정 파일이 OpenSSH와 다르게 해석된다 |
| `config.openssh_version` | `OpenSSH_10.6p2` | 이식 기준 판의 `SSH_RELEASE`(`version.h`). `Match version`의 판정 값 |

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
| `rsa` | 0.10.0(2026-11-08) | MIT OR Apache-2.0 | russh `rsa` feature가 들인다. 이 레포 그래프에 새로 든 것은 이것과 아래 둘이다(`Cargo.lock` 대조). `cargo audit`(2026-11-28)에서 이 판에 걸린 공지는 없다 |
| `pkcs1` | 0.8.0(2026-11-02) | Apache-2.0 OR MIT | `rsa`의 의존 |
| `crypto-primes` | 0.7.2(2026-06-10) | Apache-2.0 OR MIT | `rsa`의 의존 |
| `russh-config` | 0.65.3 | Apache-2.0 | 쓰지 않는다. `Match`·`Include`가 없고, 줄을 첫 공백으로만 나누고, 모르는 지시어를 로그만 남기고 넘기고, `ProxyCommand`를 `tokio::process`로 실행한다 |

- 암호 백엔드는 `ring`이다. quinn·rustls가 이미 쓰므로 C 빌드 조건이 하나다([P2P](p2p.md#외부-의존과-고정)).
- 보안 패치: CI의 `audit` 잡이 `cargo-audit` 0.23.0(2026-11-10)으로 `cargo audit`를 돌린다([CI](../dev-guide/ci.md)). russh·russh-sftp·ssh-key의 보안 공지가 나오면 판을 올린다. 2026-11-28 결과: 취약점 공지 0건(대조: 218개 의존을 1571개 공지와 대조), 허용된 경고 1건(`chrono` 0.4.45 유지보수 중단 RUSTSEC-2026-0543, `russh-util`이 들이고 이 작업 전 `Cargo.lock`에도 있었다).
- 게시 2주 규칙: `russh` 0.65.4는 2026-11-21 게시로, 내장 백엔드 첫 커밋 묶음(2026-11-28) 때 2주가 되지 않았다. 이 판을 그대로 둔다(결정됨). 태그가 2026-12-05 전이면 판 커밋에서 다시 정한다(아래 [후속 작업](#후속-작업과-미정)).

## 시험의 자리

구현 작업이 만든다. 정확한 조건은 시험 파일이 정본이다.

### 비교 시험의 기준 클라이언트

`ssh -G`·`ssh-keygen -F` 대조는 기준 판과 같은 판의 클라이언트로만 한다. 호스트에 깔린 `ssh`는 판이 OS마다 달라서 쓰지 않는다.

- 공급: `scripts/install-openssh-ref.sh`가 기준 커밋(`d449e662`, [기준 판](#기준-판))의 소스를 받아 `ssh`와 `ssh-keygen`만 빌드한다(`./configure --without-openssl`은 쓰지 않는다. 배포판과 같은 RSA·ECDSA 지원이 필요하다). 넣는 곳은 `target/tools/openssh-10.6p2/bin/`이다. shellcheck 설치 스크립트(`scripts/install-shellcheck.sh`)와 같은 방식이다: 같은 판이 이미 있으면 다시 빌드하지 않는다.
- CI: 그 폴더를 기준 커밋을 키로 캐시한다. 대조 시험 잡은 Linux·macOS에서 돌고 클라이언트가 없으면 실패한다(필수). Windows는 대조 시험을 돌리지 않는다(이식 크레이트의 단위 시험은 돈다).
- 경로 주입: 시험은 환경 변수 `TASTY_OPENSSH_REF_DIR`(그 `bin/`이 든 폴더)에서 클라이언트를 찾는다. 변수가 없으면 `target/tools/openssh-10.6p2`를 본다. `PATH`의 `ssh`는 찾지 않는다.
- 판 확인: 시험이 먼저 `ssh -V`를 실행해 `OpenSSH_10.6p2`로 시작하는지 본다. 다르면 시험 전체가 실패하고 두 판을 찍는다(다른 판끼리의 차이를 이식 결함으로 잘못 보지 않게). 클라이언트가 없는 로컬 실행에서는 미실행으로 남기고 그 사실과 넣는 명령을 찍는다.
- 고정: 모든 대조는 `ssh -F <fixture>/config -G <대상>`으로 한다. 시스템 설정은 `-F`가 읽지 않는다. known_hosts 대조는 `ssh-keygen -F <호스트> -f <파일>`과 `ssh-keygen -l -E sha1 -F <호스트> -f <파일>`이다. `ssh -G` fixture는 `tasty-net-ssh-config/tests/fixtures/` 아래 파일이다. known_hosts 대조의 틀은 `tests/data/known-hosts/`(`known_hosts.in`, `to_hash.in`, `queries`)이고, 시험이 기준 `ssh-keygen`으로 키를 만들어 틀의 자리 표시(`@ED1@` 등)를 채우고 `to_hash.in`을 `ssh-keygen -H`로 해시해 붙인 파일을 임시 폴더에 쓴다(실제 호스트 키를 레포에 두지 않는다). 시험은 그 폴더를 임시 폴더에 복사하고 복사본에서만 group·other 쓰기 권한을 지운 뒤(OpenSSH가 그런 Include 파일을 `CHECKPERM`으로 거부하고, umask 002 checkout에는 그 권한이 있다) 복사본 경로를 양쪽에 준다. 파일 내용은 바꾸지 않고 checkout도 건드리지 않는다.
- 사례와 기대: `cases`의 한 줄은 `ssh -G`에 줄 인자이고, `=>` 뒤에 기대를 적는다. 적지 않으면 기준 클라이언트가 성공하고 출력이 같으며 이식 크레이트가 거부하지 않아야 한다. `refused <종류> <파일>:<줄> [<지시어>]`(`-o` 값이면 자리 `command-line`)는 기준 클라이언트가 255로 끝나고 오류 출력이 그 파일·줄을 말하며, 이식 크레이트가 같은 종류·파일·줄(·지시어)로 거부해야 한다. 성공하는 사례에서 이식 크레이트만 하는 거부(`security_not_implemented`·`undeterminable`)는 그 사례에 `also-refused <종류>`로 적은 경우만 받는다. `connect-refused <종류> <지시어>`는 `ssh -G`가 끝난 뒤 연결 단계의 거부다(`IdentityFile`·`CertificateFile`의 값 없는 `${}` 등): 기준 클라이언트가 성공하고 출력이 같으며, 이식 크레이트가 그 종류·지시어로 거부해야 한다. 틀린 줄은 fixture 하나에 하나만 둔다.
- 로컬 계정: OpenSSH는 로컬 사용자 이름과 홈을 환경 변수가 아니라 `getpwuid(getuid())`에서 얻는다(`ssh.c:664`, `~` 펼침 `misc.c:1276`–`:1325`, `%d`·`%u`·`Match localuser`·기본 사용자). 그래서 `HOME`·`USER`를 바꿔도 기준 클라이언트의 값은 고정되지 않는다. 시험은 실행 계정의 passwd 항목(이름·홈)을 읽어 이식 크레이트의 `embedded.local_user`·`embedded.home`에 같은 값을 주고, 기대값도 그 값으로 만든다. 시험은 그 값을 기준 클라이언트에서 얻는다: `UserKnownHostsFile ~/@@%u@@%l@@%i` 한 줄짜리 설정으로 `ssh -F <그 파일> -G probe`를 실행해 출력의 `userknownhostsfile` 값을 나눠 홈·이름·호스트 이름·uid를 읽는다(`tests/matches_openssh.rs`). 이것은 대조 시험이 기대값을 맞추는 방법일 뿐이고, 이식 크레이트와 내장 백엔드의 제품 동작은 그 값을 `embedded.local_user`·`embedded.home`과 호출자에게서 받는다.
- 상대 `Include`: `-F`로 준 파일의 상대 `Include`는 실제 계정의 `~/.ssh` 아래로 풀린다(`readconf.c:2094`–`:2108`, 사용자 설정으로 읽기 때문). 대조 fixture는 절대 경로 `Include`만 쓰고, 그 경로는 환경 변수로 적는다(예: `Include ${TASTY_FIXTURE_DIR}/sub.conf`). OpenSSH는 `Include` 인자의 토큰과 환경 변수를 경로 판정 전에 펼치므로(`readconf.c:2085`–`:2093`) fixture 파일을 고치지 않고 checkout 위치와 관계없이 같은 파일을 읽는다. 시험은 같은 값을 기준 클라이언트의 환경과 이식 크레이트에 준다. 상대 경로 규칙은 주어진 홈으로 하는 단위 시험이 원본 규칙대로 본다. 실제 계정의 `~/.ssh`를 시험이 건드리지 않는다.
- 원격 빌드 머신에서 이 스크립트가 돌고, 빌드한 `ssh -V`가 `OpenSSH_10.6p2`로 시작한다(이식 크레이트 1단계에서 확인). 빌드 도구는 C 컴파일러, `make`, `autoreconf`(autoconf), zlib·libcrypto 헤더, Kerberos 개발 파일(Debian 계열 `libkrb5-dev`)이다. Kerberos 개발 파일이 없으면 GSSAPI가 꺼진 빌드가 되고 스크립트가 그 빌드를 놓지 않는다.

### exec·SFTP 시험 서버

- exec·SFTP 시험은 OpenSSH `sshd`(`Subsystem sftp internal-sftp`)에서만 돈다. Windows에서 OpenSSH `sshd`가 없어 demo 시험이 대체 Rust 서버(`tests/support/russh_server.rs`)를 쓰는 경우, 그 서버에는 SFTP subsystem이 없고 exec가 `cmd /C`로 돈다(`:151`–`:175`). 그래서 그 경우 exec·SFTP 시험은 건너뛰지 않고 실패한다. 대체 서버에서 `exec`를 못 하면 재연결 시험이 통과로 나오지 않는 [시험](testing.md#windows)의 기존 규칙과 같은 방식이다. 대체 서버에 SFTP를 더하는 것은 이 계약 밖이다.
- sshd는 exec 요청을 그 사용자의 셸로 `셸 -c <명령>`처럼 실행하고, 보고하는 종료 상태는 그 셸 자식의 것이다(`session.c:1667`–`:1677`, `:2345`–`:2359`). 그래서 명령 문자열은 원격 셸을 지난다. 시험은 이것을 이렇게 다룬다.
  - demo가 시험 도우미 실행 파일 `exec-probe`를 만든다: `--exit <n>`, `--echo-stdin`, `--stderr <문자열>`, `--bytes <n>`(출력 상한 시험), `--kill-self TERM`.
  - 시험은 도우미를 영문자·숫자와 `_-./:\`만 든 경로(공백 없음)의 임시 폴더에 두고, 그 경로를 따옴표 없이 명령의 첫 단어로 쓴다. 그런 단어는 sh·PowerShell·cmd가 모두 따옴표 없이 실행한다([포트 발견의 tasty 관례](repo-boundary.md#포트-발견의-tasty-관례)와 같은 규칙). 인자도 같은 문자만 쓴다. 셸 문법(`exit 3`, `$$`)은 쓰지 않는다.
  - 종료 코드 시험(`--exit`)은 셸이 마지막 명령의 종료 코드를 그대로 돌려주는 것에 기댄다. Unix sh와 Windows OpenSSH의 기본 셸 cmd에서 돈다. 원격 셸이 PowerShell인 서버는 포트 발견 시험(아래 Windows 셸 행)만 쓴다.
  - 신호 종료 시험은 Unix에서만, 명령을 `exec <도우미 경로> --kill-self TERM`으로 준다. POSIX 셸의 `exec`가 셸을 도우미로 바꾸므로 sshd가 보는 세션 자식이 신호로 끝난다. `exec` 없이 부르면 셸이 143으로 끝나 `Signal`이 아니라 `Code(143)`이 된다. 시험 계정의 셸은 POSIX 셸이어야 하고 시험이 그것을 먼저 확인한다.

### 시험 목록

| 시험 | 자리 | 확인하는 것 |
|---|---|---|
| `ssh -G` 대조 | `tasty-net-ssh-config` `tests/matches_openssh.rs` | 같은 설정 묶음(Host·Match·Include·토큰·첫 값 우선·`ProxyJump`와 `ProxyCommand none`의 순서·홉 별칭·`Match final`의 두 번째 읽기·`Match version`)으로 `ssh -G <대상>`(대상이 홉이면 `ssh -G <홉>`)과 이식 크레이트의 `dump_client_config` 형식 출력이 같다. 위 기준 클라이언트로만 한다 |
| `ssh-keygen -F` 대조 | 같은 크레이트 `tests/known_hosts_matches_openssh.rs` | 해시·와일드카드·부정·`[호스트]:포트`·`@revoked`·`@cert-authority`·무효 줄(표식 오류, 키 형식·곡선 불일치, base64 오류, 끊긴 줄, 잘못된 해시)이 섞인 파일에서 `ssh-keygen -F <호스트> -f <파일>`이 찾는 줄(줄 번호·표식)이 `KnownHosts::entries`의 키 형식이 있는 항목과 같고, `-l -E sha1 -F`가 찾는 줄(형식 짧은 이름·SHA1 지문·주석)이 키를 읽은 항목과 같다. 종료 코드는 찾으면 0, 못 찾으면 1이어야 하고(`ssh-keygen.c:1383`), 그 밖의 코드나 시그널은 시험 실패다. 두 호출이 모두 1로 끝나고 크레이트도 항목이 없는 이름을 "함께 거부"로 센다. 포트 없는 재조회는 `-F [호스트]:포트`가 아무 줄도 찾지 않고 `check`가 고른 줄이 `-F 호스트`가 찾는 줄 가운데 있는지로 대조한다(`HostKeyAlias`가 있으면 재조회하지 않음도 본다). 판정(`check`)은 `hostfile.rs` 단위 시험이 원본 순서의 표로 본다 |
| 지시어 분류 | 같은 크레이트 단위 시험 | `keywords[]`의 132개 항목(이름 128개)을 원본 표로 시험 자료에 두고 모든 이름이 분류됐는지 본다. 별칭(`IdentityFile2`·`HostbasedKeyTypes`·`PubkeyAcceptedKeyTypes`·`ChallengeResponseAuthentication`·`KeepAlive`·`SmartcardDevice` 등)이 원래 지시어와 같은 처리를 받는다. 보안 관련 거부마다 거부, 표시마다 `shown`, 적지 않은 기본값에 노트 없음 |
| 모르는 지시어 | 같은 크레이트 단위 시험과 기준 클라이언트 대조 | 기대값이 둘로 나뉜다. (1) 기준 클라이언트와 같아야 하는 것: 모르는 이름이 있으면 파일 전체 거부(적용되지 않는 `Host` 블록 안과, 적용되지 않는 블록의 `Include`가 읽는 파일 안도), 앞에서 적용된 `IgnoreUnknown`이 맞는 이름만 무시, 뒤에 오거나 적용되지 않는 블록의 `IgnoreUnknown`은 무시를 만들지 않음. 이 fixture에는 아는 이름의 보안 거부 값을 넣지 않고, `ssh -G` 종료 코드(0 또는 255)와 거부 여부, 거부면 그 종류와 파일·줄을 맞춘다(위 "사례와 기대"). 사례마다 fixture 폴더 하나를 둔다(`errors-unknown`, `unknown-include`, `unknown-ignored`, `unknown-not-ignored`, `unknown-ignore-after`, `unknown-ignore-inactive`). (2) 이 레포만의 것: 아는 이름의 보안 거부(예: `IgnoreUnknown VerifyHostKeyDNS` 뒤의 `VerifyHostKeyDNS yes`)는 기준 클라이언트가 받아도 이 레포는 거부한다. 이 경우는 기준 대조가 아니라 기대값 "거부"를 적은 단위 시험이다. `ssh -G` 대조에는 해석 결과만 비교한다 |
| `ssh.overrides` 거부 | `tasty-net-ssh` 단위 시험 | `StrictHostKeyChecking=no`·`off`, `NoHostAuthenticationForLocalhost=yes`, `ProxyCommand=…`, 모르는 지시어를 `SshSettings::set`이 두 백엔드 모두에서 거절한다 |
| 확인 끄기 값의 표시 | `tasty-net-ssh-config`와 `tasty-net-ssh` 단위 시험 | 설정 파일·대상 옵션의 `StrictHostKeyChecking no`·`off`와 `NoHostAuthenticationForLocalhost yes`에서 내장 백엔드가 연결을 거부하지 않고, 모르는 호스트는 다음 값(`accept-new` 또는 `ask`→`yes`)대로 판정하고, 바뀐 키는 거절하고, 노트가 남는다. 기준 클라이언트 대조는 두 지시어를 각자의 필드로 본다: `StrictHostKeyChecking no`·`off`는 `stricthostkeychecking false`, `NoHostAuthenticationForLocalhost yes`는 `nohostauthenticationforlocalhost yes`(이때 `stricthostkeychecking`은 기본 `ask`. `hosts` fixture가 설정 파일과 `-o` 두 길로 대조한다). 해석 값은 그 필드로 대조하고, 적용 여부와 노트는 이 레포의 기대값(확인 계속)으로 따로 본다 |
| 세션 종류 고정 | `tasty-net-ssh` 단위 시험 | 대상 옵션·`ssh.overrides`·설정 파일에 `SessionType=none`·`subsystem`이 있어도 터널·포트 발견·exec·SFTP 자식 명령의 세션 종류 플래그·옵션이 대상 옵션 앞에 있고, 기준 클라이언트 `ssh -G`가 첫 값을 낸다(터널 `none`, SFTP `subsystem`, 포트 발견·exec `default`. `ssh -G`는 설정값을 그대로 내고 `exec`·`shell`은 `Match sessiontype`에서만 가른다, `readconf.c:858`–`:867`·`:3791`). `Match sessiontype exec` 블록이 고른 `User`로 포트 발견·exec의 판정을 따로 본다. 내장 백엔드도 같은 값으로 해석하고 노트를 남긴다. 뒤에 온 틀린 `SessionType` 값은 두 백엔드 모두 설정 오류다 |
| 8.7 미만 `ssh` | `tasty-net-ssh` 단위 시험(가짜 `ssh` 스크립트) | `-G -F none -o SessionType=default`에 옵션 해석 오류를 내는 `ssh`면 포트 발견·exec가 처음부터 옵션 없이 한 번만 실행되고, 그 판정이 실행 파일 경로마다 기억된다(다른 경로면 다시 정한다). 원격 명령이 stderr에 `Bad configuration option: sessiontype`을 쓰고 255로 끝나도 exec는 한 번만 실행된다 |
| 폴백 선택 | `tasty-net-ssh` 단위 시험 | 발견 함수에 프로그램 이름과 `PATH` 값을 인자로 주어(프로세스 환경 변수를 바꾸지 않는다) `found{path}`·`not_found{searched}`, Windows의 `.exe` 붙이기, `configured`, `system-ssh`만인 빌드의 `NotFound`를 본다. 다른 프로그램을 실행하지 않는다 첫 커밋 묶음의 시험: `backend.rs`의 `a_bare_name_is_found_in_the_first_directory_that_has_it`·`a_file_without_an_execute_bit_is_not_the_program`·`not_found_lists_every_file_looked_at`·`windows_tries_the_exe_name_after_the_bare_name`·`a_program_with_a_separator_is_that_file_only`·`the_reasons_have_the_names_of_the_design`, `connector.rs`의 `auto_finds_the_launcher_program_and_names_the_file`·`a_missing_program_ends_with_not_found_and_no_fallback`, 두 백엔드 빌드의 `connector/embedded_tests.rs` `auto_falls_back_to_embedded_only_when_the_program_file_is_missing`(발견·미발견·`system` 강제·이유 문구·`inspect`), `embedded_forced_by_the_setting_is_configured`, 내장만 빌드의 `auto_without_the_system_backend_built_is_embedded`(`system_not_built`, `system` 값 거절) |
| 내장 백엔드 attach | `crates/tasty-net-demo/tests/ssh_attach.rs`·`ssh_reverse_attach.rs`의 D2·D3 흐름을 내장 백엔드로 한 번 더(시험 이름 끝 `_embedded`) | demo 자식에게 `ssh.backend = auto`와 런처 프로그램 `ssh`(구분자 없는 이름)를 주고, 자식 환경의 `PATH`를 빈 폴더 하나로 준다. 정본 조건은 내장 백엔드를 쓰는 자식 프로세스 환경에 OpenSSH가 없는 것이다. 시험 도우미가 시험 sshd를 확인하는 로그인은 준비 단계라 시스템 `ssh`를 써도 된다. 구분자 없는 이름이라 Windows의 `%WINDIR%` 규칙을 타지 않으므로 세 OS에서 `not_found`로 내장을 고른다. 연결 목록의 `ssh.backend.reason`이 `not_found`인지 본다. 터널 종료·재연결 단계는 자식 PID 대신 `SessionHook`(시험 훅, `SpawnHook`과 같은 자리)으로 내장 세션을 끊고, 새 `session_id`와 새 연결 항목을 본다 첫 커밋 묶음의 시험: `ssh_attach.rs`의 `connect_and_echo_embedded`·`reconnect_after_session_drop_embedded`·`reconnect_after_server_restart_embedded`, `ssh_reverse_attach.rs`의 `reversed_holder_echo_embedded`·`reconnect_after_session_drop_embedded`. 거절 흐름(conductor M4 Gate 4): `ssh_attach.rs`의 `wrong_token_is_rejected_without_retry_embedded`(D2 틀린 토큰), `ssh_reverse_attach.rs`의 `refused_proposal_stops_without_retry_embedded`(D3 역할 제안 거절)·`self_attach_is_rejected_embedded`(D3 자기 attach 거절). 시스템 시험과 본문 함수를 함께 쓰고 dialer를 띄우는 방법만 다르다. `tasty-net-demo/embedded-ssh`가 켜진 빌드에서만 있다. 세션 끊기는 `demo-dialer`의 stdin `drop-session`(내장 `SessionHook`이 잡은 세션을 끊는다)이 하고, 연결 목록의 `ssh_backend`·`ssh_reason`·`ssh_session`으로 내장·`not_found`·세션 번호 바뀜을 본다 |
| exec | demo 시험 | 위 시험 서버와 `exec-probe`로 두 백엔드의 종료 상태와 출력이 같다(`--exit 0`·`--exit 3`·`--echo-stdin`·`--stderr`). Unix에서 `exec`로 부른 `--kill-self TERM`은 내장이 `Signal{name: "TERM"}`, 시스템이 `Ambiguous255`. `--bytes`가 `exec.output_buffer`를 넘으면 stdout·stderr 각각에서 두 백엔드 모두 `OutputLimitExceeded`이고 같은 세션의 다른 exec는 계속 돈다(내장). 정확히 상한인 경우의 기대값은 후속 작업이 정한 뒤 더한다. 빈 명령은 `EmptyCommand` |
| SFTP | demo 시험 | 위 시험 서버에서 두 백엔드로 파일 쓰기·읽기·`read_dir`·`rename`·`remove_file`의 결과가 같다. 내장은 같은 세션 위에서 터널과 SFTP를 함께 연다. `sftp.*` 값이 두 백엔드의 russh-sftp `Config`에 들어간다. `Subsystem` 줄이 없는 sshd에서는 내장과, `-E`가 확인된 판의 시스템이 `SubsystemUnavailable`이고 Windows 시스템 백엔드는 `EndedBeforeSftp`다(`-E` 확인 전, [후속 작업](#후속-작업과-미정)). 인증이 거절되는 대상에서는 시스템이 `EndedBeforeSftp`, 내장이 인증 오류다. sftp subsystem을 SFTP 협상 전에 stderr에 `subsystem request failed on channel 0`을 쓰고 끝나는 시험 프로그램으로 바꾼 sshd에서는 두 백엔드 모두 `EndedBeforeSftp`다(원격 문구를 거절로 보지 않는다) |
| 세션 키 | `tasty-net-ssh` 단위 시험 | `Match command`·`Match sessiontype`으로 exec만 다른 `User`를 고르는 설정에서 세션이 둘이고, 같은 값을 고르면 하나다. 서버·사용자가 같고 메모리 키만 다른 두 대상은 세션이 둘이고 각 호출이 자기 키로 인증한 세션에서 돈다. 설정의 `SessionType none`이 exec 용도를 바꾸지 않는다(두 백엔드, 시스템은 자식 명령에 `-o SessionType=default`가 있다) |
| `RemoteCommand` 충돌 | `tasty-net-ssh` 단위 시험과 demo 시험 | `RemoteCommand`가 적용된 대상으로 exec·포트 발견·SFTP는 두 백엔드 모두 연결 전에 실패하고, 터널은 연결한다. 기준 클라이언트의 `ssh -s <대상> sftp`도 같은 설정에서 실패한다 |
| 시스템 백엔드 명령 | `tasty-net-ssh` 단위 시험 | 포트 발견·exec·SFTP 자식 명령에 `ControlMaster=no`·`ControlPath=none`이 대상 옵션 앞에 있다 |
| 호스트 키 | `tasty-net-ssh-config`와 `tasty-net-ssh` 시험, demo 시험 하나 | 같은 형식의 바뀐 키 거절, known_hosts에 다른 형식(ed25519)의 키만 있는 호스트가 ecdsa 키를 내면 `HOST_CHANGED`로 거절(`accept-new`에서도 기록하지 않음), `@revoked` 거절, `@cert-authority` 줄만 있는 호스트는 모르는 호스트, `RevokedHostKeys` 파일이 없으면 거절, 1024비트보다 작은 RSA 호스트 키 거절, `accept-new` 기록. 판정 부분은 `ssh-keygen -F`가 아니라 OpenSSH `check_hostkeys_by_key_or_type`의 결과를 원본 표로 둔다 첫 커밋 묶음의 `tasty-net-ssh` 시험(`embedded/hostkey/tests.rs`, 키는 시험이 만든다): `a_known_key_passes_and_another_key_is_refused_as_changed`, `a_revoked_line_refuses_even_with_a_matching_line`, `a_cert_authority_line_is_not_trusted_and_noted`, `accept_new_records_the_key_plain_or_hashed`, `ask_without_an_answer_refuses_and_with_yes_records`(`BatchMode yes` 포함), `revoked_host_keys_files_are_read_before_the_check`(없는 파일·목록·KRL 거부), `a_small_rsa_host_key_is_refused`, `the_record_goes_to_the_first_user_file` |
| 미지원 지시어 표시와 보안 미지원 거부 | `tasty-net-ssh` 단위 시험(`connector/embedded_tests.rs`) | `unsupported_directives_are_shown_and_host_key_checking_is_kept`: 읽었지만 미지원(`GSSAPIAuthentication yes`, 1단계의 `ChannelTimeout`)은 `not_supported`, `StrictHostKeyChecking no`는 `host_key_check_kept`로 파일·줄과 함께 `shown`이고 해석은 계속된다. `security_directives_the_backend_lacks_refuse_before_connecting`: `ProxyCommand`·`KnownHostsCommand`·`VerifyHostKeyDNS yes`·`PubkeyAuthentication host-bound`가 연결 전 `security_not_implemented`로 거부되고 `inspect`도 같은 거부를 낸다. `an_unknown_keyword_in_a_file_refuses_like_openssh`: 모르는 지시어는 `unknown_keyword` 거부 |
| 리스너 판정 | `tasty-net-ssh` 시험 | 사유 코드 2만 `RemoteListenerAbsent`, 1·3·4는 `ChannelOpenRefused{code}` |
| 메모리 키 | `tasty-net-ssh` 단위 시험 | [android 요구](#android-요구의-자리)의 메모리 키로 인증하고, `inspect`·`Debug` 출력에 키 바이트가 없다. 시스템 백엔드를 고르면 `BackendUnavailable{reason: memory_key}` |
| Windows 셸 포트 발견 | demo 시험(Windows OpenSSH `sshd`) | 원격 기본 셸이 PowerShell·cmd인 서버에서 내장 백엔드의 포트 발견이 시스템 백엔드와 같은 레코드를 읽는다([android 요구](#android-요구의-자리) #11) |

## android 요구의 자리

tasty-android가 요구한 목록(director 2865, 앱 쪽 보고서 R2의 번호) 가운데 Rust API 쪽 자리다. FFI로 무엇을 내는지는 [모바일 바인딩 층](mobile.md#android-요구의-자리)이 맡는다. 여기에는 경계와 계약만 적고, 세부 API는 내장 SSH 백엔드 작업이 정한다.

| # | 요구 | 이 레포의 계약 | 세부를 정할 곳 |
|---|---|---|---|
| 1 | 앱이 준 폴더의 OpenSSH 형식 config·키·known_hosts로 접속, 비밀번호·passphrase·keyboard-interactive·호스트 키 확인을 콜백으로 | `embedded.home`·`embedded.ssh_dir`([설정값](#설정값))과 `SshPrompt`. 모르는 지시어 거부와 `@cert-authority` 불신은 [지시어 분류](#지시어-분류)·[호스트 키 판정](#호스트-키-판정)대로 | 내장 SSH 백엔드 |
| 2 | 키를 파일 대신 메모리 바이트로 주기 | 대상 옵션에 메모리 키 원천을 더한다. 메모리 키는 `IdentityFile`보다 앞에 내민다(OpenSSH의 `-i`와 같은 자리). 바이트는 넘겨받은 뒤 이 레포가 소유하고 세션이 끝나거나 대상이 지워지면 지운다(zeroize). `inspect`·`ConfigNote`·`Debug`·오류에는 키 바이트도 passphrase도 내지 않고 "메모리 키 n개"와 공개키 지문만 낸다. 시스템 백엔드는 메모리 키를 쓸 수 없으므로 그런 대상은 내장 백엔드가 필요하다(`auto`에서 시스템을 고르면 `BackendUnavailable{reason: memory_key}`로 거절, 백엔드를 바꾸지 않는다). #13: 메모리 키는 설정의 `IdentityFile` 참조와 이름으로 잇지 않는다. 앱이 그 대상에 준 키가 그 대상의 키다. `-i`와 같은 자리이므로 OpenSSH의 두 규칙을 그대로 따른다. 하나, identity가 하나라도 있으면 기본 키 목록(`id_rsa`·`id_ecdsa`·`id_ed25519` 등)을 채우지 않는다(`readconf.c:2930`–`:2941`, `-i`는 `ssh.c:845`–`:854`). 그래서 메모리 키가 있는 대상은 설정에 `IdentityFile`이 없어도 기본 키 파일을 내밀지 않는다. 둘, `IdentitiesOnly yes`는 identity 목록에 없는 agent 키(와 목록에 없는 PKCS#11 키)를 뺄 뿐이다(`sshconnect2.c:1866`·`:1901`). 그래서 메모리 키는 `IdentitiesOnly`와 상관없이 쓰인다. 고른 메모리 키는 공개키 지문 목록으로 세션 키에 든다([용도마다 해석하고 같을 때만 세션을 나눈다](#용도마다-해석하고-같을-때만-세션을-나눈다)) | 내장 SSH 백엔드 |
| 7+ | 포트 발견 대상과 `direct-tcpip` 터널 대상의 일치 | [포트 발견과 터널의 대상 일치](#포트-발견과-터널의-대상-일치). 다르면 `ConnectError::TargetMismatch` | 내장 SSH 백엔드 |
| 3 | exec: 스트림 또는 상한 있는 수집, 종료 상태, 시간 제한·취소, env | [공개 API](#공개-api-tasty-net-ssh)와 [출력 상한](#출력-상한과-세션-수신-루프) | 내장 SSH 백엔드 |
| 4 | SFTP 연산과 실패 사유 구분 | 연산: `read_dir`(이름·종류·크기·mtime·권한·링크 대상), `metadata`·`symlink_metadata`, `canonicalize`, 범위 읽기(offset·len). `SftpError`는 적어도 `PermissionDenied`·`NotFound`·`SubsystemUnavailable`·`EndedBeforeSftp{ssh_exit, stderr_tail}`·`SessionLost`·`Timeout`·`Other{code, message}`(SFTP 상태 코드 그대로)를 가른다. `SubsystemUnavailable`은 서버가 `subsystem` 요청을 거절한 것을 원격 출력과 분리된 증거로 확인한 경우만이다: 내장은 그 요청의 `SSH_MSG_CHANNEL_FAILURE`. 시스템은 자식 stderr를 증거로 쓰지 않는다. stderr에는 OpenSSH 자신의 진단과 원격 채널의 stderr(확장 데이터)가 섞이므로, 원격 프로그램이 같은 문구를 쓰고 끝나도 구분할 수 없기 때문이다. 대신 SFTP 자식에 `-E <임시 파일>`을 주어 OpenSSH의 로그를 그 파일로 보낸다(`ssh.c:735`–`:737`, `:1167`–`:1168` `log_redirect_stderr_to`). 원격 stderr는 그 파일에 가지 않는다. 그 파일에 `subsystem request failed on channel` 줄(`clientloop.c:835`–`:846`)이 있을 때만 `SubsystemUnavailable`이고, 그 밖에 SFTP 판 협상 전에 자식이 끝나면(인증 거절, 호스트 키 불일치, 접속 실패, 원격 프로그램이 일찍 끝남, `LogLevel QUIET`) 분류할 수 없는 조기 종료 `EndedBeforeSftp`다. Windows OpenSSH에서 `-E`가 같은지는 확인하지 않았다([후속 작업](#후속-작업과-미정)). 확인되지 않은 판에서는 늘 `EndedBeforeSftp`다. SFTP 상태 코드 `SSH_FX_PERMISSION_DENIED`(3)·`SSH_FX_NO_SUCH_FILE`(2)을 앞의 둘로 옮긴다 | 내장 SSH 백엔드 |
| 5 | SFTP 쓰기 | 공개 API에 이미 있다. 앱이 쓸지는 앱 쪽 결정 뒤다 | 보류 |
| 6 | 터널·exec·SFTP가 한 세션을 함께 쓰는 경계 | [용도마다 해석하고 같을 때만 세션을 나눈다](#용도마다-해석하고-같을-때만-세션을-나눈다) | 정해짐 |
| 6b | attach 없는 일반 SSH 세션의 수명·끊김을 연결 목록과 같은 상태 API로 | [SSH 세션 목록](#ssh-세션-목록)이 맡는다. transport의 [연결 목록](transport.md#연결-목록)은 바꾸지 않는다 | 내장 SSH 백엔드 |
| 11 | 내장 백엔드에서 PowerShell·cmd 포트 발견 | 포트 발견은 두 백엔드가 같은 명령 문자열을 쓴다([포트 발견의 tasty 관례](repo-boundary.md#포트-발견의-tasty-관례)). 차이는 시스템 백엔드가 명령을 `ssh`의 인자로 넘기는 것과 내장 백엔드가 `exec` 요청에 그대로 싣는 것뿐이다. OpenSSH는 인자를 공백으로 이어 한 문자열로 보내므로 같은 문자열이 가야 한다. 시험(위 목록)이 원격 셸 PowerShell·cmd 서버에서 이를 확인한다 | 내장 SSH 백엔드 |
| 14 | 직접 LAN 연결 판정과 두 차단 사유 | 판정과 오류 갈래는 [모바일의 직접 LAN 연결](mobile.md#직접-lan-연결-android-지역망-권한)이 정본이다. 이 레포 쪽 계약: 내장 백엔드가 첫 SSH 홉의 소켓을 열기 전에 앱의 지역망 판정을 묻고, 거절이면 소켓을 열지 않고 `LocalNetworkDenied`로 끝낸다. 소켓 연결이 실패하면 그 소켓을 가진 코드가 닫기 전에 차단 사유를 읽어 지역망 차단이면 `LocalNetworkBlocked`로 끝낸다. 두 변형은 transport·exec·SFTP 어느 입구로 연결해도 그 입구의 오류에 실려 간다. 싣는 자리와 연결 목록 원인 이름은 세부다 | 내장 SSH 백엔드 |

#7·#7b·#8·#9·#10·#12·#15와 #7+의 FFI 쪽은 FFI 경계 항목이라 [모바일 바인딩 층](mobile.md#android-요구의-자리)에 있다.

## 후속 작업과 미정

| 항목 | 상태 |
|---|---|
| 1단계 feature 분리 | 들어갔다(transport ssh feature 분리). 검사 단계 `scripts/check.sh transport-no-ssh` |
| 이식 크레이트 `tasty-net-ssh-config` | 1단계가 만들었다: `ssh_config` 해석(`readconf.c`, `ssh.c` 설정 단계, `dump_client_config`)과 결과 API, 기준 클라이언트 `ssh -G` 대조 시험. 2단계가 더했다: known_hosts 읽기와 판정(`KnownHosts`), `ssh-keygen -F` 대조, `IdentityFile`·`CertificateFile` 펼침과 출처(`identity_files`·`certificate_files`), known_hosts 파일 목록과 조회 이름. 남은 것: `accept-new` 기록(`add_host_to_hostfile`·`HashKnownHosts`), 메모리 키 입구(아래) |
| 2단계 내장 백엔드·선택 규칙·exec·SFTP·cargo-audit | 첫 커밋 묶음이 들어갔다: 선택 규칙과 연결 정보, 내장 `-L` 터널(`direct-tcpip`)과 포트 발견, 호스트 키 판정, 키 파일·인증서·agent·비밀번호·keyboard-interactive 인증, `ProxyJump`, 지시어 표시·거부, CI `cargo audit`. 남은 것: exec·SFTP, 메모리 키(android #2), #3·#4·#6b·#11, 내장 쪽 #7+, #13·#14 |
| `@cert-authority`와 호스트 인증서 | 2단계 밖. 넣을 때 인증서 형식·주체 이름·중요 옵션·CA 서명 알고리즘·`@revoked` 적용을 따로 정한다 |
| 곡선 검사·CA 서명 | 결정 대기. `check`가 기준판과 달리 통과시키는 입력이 둘 남았다([OpenSSH 해석 이식 크레이트](#openssh-해석-이식-크레이트)의 키 비교): 표식 없는 줄과 같은 곡선 밖 ECDSA 점, 평문 키가 줄에 있는 CA 서명이 틀린 호스트 인증서. 곡선 연산·서명 확인이 들어 새 의존 없이는 기준판처럼 할 수 없다. 그동안 `check`는 전송 계층이 키로 읽고 서명을 확인한 서버 키를 받는다는 전제다. CA 키가 `@revoked`인 호스트 인증서는 닫혔다(`Revoked`, [호스트 키 판정](#호스트-키-판정)) |
| 인증서의 포트 없는 재조회 순서 | 고치지 않음(기준판보다 더 거절하는 쪽). 기준판은 인증서로 포트 포함 이름을 먼저 보고(`@cert-authority` 줄만 본다, `hostfile.c:349`–`:404`) `HOST_NEW`면 인증서로 포트 없는 이름을 다시 보며, 그 안에서 맞는 CA가 없으면 평문 키로 내려가 다시 본다(`sshconnect.c:1473`–`:1486`). 이 크레이트는 처음부터 평문 키로 포트 포함 이름을 본다. 그래서 포트 포함 이름에 다른 평문 키만 있고 포트 없는 이름에 그 평문 키가 있으면, 기준판은 통과하고 `check`는 `Mismatch`다 |
| 연결 단계 거부 순서 | 고치지 않음(둘 다 거부). 기준판은 `GlobalKnownHostsFile`을 먼저 펼친다(`ssh.c:1791`–`:1792`, 그다음 `:1793`–`:1794`의 사용자 파일). 이 크레이트는 사용자 파일을 먼저 본다. 두 목록이 모두 `~other` 펼침에 실패하면 `Refusal`의 지시어 이름만 다르다 |
| Windows의 설정 경로 규칙 | 175500 질문. 이식 크레이트는 기준판(portable)대로 `/`만 경로 구분자로 본다([내장 백엔드가 읽는 파일](#내장-백엔드가-읽는-파일)). Win32-OpenSSH 규칙(드라이브 문자와 `\`를 절대 경로로 보기, 시스템 설정의 상대 `Include`를 `%PROGRAMDATA%\ssh` 아래로 풀기)을 따를지. 따르면 `path_absolute`·glob·`SSHDIR`을 바꾸고 Include 시험을 Windows에서도 돌린다 |
| IPv6 scope의 정규화 | 175500 질문. 기준판은 `getaddrinfo`·`getnameinfo`가 scope를 OS 인터페이스 표로 풀어 `hostname`을 바꾼다(`fe80::1%0001`→`fe80::1%lo`, 16진 소문자화). 이식 크레이트는 인터페이스 표를 읽지 않아 scope가 붙은 주소를 적힌 그대로 둔다. 인터페이스 표를 읽을지, 그 값이 `%h`·호스트 키 조회 이름에 쓰이는 차이를 둘지 |
| 숫자 주소의 OS별 규칙 | 결정됨(director 3106). 실행 OS의 libc 규칙(glibc·bionic·Darwin)을 따르고, 규칙이 없는 OS에서 라이브러리마다 갈리는 표기(앞자리 0·`0x` 16진·짧은 형식·`AddressFamily inet`의 IPv4 매핑 IPv6·IPv6 그룹과 점 표기 꼬리 첫 조각의 자리 수·점 표기 꼬리의 앞자리 0)와 Darwin의 둘째 워드 scope 주소는 거부한다([옮기는 원본](#옮기는-원본)의 숫자 주소). bionic 규칙의 실제 기기 확인은 android 쪽이 한다 |
| 설정 파일 소유자의 판정 시점 | 175500 질문. `load`는 로컬 uid를 입력으로 받지 않아 사용자 기본 파일의 모드만 보고 읽으며, 소유자는 `resolve`가 판정한다([결과 API](#결과-api)의 "파일 읽기"). `ConfigInputs`에 로컬 uid를 받아 `load`에서 소유자까지 판정할지(기준판처럼 다른 사용자 소유 파일을 읽지 않는다) |
| 비교 시험의 기준 클라이언트 빌드 | 원격 빌드 머신에서 확인했다. `scripts/install-openssh-ref.sh`가 기준 커밋을 빌드하고 `ssh -G` 대조가 돈다([비교 시험의 기준 클라이언트](#비교-시험의-기준-클라이언트)의 빌드 도구) |
| 채널 열기 거절의 설명 문구 | 후속 작업(내장 SSH 백엔드). 실패한 호출자가 채널 ID를 받지 못하는 russh 0.65.4에서 `Handler::channel_open_failure`의 설명을 그 오류에 맞출 수 있는지(같은 세션의 열기 요청 순서로 맞추기 등), 없으면 `ChannelOpenRefused{code}`만 둔다 |
| exec 출력 상한의 경계와 관측 순서 | 후속 작업(내장 SSH 백엔드). 정확히 `exec.output_buffer`만큼의 출력이 성공인지, 이미 담긴 바이트와 `OutputLimitExceeded`를 `Read`(`io::Error`)와 `wait`에서 어떤 순서로 보는지, stdout·stderr별 시험 |
| SSH 세션 목록의 필드와 보관 상한 | 후속 작업(내장 SSH 백엔드). [SSH 세션 목록](#ssh-세션-목록)의 경계 안에서 필드 표, 닫힌 세션의 보관 상한, 이벤트 종류 |
| Windows OpenSSH의 `-E` | 후속 작업(내장 SSH 백엔드). 시스템 SFTP의 `SubsystemUnavailable` 판정에 쓰는 `-E <임시 파일>`이 Windows OpenSSH에서도 로컬 로그만 그 파일로 보내는지. 확인 전에는 Windows 시스템 백엔드에서 늘 `EndedBeforeSftp` |
| 시스템 SFTP의 `-E` 로그와 `stderr_tail` | 후속 작업(내장 SSH 백엔드). `-E`로 OpenSSH 진단이 로그 파일로 가므로 `EndedBeforeSftp`의 `stderr_tail`에 로그 파일 끝을 함께 싣는지, 임시 파일의 위치·권한·지우는 때 |
| `ChannelTimeout` 구현 | 결정됨(conductor M4 1단계 판단 답): 1단계는 적용된 값을 `not_supported` 노트로 표시만 하고, 2단계에서 exec·SFTP와 함께 구현한다. 2단계가 정할 것: 기준판 규칙대로의 종류 패턴(예: `sess*`) 비교와 `0`을 포함한 첫 일치, `global` 타이머의 뜻, 용도마다 다른 `global` 값을 고른 경우의 세션 공유, 시험 |
| 터널 자식 stderr의 출처 | 후속 작업(내장 SSH 백엔드). 시스템 터널의 `ForkAfterAuthentication` 재시도는 자식 stderr의 문구로 정하는데, 그 stderr에는 로컬 `Match exec` 명령의 출력과 서버의 인증 전 Banner도 섞일 수 있다. 원격 출력과 무관한 증거(8.7 미만 판정과 같은 `ssh -G` 확인 등)로 바꿀지 |
| 8.7 미만 판정의 기억 수명 | 후속 작업(내장 SSH 백엔드). 같은 경로의 `ssh`가 갱신으로 판이 바뀐 경우, 기억을 프로세스 동안 둘지와 파일이 같은지를 경로만으로 볼지(수정 시각 등도 볼지) |
| 포트 발견과 터널의 대상 일치 | 시스템 백엔드는 들어갔다(director 2937, 원인 `target_mismatch`, 재시도하지 않음, 시험 `connector.rs` `a_tunnel_to_another_target_is_refused_before_it_starts`). 남은 것: 내장 백엔드의 비교(android #7+ 내장 쪽) |
| 메모리 키와 `IdentityFile` 이름 | 후속 작업(내장 SSH 백엔드). 앱 제안(감싼 키 이름이 `IdentityFile ~/.ssh/<이름>`이나 기본 키 자리 이름과 같으면 그 자리의 키로 쓰기)을 대상 옵션과 따로 더할지. `ProxyJump` 홉에 메모리 키를 내밀지(OpenSSH는 점프 홉의 `ssh`에 `-i`를 넘기지 않는다, `ssh.c:1307`–`:1325`). 기본 키 목록을 채우지 않는 것과 `IdentitiesOnly`의 시험 |
| 호스트 키 알고리즘 목록의 결과 API | 후속 작업(내장 SSH 백엔드, 2단계). 이식 크레이트는 조립(`kex_assemble_names`)에 실패하는 `HostKeyAlgorithms`(예: `!ssh-rsa`)를 `resolve`에서 설정 오류로 거부하고, 조립한 목록은 `ssh -G` 형식 출력에만 쓴다. 조립 결과를 구조화된 API로 내기, known_hosts에 있는 키 종류를 앞에 두는 순서 변경(`sshconnect2.c:150`–`:236`), russh가 아는 알고리즘과의 교집합은 2단계가 정한다 |
| 메모리 키를 넣는 해석 입구 | 175500 질문. 메모리 키를 `-i`처럼 명령행 identity로 넣어 기본 키 채움(`readconf.c:2930`–`:2941`)을 OpenSSH와 같게 하는 입구와, 메모리 키를 `IdentityFile`·`IdentitiesOnly` 값에 잇는 규칙. 결과 API의 구별은 이식 2단계가 냈다: `identity_files`의 `origin`이 `default`면 기본 키 자리, 그 밖은 사용자가 적은 값이다([결과 API](#결과-api)) |
| 지역망 판정과 차단 사유 | 후속 작업(내장 SSH 백엔드). 지역망으로 볼 주소 범위(사설·링크 로컬 등)를 Android 정의로 확인해 정하기, `.local` 판정을 유효 `HostName`에 하는지, `LocalNetworkDenied`·`LocalNetworkBlocked`를 싣는 오류 타입과 연결 목록 원인 이름, 시험. 차단 사유를 읽는 NDK 호출([모바일](mobile.md#직접-lan-연결-android-지역망-권한))의 자리는 결정됨(director 2937): `tasty-net-ssh`를 `deny(unsafe_code)`로 바꾸고 `cfg(target_os = "android")` 모듈 하나에서만 허용한다([크레이트 구성](crates.md#unsafe-규칙)). 그 모듈 밖에 `unsafe`가 없는지 보는 검사(grep 또는 clippy 설정)의 모양 |
| E1을 판정할 CI 조합 | 정해졌다. clippy·test 잡(세 OS)의 단계 `clippy, both SSH backends`와 `test, both SSH backends (E1)`이 `tasty-net-ssh/embedded-ssh,tasty-net-transport/embedded-ssh,tasty-net-demo/embedded-ssh`로 빌드하고 위 `_embedded` 시험을 돌린다. 같은 잡의 `embedded SSH backend alone`이 모바일 조합(내장만)의 clippy·시험이다 |
| `russh` 판의 게시 기간 | 결정됨(conductor M4 1단계 판단 답): `=0.65.4`(2026-11-21 게시)를 그대로 둔다. 태그 시점 조건: 태그가 2026-12-05 전이면 판 커밋에서 다시 정한다 |
| 연결 목록 `applied`의 백엔드 키 | 결정됨(conductor M4 1단계 판단 답): 더하지 않는다. 백엔드와 이유는 `ssh` 필드, `embedded.*` 값은 `SshConnector::inspect`의 `settings`다([transport 연결 목록](#transport-연결-목록)) |
| 터널 채널이 세션 수신 루프를 멈추는 경우 | 알려진 한계. 로컬 쪽이 읽지 않는 터널 채널은 russh 큐를 채워 같은 세션 전체를 멈춘다. 링크 읽기가 늘 읽으므로 막는 장치를 두지 않는다 |
| android 요구의 세부 API | 후속 작업(내장 SSH 백엔드, 바인딩 크레이트). [android 요구의 자리](#android-요구의-자리) |
| 내장 세션을 붙들어 두는 시간 | 정하지 않음. 모바일에서 연속 exec마다 인증하는 비용이 문제가 되면 android 요구와 함께 설정값으로 정한다 |
| tasty 설정 화면의 SSH 항목 | tasty 전환 때. 이 레포 범위 밖 |
| 설정값이 아닌 동작값의 조회 | 정하지 않음. `ConnectionAttempts`·`NumberOfPasswordPrompts`처럼 지시어로만 정해지는 횟수는 `resolved.sources`를 연결이 어디로 가는지 정하는 값으로 좁힌 뒤(위 `ResolvedTarget.sources`) 보이는 곳이 없다. 모든 횟수는 조회할 수 있다는 설정값 규칙([크레이트 구성](crates.md#설정값-규칙))과 맞추려면 그 목록에 두 지시어를 더하거나 따로 조회 항목을 둔다 |
