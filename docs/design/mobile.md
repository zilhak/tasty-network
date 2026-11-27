# 모바일 (Android·iOS)

이 레포의 크레이트가 Android와 iOS에서 빌드되고 동작하는 데 필요한 경계를 정한다. 대상 target, 크레이트마다의 판정, 다른 프로그램 실행 금지 범위, SSH 백엔드(시스템 ssh와 내장 백엔드), 앱 수명, 바인딩 층, 모바일 cross check를 다룬다. 크레이트 이름과 의존 방향의 정본은 [크레이트 구성](crates.md)이다.

## 사용자 결정

| 날짜 | 원문 | 이 문서의 결과 |
|---|---|---|
| 2026-11-27 | "tasty 전환과 함께, 안드로이드에도 구현이 같이 들어가야해 안드로이드나 ios 등에서도 동작 가능한 형태로 만들 수 있는 형태의 베이스가 만들어지는거 맞나?" | 이 문서 전체 |
| 2026-11-27 | iOS 범위 "구조만 iOS 가능하게" | iOS는 빌드 검사와 Swift 바인딩 생성 구조까지. iOS 앱은 만들지 않는다 |
| 2026-11-27 | "그래 폴백으로 넣자 / 환경 따지지말고 설치된 openssh가 없다면 자체 ssh 모듈로 동작하는거야" | 아래 "SSH 백엔드"의 선택 규칙. OS로 나누지 않는다 |
| 2026-11-27 | "그냥 tasty 구현쪽에서 옵션에 ssh 관련 설정을 넣으면 될거야" | 백엔드 선택과 파일 위치·덮어쓰기 값을 라이브러리 설정값으로 받는다. tasty 설정 화면은 tasty 전환 때 만든다(이 레포 범위 밖) |
| 2026-11-27 | "그리고 자체 ssh의 설정파일이나 키파일 자체는 Openssh랑 동일한 방식으로 최대한 동작 가능하게 해" | 내장 백엔드는 OpenSSH와 같은 형식의 파일을 같은 뜻으로 읽는다. 앞 결정의 "OpenSSH 파일을 흉내 내지 않는다"는 이 결정으로 철회됐다. 아래 "지시어 지원 표" |

## 대상 target

| target | 종류 | 최소 OS·툴체인 | 근거·확인 상태 |
|---|---|---|---|
| `aarch64-linux-android` | 실제 기기 | NDK API 수준 29 | tasty-android 앱의 `minSdk = 29`(`tasty-android` 레포 `app/build.gradle.kts`)에 맞춘다. 앱보다 낮은 수준으로 빌드할 까닭이 없다. NDK 판은 CI 작업이 고정하고 [CI](../dev-guide/ci.md)에 근거를 적는다 |
| `x86_64-linux-android` | 에뮬레이터 | NDK API 수준 29 | 위와 같다 |
| `aarch64-apple-ios` | 실제 기기 | iOS 배포 대상: rustc 1.96.0 기본값 10.0을 그대로 둔다 | `rustc --print deployment-target --target aarch64-apple-ios` = `IPHONEOS_DEPLOYMENT_TARGET=10.0`(2026-11-27 확인). 의존(tokio·quinn·ring·russh)이 그보다 높은 판을 요구하는지는 확인하지 않음. iOS 앱을 만들지 않으므로 앱의 배포 대상은 없다 |
| `aarch64-apple-ios-sim` | 시뮬레이터 | rustc 기본값 14.0 | 같은 명령으로 `IPHONEOS_DEPLOYMENT_TARGET=14.0`(2026-11-27 확인) |

- 툴체인은 레포의 `rust-toolchain.toml`(1.96.0)이다. 네 target 모두 그 판의 std를 `rustup target add`로 받는다.
- Android target은 C 코드를 빌드하는 의존(`ring`, 아래 표)이 있어 NDK의 clang이 필요하다. `CC_<target>`·`AR_<target>`·`CARGO_TARGET_<TARGET>_LINKER`를 NDK의 `<target><API 수준>-clang`으로 준다. iOS target은 macOS의 Xcode 툴체인이 필요하다.
- 32비트 Android(`armv7-linux-androideabi`, `i686-linux-android`)는 대상이 아니다. 요청의 네 target에 없다.

## 크레이트별 판정

빌드 확인은 develop b103e90에서 했다. 네 target 가운데 원격 빌드 환경과 이 작업 머신에 설치된 모바일 target은 `aarch64-linux-android` 하나뿐이고 NDK는 두 곳 모두 없다. 그래서 나머지 세 target과 C 코드가 있는 크레이트는 "확인하지 않음"이다. 설치는 하지 않았다.

| 크레이트 | 모바일 대상 | `aarch64-linux-android` `cargo check --lib` (b103e90) | 막거나 주의할 것 (근거) |
|---|---|---|---|
| `tasty-net-frame` | 대상 | 통과 | 없음. 외부 의존 없이 std만 쓴다 |
| `tasty-net-identity` | 대상 | 통과 | 파일 잠금 `File::lock`(`crates/tasty-net-identity/src/secret_file.rs:281`), `O_NOFOLLOW`·`geteuid`(`src/secret_file/unix.rs:123`, `:187`). Android·iOS 모두 `cfg(unix)` 경로를 탄다. 컴파일은 되지만 `File::lock`이 두 OS에서 실제로 잠그는지(`Unsupported`를 돌려주지 않는지)는 확인하지 않음. 키 디렉터리는 소비자가 준다([기기 키·토큰](identity.md#저장-위치)). 앱은 자기 전용 폴더(Android `filesDir`, iOS Application Support)를 준다. `std::process::id()`(`src/secret_file.rs:172`)는 임시 파일 이름용이고 다른 프로그램을 실행하지 않는다 |
| `tasty-net-topology` | 대상 | 통과 | 없음 |
| `tasty-net-ssh` | 대상(`embedded-ssh` feature만, 아래 "SSH 백엔드") | 통과 | 시스템 `ssh` 실행: `std::process`를 `src/spawn.rs:7`·`:158`, `src/discovery.rs:9`, `src/tunnel.rs:7`, `src/cancel.rs:5`가 쓴다. std에 `std::process`가 있으므로 모바일 target에서도 컴파일된다. 그래서 빌드 통과는 금지 범위를 지킨다는 증거가 아니다(아래 "다른 프로그램 실행 금지"). 이 코드는 `system-ssh` feature 안으로 옮기고 모바일 빌드에서 뺀다 |
| `tasty-net-transport` | 대상(`ssh` 쪽은 `embedded-ssh`만) | 통과 | `src/`에 `std::process` 없음(같은 검색으로 ssh 크레이트에서는 파일 다섯 개가 나온다). 소켓 깨우기 `shutdown(Both)`(`src/wake.rs:15`)는 Unix 경로를 탄다. 지금은 `tasty-net-ssh`를 무조건 의존한다(`Cargo.toml`). p2p 의존이 더해지면 아래 rendezvous와 같은 C 빌드 조건이 생긴다 |
| `tasty-net-rendezvous` 라이브러리 | 대상(p2p가 쓰는 형식·코덱·TLS 설정). 서버 본체도 같은 라이브러리라 함께 컴파일된다 | 실패: `ring` 0.17.14 빌드 스크립트가 `aarch64-linux-android-clang`을 찾지 못함(NDK 없음). 이 크레이트의 Rust 코드까지는 가지 않아 확인하지 않음 | `ring`(C, `quinn`·`rustls` 경유). tokio `signal` feature(`Cargo.toml`)는 실행 파일용이다. `std::process`는 실행 파일 `src/bin/tasty-rendezvous.rs`에만 있다(`src/` 라이브러리 쪽 0건). 보내기 오류 번호는 `cfg(unix)`의 `EMSGSIZE`(`src/packet_io.rs:244`) |
| `tasty-net-rendezvous` 실행 파일 `tasty-rendezvous` | 대상 아님 | 해당 없음 | 서버다. 모바일 기기에서 띄우지 않는다. 모바일 cross check는 `--lib`로 본다 |
| `tasty-net-p2p` | 대상 | 확인하지 않음(develop에 없다. 코드 근거는 T3 lane 커밋 c11f671) | `ring`(위와 같다). 키 디렉터리의 잠금 파일 `p2p.lock`을 `try_lock`으로 잡는다(`crates/tasty-net-p2p/src/endpoint.rs:494`–`:498`@c11f671). identity와 같은 `File::lock` 확인이 필요하다. `socket2`로 `IPV6_V6ONLY` 끄기·버퍼 크기(`src/socket.rs:122`–`:130`@c11f671). 소유 런타임은 멀티스레드 tokio(`src/runtime.rs:113`@c11f671, 워커 수는 설정값). `std::process` 0건 |
| `tasty-net-demo` | 대상 아님 | 해당 없음 | 예제 바이너리와 두 프로세스 시험, sshd를 띄운다 |
| `tasty-net-ffi`(새 크레이트, 아래 "바인딩 층") | 대상 | 아직 없음 | 위 대상 크레이트만 의존한다 |

- 확인 로그: 원격 `mob-design-targets`(rbuild 칸 2, b103e90, `RBUILD_RC=0`, 크레이트별 rc는 로그 안). 원격의 설치된 target: `aarch64-linux-android`, `x86_64-pc-windows-gnu`, `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`.
- 판정의 정본은 모바일 cross check(아래)다. 이 표는 그 단계가 생기기 전의 확인 기록이고, 그 단계가 처음 성공하면 "확인하지 않음" 칸을 그 실행으로 고친다.

## 다른 프로그램 실행 금지

모바일 target에서 컴파일되는 코드는 다른 프로그램을 실행하지 않는다. iOS는 앱이 자식 프로세스를 만들 수 없고, Android도 앱 안에 `ssh` 같은 실행 파일이 없다.

- 금지 대상: `std::process::Command`·`Child`·`Stdio`, `std::os::*::process::CommandExt`, `tokio::process`, 그리고 이것들을 쓰는 의존(예: `russh-config`의 `ProxyCommand` 실행, 아래). `std::process::id()`·`exit()`는 다른 프로그램을 실행하지 않으므로 해당하지 않는다.
- 경계(크레이트·feature 단위):

  | 자리 | 다른 프로그램 실행 |
  |---|---|
  | `tasty-net-ssh`의 `system-ssh` feature 안(시스템 `ssh` 백엔드, 자식 생성 훅, 자식 회수) | 허용. 모바일 빌드는 이 feature를 켜지 않는다 |
  | `tasty-net-ssh`의 나머지(`embedded-ssh` 포함), frame, identity, topology, transport, p2p, rendezvous 라이브러리, ffi | 금지 |
  | `tasty-net-rendezvous` 실행 파일, `tasty-net-demo`, 모든 크레이트의 시험 코드(`tests/`, `#[cfg(test)]`) | 허용. 모바일 대상이 아니다 |

- 검사 방법:
  1. 정적 검사 단계 `no-process-spawn`(`scripts/check.sh no-process-spawn`, 후속 작업이 만든다): 위 "금지" 자리의 `src/`에서 금지 대상 경로를 찾는다. `tasty-net-ssh`는 `system-ssh` feature로만 컴파일되는 모듈 폴더(`src/system/`)를 빼고 본다. 시험 코드는 보지 않는다. `no-hidden-constants`처럼 검사 자체의 시험(금지 경로를 둔 입력은 실패, 허용 자리만 둔 입력은 통과)을 둔다.
  2. 모바일 cross check(아래)가 `system-ssh` 없이 빌드한다. 금지 자리의 코드가 `system-ssh` 쪽 항목을 쓰면 이 빌드가 실패한다.
  3. 의존 쪽은 `cargo tree --target <모바일 target> -e normal`에 `tokio`의 `process` feature와 `russh-config`가 없는지 같은 단계가 본다.
- 빌드 통과만으로 판정하지 않는 근거: std의 `std::process`는 Android·iOS target에서도 컴파일된다(위 표의 ssh 행이 지금 통과한다).

## SSH 백엔드

`tasty-net-ssh`에 백엔드 둘을 둔다.

| 백엔드 | 하는 일 | feature |
|---|---|---|
| 시스템 ssh(`system`) | 지금의 동작. 설치된 OpenSSH `ssh`를 자식 프로세스로 띄워 `-L` 터널과 원격 포트 발견을 한다 | `system-ssh` |
| 내장(`embedded`) | 프로세스 안 SSH 클라이언트(russh). 같은 터널·포트 발견 계약을 지킨다 | `embedded-ssh` |

### feature 계약

`tasty-net-ssh`의 백엔드 feature와 `tasty-net-transport`의 ssh feature를 하나의 계약으로 정한다. [크레이트 구성](crates.md#의존-방향)의 "feature로 기능을 켜고 끄지 않는다"의 예외는 이 계약 하나다.

| 크레이트 | feature | 기본 | 켜는 것 |
|---|---|---|---|
| `tasty-net-ssh` | `system-ssh` | 켬 | 시스템 ssh 백엔드. `std::process`를 쓰는 모든 코드(`SpawnHook`·`SpawnFn`, `SshLauncher`, `resolve_ssh_path`, OpenSSH 발견, 자식 등록·회수, 발견 자식)를 이 feature 안의 `src/system/`에 둔다 |
| `tasty-net-ssh` | `embedded-ssh` | 켬 | 내장 백엔드. 의존 `russh`(정확한 판, `default-features = false`, `ring`), `tokio`(`rt-multi-thread`·`net`·`time`·`sync`·`io-util`. `process` 없음) |
| `tasty-net-ssh` | 둘 다 끔 | | `compile_error!`. 백엔드 없는 SSH 크레이트는 만들지 않는다 |
| `tasty-net-transport` | `ssh` | 켬(아래 두 feature를 통해) | `tasty-net-ssh` 의존(`optional = true`, `default-features = false`)과 `SshTransport`, SSH 연결 목록 항목 |
| `tasty-net-transport` | `system-ssh` | 켬 | `ssh` + `tasty-net-ssh/system-ssh` |
| `tasty-net-transport` | `embedded-ssh` | 켬 | `ssh` + `tasty-net-ssh/embedded-ssh` |

- `tasty-net-transport`의 `default = ["system-ssh", "embedded-ssh"]`. 데스크톱 소비자(tasty)는 기본 그대로 쓰고, OpenSSH가 없는 데스크톱에서도 폴백이 된다.
- 모바일 조합: `tasty-net-transport --no-default-features --features embedded-ssh`. SSH 없는 조합 `--no-default-features`도 빌드되어야 한다(메모리·P2P 전송만).
- `ssh`가 꺼진 transport에서 사라지는 공개 API는 `tasty-net-ssh`의 타입을 싣거나 SSH 연결을 여는 항목이다: `SshTransport`, `FORWARDED_SEPARATOR`, `LOCAL_CONNECT_TIMEOUT`, `AttemptCancel::ssh_cancel`, `TransportError::Discovery(PortDiscoveryError)`·`TransportError::Tunnel(TunnelError)`, `HandshakeError::RemoteListenerAbsent(RemoteListenerAbsent)`, 연결 목록 `Cause::Discovery(PortDiscoveryFailureKind)`. 지금의 사용 자리는 `crates/tasty-net-transport/src/cancel.rs:93`, `src/transport.rs:200`·`:202`, `src/reject.rs:226`, `src/connection_table/info.rs:153`, `src/handshake.rs:616`, `src/ssh.rs`다.
- 남는 API: `Transport`·`Listener`·`RawChannel`·`Link`, handshake v2, 메모리 전송, P2P 어댑터, `ConnectionTable`과 그 설정값, `Redialer`. `tasty-net-ssh` 타입을 싣지 않는 enum 변형은 조합과 관계없이 모두 남는다: 연결 목록 `PathKind`의 `ssh_forward`·`ssh_role_reversed`·`ssh_inbound`, `Cause::RemoteListenerAbsent`, 연결 목록 `via`의 `ssh{…}`(아래 `backend` 필드는 공통 타입), `EdgeTransport`의 SSH 값. 그래서 FFI와 직렬화에 보이는 모양이 feature 조합마다 같다. `ssh`가 꺼진 빌드에서 이 변형은 만들어지지 않을 뿐이고, 받는 쪽 `ssh_inbound`는 꺼진 빌드에서도 생긴다(상대가 SSH로 연 연결을 루프백 listener로 받는다).
- 백엔드와 무관한 공통 항목(두 feature 어느 쪽이든 있다): `SshTarget`, `SshSettings`와 설정값, `Backoff`, `SshTunnel`, `SshCancel`, `ListenerVerdict`·`RemoteListenerAbsent`, `PortDiscoveryError`·`PortDiscoveryFailureKind`, `TunnelError`, `connection_closed_error`, 백엔드 선택 타입(`SshBackendSetting`, `SelectedBackend`, `BackendReason`), 진입점 `SshConnector`.
- 진입점: `SshConnector::new(settings)`가 백엔드를 고르고 터널·포트 발견을 연다. `system-ssh`가 켜져 있으면 `SshConnector::with_launcher(SshLauncher)`로 프로그램·자식 생성 훅·`with_verify`를 준다. transport의 `SshTransport::new`는 `SshLauncher` 대신 `SshConnector`를 받는다. 지금의 `SshTransport::new(launcher, …)`는 `system-ssh` 안에서 `SshConnector::new(settings).with_launcher(launcher)`의 줄임으로 남긴다.

### 선택 규칙

설정값 `ssh.backend`가 정한다. 값은 `auto`(기본)·`system`·`embedded`다.

| `ssh.backend` | 고르는 백엔드 | 이유(`BackendReason`) |
|---|---|---|
| `auto` | OpenSSH 클라이언트를 찾으면 시스템, 못 찾으면 내장 | `found{path}` / `not_found{searched}` |
| `auto`, `system-ssh` feature가 꺼진 빌드 | 내장 | `system_not_built` |
| `system` | 시스템. 찾지 못하면 연결 시도가 `BackendUnavailable{backend: system, searched}`로 끝난다(내장으로 넘어가지 않는다) | `configured` |
| `embedded` | 내장 | `configured` |

- OS·플랫폼으로 나누지 않는다. 같은 규칙이 데스크톱과 모바일에 같다. 모바일은 `system-ssh` 없이 빌드하므로 `system_not_built`로 내장을 고른다. `system-ssh`가 꺼진 빌드에서 `ssh.backend = system`은 설정 단계에서 `SettingsError`(범위 밖)로 거절한다. `embedded-ssh`가 꺼진 빌드의 `embedded`도 같다.
- OpenSSH 발견: 다른 프로그램을 실행하지 않고 파일만 본다(`ssh -V`를 돌리지 않는다).
  1. 런처의 프로그램(`SshLauncher::program()`, 기본은 지금의 `resolve_ssh_path()`)이 경로 구분자를 가지면 그 경로 하나를 본다. Windows의 `%WINDIR%\System32\OpenSSH\ssh.exe` 우선은 `resolve_ssh_path`의 기존 규칙이고, 백엔드 선택이 아니라 어느 `ssh` 파일을 쓸지의 규칙이다.
  2. 구분자가 없는 이름(`ssh`, `ssh.exe`)이면 `PATH`의 디렉터리를 앞에서부터 보고 그 이름의 일반 파일을 처음 찾은 곳이 발견 경로다. Unix는 실행 권한 비트가 있어야 한다.
  3. 찾은 파일이 OpenSSH인지는 가리지 않는다. 같은 이름의 다른 클라이언트가 먼저 있으면 그것이 시스템 백엔드가 되고, 그 클라이언트가 옵션을 거절하면 시스템 백엔드의 오류로 끝난다. 알려진 한계로 둔다.
- 고르는 때: 연결 시도마다(터널을 새로 열거나 포트를 발견할 때) 다시 본다. 실행 중에 OpenSSH를 설치하거나 지운 것이 다음 시도에 반영된다. 이미 연 터널은 처음 고른 백엔드로 끝까지 간다.
- 시스템 백엔드가 실패해도 내장으로 넘어가지 않는다. 폴백은 "찾지 못함"일 때만이다. 인증 실패나 호스트 키 거절을 다른 구현으로 다시 시도하면 사용자가 정한 동작과 다른 결과가 날 수 있기 때문이다.

### 보이는 자리

| 자리 | 필드 | 값 |
|---|---|---|
| `tasty-net-ssh` | `SshTunnel::backend()`, 포트 발견 결과의 `backend` | `SelectedBackend{backend: system·embedded, reason, at}` |
| `tasty-net-ssh` 설정 조회 | `SshConnector::settings()`의 `ssh.backend` 행(`{key, value, source}`)과 `SshConnector::backend_selection()`(지금 판정하면 고를 백엔드와 이유, 판정 시각) | 위와 같다. 내장 백엔드에서 읽은 설정 파일의 지원하지 않는 지시어 목록도 함께 보인다(아래 "지원하지 않는 지시어의 표시") |
| transport 연결 목록 | SSH를 띄운 쪽 항목의 `via = ssh{target, remote_port, backend, backend_reason}` | 그 터널을 연 때의 `SelectedBackend`. `applied`에 그 연결에 적용된 SSH 설정값과 출처, 내장이면 해석한 호스트·사용자·포트·키 파일과 지원하지 않는 지시어 |
| transport 연결 목록 | 받는 쪽 `ssh_inbound` 항목 | 백엔드를 모른다(루프백으로 받은 TCP 연결이다). 비운다 |

- `BackendReason`의 문자열 값: `found`(경로 함께), `not_found`(본 경로 목록 함께), `system_not_built`, `configured`.

### 내장 백엔드의 경계

- 같은 계약: 내장 백엔드도 로컬 루프백 포워딩 종단(`127.0.0.1:<포트>`)을 연다. 그 포트로 온 TCP 연결마다 SSH `direct-tcpip` 채널을 원격 루프백 리스너로 연다. 그래서 transport의 터널 수명·공유 키·`segments.local_forward` 구간이 두 백엔드에 같다. 포트 발견은 SSH `exec` 채널로 같은 원격 명령을 돌리고 stdout·stderr·종료 코드를 지금의 분류 규칙(`discovery.*` 설정값 포함)으로 읽는다. 루프백 포트를 거치지 않는 프로세스 안 스트림은 이 계약 밖의 후속 최적화로 둔다.
- 리스너 판정: 시스템 백엔드는 ssh의 stderr 거절 줄을 세지만, 내장 백엔드는 `direct-tcpip` 채널 열기 실패(`SSH_MSG_CHANNEL_OPEN_FAILURE`의 사유 코드)를 그대로 받아 같은 `ListenerVerdict`로 낸다.
- 런타임: russh는 tokio 위에서 돈다. 내장 백엔드는 `tasty-net-p2p`와 같은 방식으로 자기 런타임을 소유하고 동기 API 뒤로 숨긴다([전송 추상화](transport.md#동기비동기-경계)).
- 연결 하나에 SSH 세션 하나다. 같은 공유 키의 채널(attach·bulk·요청)은 그 세션의 채널을 나눠 쓴다.
- 비밀 입력: 암호 걸린 키의 passphrase, 비밀번호·keyboard-interactive 응답, `StrictHostKeyChecking ask`의 확인은 소비자가 준 콜백(`SshPrompt`)으로 묻는다. 콜백이 없으면 그 방법은 실패로 보고 다음 방법으로 간다(호스트 키 확인은 거절). 비밀 값은 설정 조회와 연결 정보에 나오지 않는다.

### 내장 백엔드가 읽는 파일과 위치

OpenSSH와 같은 형식의 파일을 OpenSSH와 같은 뜻으로 읽는다. 위치 기본값은 OpenSSH와 같다. 홈 폴더에 ssh 파일이 없는 환경(모바일)에서는 소비자(tasty)가 경로나 덮어쓰기 값을 넣는다.

| 파일 | 기본 위치(OpenSSH와 같다) | 읽는 형식 |
|---|---|---|
| 사용자 설정 | `~/.ssh/config` | `ssh_config` 문법 전부(아래 지시어 표) |
| 시스템 설정 | Unix `/etc/ssh/ssh_config`, Windows `%PROGRAMDATA%\ssh\ssh_config` | 같다. 사용자 설정 다음에 읽는다(첫 값 우선) |
| 사용자 known_hosts | `~/.ssh/known_hosts`, `~/.ssh/known_hosts2` | 평문·와일드카드(`*`·`?`)·부정(`!`)·`[호스트]:포트`·해시(`|1|`) 호스트, `@cert-authority`, `@revoked` |
| 시스템 known_hosts | Unix `/etc/ssh/ssh_known_hosts`, `/etc/ssh/ssh_known_hosts2`(Windows는 `%PROGRAMDATA%\ssh\` 아래) | 같다 |
| 기본 키 파일 | `~/.ssh/id_rsa`, `id_ecdsa`, `id_ecdsa_sk`, `id_ed25519`, `id_ed25519_sk`(OpenSSH 10 기준 순서) | OpenSSH 개인키 형식, PEM(PKCS#1 `RSA PRIVATE KEY`, SEC1 `EC PRIVATE KEY`, PKCS#8 `PRIVATE KEY`·`ENCRYPTED PRIVATE KEY`), 암호 걸린 키. 같은 이름의 `-cert.pub` 인증서 |
| ssh-agent | `IdentityAgent`, 없으면 환경 변수 `SSH_AUTH_SOCK`. Windows는 OpenSSH 에이전트 named pipe(`\\.\pipe\openssh-ssh-agent`)와 Pageant | agent 프로토콜. Unix 도메인 소켓이 있는 플랫폼(Linux·macOS·Android·iOS)과 Windows named pipe·Pageant. 모바일에는 보통 에이전트가 없고, 소켓이 없으면 agent 인증을 건너뛰고 그 사실을 연결 정보에 남긴다 |

- `~`는 설정값 `embedded.home`(기본은 OS의 홈: `HOME`, Windows `USERPROFILE`, 출처 `os_default`)으로 펼친다. 모바일은 앱이 자기 폴더를 준다. 홈을 알 수 없고 설정도 없으면 `~`가 든 경로는 "없는 파일"로 다룬다(OpenSSH와 같이 기본 파일이 없으면 건너뛴다).

### 설정값

[크레이트 구성](crates.md#설정값-규칙)의 규칙(설정·조회, 출처, 코드 기본값 표 하나, 표와 코드 대조)을 따른다. 아래 표는 이 설계가 정한 항목이고, 구현 작업이 [SSH 크레이트 설정값](transport.md#ssh-크레이트-설정값) 표로 옮겨 기본값 대조 시험의 대상으로 만든다(그 전에는 이 표가 정본이다). 기존 `ssh.connect_timeout`·`ssh.server_alive_interval`·`ssh.server_alive_count_max`·`discovery.*`·`tunnel.*`·`backoff.*`는 두 백엔드가 함께 쓴다(내장은 `ServerAliveInterval` 대신 russh `keepalive_interval`·`keepalive_max`에 넣는다).

| 키 | 기본 | 범위 | 출처·근거 |
|---|---|---|---|
| `ssh.backend` | `auto` | `auto`·`system`·`embedded`(빌드에 없는 백엔드는 거절) | 사용자 결정. `default`·`configured` |
| `embedded.config_file` | 없음(OpenSSH 기본: 사용자 설정 + 시스템 설정) | 경로 하나 또는 `none` | `ssh -F`와 같다. 주면 그 파일만 읽고 시스템 설정은 읽지 않는다. `none`이면 설정 파일을 읽지 않는다 |
| `embedded.system_config_file` | OS의 OpenSSH 시스템 설정 경로 | 경로 또는 `none` | 출처 `os_default`. 모바일에는 없다 |
| `embedded.home` | OS 홈 | 경로 | 출처 `os_default`. `~` 펼침과 `%d` 토큰 |
| `embedded.local_user` | OS 사용자 이름(`USER`·`USERNAME`) | 문자열 | 출처 `os_default`. `%u`·`Match localuser`와 원격 사용자를 정하지 않았을 때의 기본 사용자(OpenSSH와 같다). 모바일처럼 값이 없고 설정도 없으면 원격 사용자가 정해지지 않은 연결은 `UserRequired`로 끝난다 |
| `ssh.overrides` | 빈 목록 | `지시어=값` 목록 | `ssh -o`와 같은 뜻의 덮어쓰기 값. 두 백엔드가 같이 쓴다: 시스템 백엔드에는 `-o`로 넘기고, 내장 백엔드는 설정 파일보다 먼저 적용한다. 그래서 첫 값 우선 규칙으로 설정 파일을 이긴다. 대상의 `extra_options`는 이 목록 뒤, 설정 파일 앞에 온다. 기존 설정값이 만드는 옵션(`ServerAliveInterval` 등)과의 순서는 [SSH 크레이트 설정값](transport.md#ssh-크레이트-설정값)의 규칙 그대로다 |
| `embedded.channel_open_timeout` | 10 s | 100 ms ~ 10 min | `direct-tcpip`·`exec` 채널 열기 응답 기한. `ssh.connect_timeout`과 같은 값으로 잡았다 |
| `embedded.prompt_timeout` | 5 min | 1 s ~ 1 h | `SshPrompt` 콜백(아래 "바인딩 층")의 답을 기다리는 시간. 사람이 passphrase를 치는 시간이다. 이 대기 동안 `ssh.connect_timeout`은 멈춘다 |
| `embedded.runtime.worker_threads` | 1 | 1 ~ 16 | 연결 몇 개의 SSH 세션에 워커 하나로 충분하다. 모바일 스레드 비용을 줄인다 |
| `embedded.window_size` | 2 MiB | 32 KiB ~ 64 MiB | russh `Config::window_size` 기본(2097152)을 그대로 쓴다 |
| `embedded.max_packet_size` | 32 KiB | 1 KiB ~ 256 KiB | russh `Config::maximum_packet_size` 기본(32768) |
| `embedded.channel_buffer` | 100 | 1 ~ 10 000 | russh `Config::channel_buffer_size` 기본 |
| `embedded.rekey.bytes` | 1 GiB | 1 MiB ~ 64 GiB | russh `Limits` 기본(읽기·쓰기 같은 값, RFC 4253 §9). OpenSSH `RekeyLimit` 지시어가 있으면 그 값이 이긴다 |
| `embedded.rekey.time` | 1 h | 1 min ~ 24 h | russh `Limits::rekey_time_limit` 기본 |
| `embedded.gex.min_bits`, `embedded.gex.preferred_bits`, `embedded.gex.max_bits` | 3072, 8192, 8192 | 2048 ~ 8192 | russh `GexParams` 기본 |

- russh 0.65.4 `client::Config`의 공개 필드 가운데 시간·횟수·크기·상한을 받는 것은 모두 위 표나 기존 `ssh.*`에 있다. `inactivity_timeout`은 쓰지 않는다(링크 heartbeat와 keepalive가 끊김을 본다). 구현 작업은 [P2P](p2p.md#의존성이-노출하는-값)와 같은 방식으로 고정판의 공개 setter 목록을 시험 자료로 두고 빠진 항목을 시험이 찾게 한다.
- 알고리즘 목록은 설정값으로 따로 두지 않고 OpenSSH 지시어(`KexAlgorithms`·`Ciphers`·`MACs`·`HostKeyAlgorithms`·`PubkeyAcceptedAlgorithms`)로 받는다. 기본은 russh 0.65.4의 기본 목록(아래 "russh 사실")이다.
- 호스트 키 확인을 건너뛰는 설정값은 두지 않는다.

### 지시어 지원 표

구분: 지원(OpenSSH와 같은 뜻), 부분 지원(무엇이 빠지는지 적는다), 미지원. 미지원 지시어의 동작은 둘 중 하나다.

- 표시: 읽고 연결은 계속한다. 그 지시어가 빠져도 고르는 호스트·사용자·포트·키·호스트 키 판정이 OpenSSH와 같거나 더 엄격해지는 경우다.
- 실패: 그 호스트의 연결을 `UnsupportedDirective{directive, file, line}`로 끝낸다. 빠지면 고르는 대상이 OpenSSH와 달라지거나 호스트 키 판정이 느슨해지는 경우다(안전한 쪽으로 실패).

"구현" 열은 그 부분을 누가 하는지다. russh는 프로토콜·키 형식·agent 클라이언트를 준다. 설정 파일 해석에 쓸 만한 크레이트(`russh-config` 0.65.3)는 아래 "russh 사실"의 이유로 쓰지 않으므로 해석은 모두 이 레포가 한다.

| 지시어·문법 | 구분 | 미지원 때 | 구현 |
|---|---|---|---|
| 줄 문법(`키 값`, `키=값`, 따옴표, 주석, 대소문자 무시) | 지원 | | 이 레포 |
| 첫 값 우선(같은 지시어는 처음 얻은 값, `IdentityFile`·`CertificateFile`·`LocalForward` 등 누적 지시어는 쌓음) | 지원 | | 이 레포 |
| `Host`(와일드카드 `*`·`?`, 부정 `!`, 여러 패턴) | 지원 | | 이 레포 |
| `Match host`·`originalhost`·`user`·`localuser`·`all`·`canonical`·`final`·`tagged`, 부정 `!` | 지원. `final`은 OpenSSH처럼 설정을 한 번 더 읽는다 | | 이 레포 |
| `Match exec` | 미지원 | 실패. 다른 프로그램을 실행해야 판정되므로 금지 범위에 걸린다 | |
| `Match localnetwork` | 미지원 | 실패. 블록이 적용되는지 판정할 수 없다 | |
| `Include`(glob, 상대 경로는 `~/.ssh` 또는 시스템 설정 폴더 기준, 중첩 상한 16) | 지원 | | 이 레포 |
| 토큰 `%%`·`%C`·`%d`·`%h`·`%j`·`%k`·`%L`·`%l`·`%n`·`%p`·`%r`·`%u`, 환경 변수 `${이름}`, `~` | 지원. OpenSSH가 지시어마다 허용하는 토큰 표를 따른다 | | 이 레포 |
| `%i`(로컬 uid) | 부분: Unix만. Windows·홈 없는 환경에서는 그 지시어가 실패 | 실패 | 이 레포 |
| `Hostname`, `User`, `Port`, `HostKeyAlias` | 지원 | | 이 레포 |
| `CanonicalizeHostname yes·always`와 `Canonical*` | 미지원 | 실패. 고르는 호스트가 달라진다. `no`(기본)는 지원 | |
| `IdentityFile`, `IdentitiesOnly`, `CertificateFile` | 지원 | | 파일 읽기·형식은 russh(`load_secret_key`, `load_openssh_certificate`), 고르기 순서는 이 레포 |
| `IdentityAgent`(`none`, 경로, `SSH_AUTH_SOCK`) | 지원 | | agent 클라이언트는 russh(`AgentClient`: Unix 소켓, Windows named pipe·Pageant), 경로 결정은 이 레포 |
| `AddKeysToAgent` | 미지원 | 표시. agent에 키를 넣지 않는다(인증 결과는 같다) | |
| `ForwardAgent` | 미지원 | 표시. 넘기지 않는다(더 엄격) | |
| `PreferredAuthentications`, `PubkeyAuthentication`, `PasswordAuthentication`, `KbdInteractiveAuthentication`, `NumberOfPasswordPrompts` | 지원 | | 방법은 russh(`authenticate_publickey`·`_with`·`_openssh_cert`·`_password`·`_keyboard_interactive_*`·`_none`), 순서는 이 레포 |
| `GSSAPIAuthentication yes` | 미지원 | 표시. 그 방법을 건너뛰고 다음 방법으로 간다. russh에 `authenticate_gssapi_with_mic`가 있지만 GSSAPI 구현을 주지 않는다 | |
| `PKCS11Provider`, `SecurityKeyProvider` | 미지원 | 표시. 그 키 소스를 건너뛴다(`*_sk` 키 파일은 agent가 있을 때만 쓴다) | |
| `BatchMode` | 지원. `yes`면 `SshPrompt`를 부르지 않는다(묻는 방법은 실패로 보고 다음으로) | | 이 레포 |
| `StrictHostKeyChecking yes`(기본 `ask`도 확인 콜백이 없으면 `yes`) | 지원 | | 이 레포 |
| `StrictHostKeyChecking accept-new` | 지원(모르는 호스트만 받아 기록, 바뀐 키는 거절) | | 기록 형식은 `HashKnownHosts`를 따른다. 이 레포 |
| `StrictHostKeyChecking no`·`off` | 미지원 | 실패. 호스트 키 확인을 건너뛰는 동작은 두지 않는다(사용자 결정). OpenSSH는 이 값에서 바뀐 키도 일부 받는다 | |
| `UserKnownHostsFile`, `GlobalKnownHostsFile`(여러 파일, `none`) | 지원 | | 이 레포 |
| known_hosts 와일드카드·부정·`[호스트]:포트` | 지원 | | 이 레포(russh `check_known_hosts_path`는 평문 일치와 해시만 본다) |
| known_hosts 해시(`|1|`) | 지원 | | 이 레포(같은 방식, HMAC-SHA1) |
| known_hosts `@cert-authority`(호스트 인증서) | 지원 | | 인증서 협상은 russh(`Preferred::host_key_certificates`, `Handler::check_server_key`가 인증서를 받는다), CA 서명·유효 기간·주체 이름 확인은 이 레포(검증 함수가 `ssh-key`에 있는지는 확인하지 않음) |
| known_hosts `@revoked` | 지원 | | 이 레포 |
| `RevokedHostKeys` | 부분: 공개키 목록 형식만. KRL 바이너리 파일이면 실패 | 실패 | 이 레포 |
| `CheckHostIP` | 지원(OpenSSH 8.5부터 기본 `no`) | | 이 레포 |
| `HashKnownHosts`, `UpdateHostKeys` | `HashKnownHosts` 지원. `UpdateHostKeys`는 미지원 | 표시. 새 키를 known_hosts에 더하지 않는다 | |
| `VerifyHostKeyDNS` | 미지원 | 표시. DNS SSHFP로 키를 받지 않는다(known_hosts 확인만 하므로 더 엄격) | |
| `KnownHostsCommand` | 미지원 | 실패. 다른 프로그램을 실행해야 하고, 빠지면 `accept-new`가 다른 키를 받을 수 있다 | |
| `HostKeyAlgorithms`, `KexAlgorithms`, `Ciphers`, `MACs`, `PubkeyAcceptedAlgorithms`, `CASignatureAlgorithms`(`+`·`-`·`^` 연산 포함) | 부분: russh가 아는 이름과의 교집합 | 교집합이 비면 실패. 모르는 이름만 빠졌으면 표시 | 목록 계산은 이 레포, 협상은 russh(`Preferred`) |
| `RekeyLimit` | 지원 | | russh `Limits` |
| `ProxyJump`(여러 단계, `none`) | 지원. 앞 단계 세션의 `direct-tcpip` 채널 위에 다음 SSH 세션을 연다. 단계마다 같은 지시어 규칙과 호스트 키 확인을 한다 | | 이 레포(russh `channel_open_direct_tcpip`와 스트림 위 세션 시작) |
| `ProxyCommand` | 미지원 | 실패. 다른 프로그램 실행이다 | |
| `ConnectTimeout`, `ServerAliveInterval`, `ServerAliveCountMax`, `ConnectionAttempts`, `TCPKeepAlive`, `AddressFamily` | 지원(앞의 셋은 기존 설정값과의 우선 규칙을 그대로 쓴다, [SSH 크레이트 설정값](transport.md#ssh-크레이트-설정값)) | | 이 레포 |
| `BindAddress` | 지원 | | 이 레포 |
| `BindInterface` | 미지원 | 실패. 나가는 경로가 달라진다 | |
| `Compression yes` | 미지원(russh `flate2` feature를 켜지 않는다) | 표시. 압축 없이 연결한다 | |
| `LocalForward`, `RemoteForward`, `DynamicForward`, `ExitOnForwardFailure`, `GatewayPorts`, `ClearAllForwardings` | 미지원 | 표시. 이 레포는 자기 터널 하나만 연다. 같은 설정에서 시스템 백엔드가 이 지시어의 포워딩을 함께 여는지는 확인하지 않음 | |
| `ControlMaster`, `ControlPath`, `ControlPersist` | 미지원 | 표시. 세션을 공유하지 않는다 | |
| `RemoteCommand`, `RequestTTY`, `SessionType`, `StdinNull`, `ForkAfterAuthentication` | 미지원 | 표시. 포트 발견은 자기 명령을 주므로 OpenSSH에서도 `RemoteCommand`가 쓰이지 않는다 | |
| `SendEnv`, `SetEnv` | 지원(`exec` 채널의 env 요청) | | russh |
| `ForwardX11*`, `Tunnel*`, `LogLevel`, `Ignore*`, `PermitLocalCommand`, `LocalCommand`, `EscapeChar`, `VisualHostKey`, `Protocol` | 미지원 | 표시. `PermitLocalCommand yes` + `LocalCommand`도 실행하지 않고 표시만 한다 | |
| 위에 없는 지시어, 모르는 지시어 | 미지원 | 표시. OpenSSH는 모르는 지시어에 오류를 낸다. 내장 백엔드는 OpenSSH보다 새 지시어를 모를 수 있어 표시로 둔다. 단 `IgnoreUnknown`에 든 이름은 표시하지 않는다 | |

### 지원하지 않는 지시어의 표시

- 내장 백엔드가 설정 파일을 해석할 때마다 `{directive, value, file, line, action}`(`action`은 `shown`·`failed`)을 모은다. 비밀 값이 들 수 있는 지시어는 값 대신 `<redacted>`를 싣는다.
- 보이는 곳: 그 연결의 연결 정보(`SshTunnel`과 transport 연결 목록 항목의 `applied`), 설정 조회(`SshConnector::backend_selection()`과 함께 마지막 해석 결과).
- `failed`인 지시어가 그 대상에 적용되면 연결은 네트워크에 나가기 전에 `UnsupportedDirective`로 끝난다. `ProxyJump` 단계의 설정도 같다.

### russh 사실

2026-11-27에 crates.io와 배포 소스(`russh` 0.65.4, `russh-config` 0.65.3)로 확인했다.

| 항목 | 값 |
|---|---|
| 최신 판 | 0.65.4(2026-11-21 게시, 같은 날 0.65.3). `rust-version = "1.89"`. workspace의 `rust-version`(1.89)과 같다. 이 레포는 이미 `tasty-net-demo`의 dev 의존으로 0.65.4를 쓴다(`Cargo.lock`) |
| 라이선스 | Apache-2.0(`deny.toml` 허용 목록에 있다). 암호 의존 `ring`은 quinn·rustls가 이미 쓴다 |
| crypto feature | 기본은 `aws-lc-rs`. `default-features = false, features = ["ring"]`로 바꾼다(`ring`도 `aws-lc-rs`도 없으면 `compile_error!`). P2P와 같은 provider라 C 빌드 조건이 하나다([P2P](p2p.md#외부-의존과-고정)). 기본 feature의 `flate2`(압축)·`rsa`는 빠진다. RSA 키 인증이 필요하므로 `rsa` feature는 켠다 |
| 기본 kex | `mlkem768x25519-sha256`, `curve25519-sha256`(@libssh.org 포함), `diffie-hellman-group-exchange-sha256`, group18·17·16·15-sha512, group14-sha256, strict kex 확장 |
| 기본 암호 | `chacha20-poly1305@openssh.com`, `aes256-gcm@openssh.com`, `aes256-ctr`, `aes192-ctr`, `aes128-ctr` |
| 기본 MAC | `hmac-sha2-512-etm`, `hmac-sha2-256-etm`, `hmac-sha2-512`, `hmac-sha2-256`(SHA-1 계열은 기본에 없다) |
| 기본 호스트 키 | ed25519, ecdsa P-256·P-384·P-521, rsa-sha2-512, rsa-sha2-256, ssh-rsa |
| 인증 | none, password, keyboard-interactive, publickey(키·`Signer`·agent), OpenSSH 사용자 인증서, GSSAPI 진입점(구현은 주지 않는다) |
| 키 파일 | `-----BEGIN OPENSSH PRIVATE KEY-----`, `RSA PRIVATE KEY`(암호 걸린 PKCS#5 포함), `ENCRYPTED PRIVATE KEY`, `PRIVATE KEY`, `EC PRIVATE KEY`(`src/keys/format/mod.rs`) |
| known_hosts | 평문 정확 일치와 해시 호스트만 본다. 와일드카드·부정·`@cert-authority`·`@revoked`는 없다(`src/keys/known_hosts.rs` `match_hostname`). 기본 경로를 `env::home_dir()`로 정한다. 그래서 내장 백엔드는 이 함수를 쓰지 않고 이 레포가 판정한다 |
| agent | `AgentClient::connect_uds`·`connect_env`(Unix), `connect_pageant`·`connect_named_pipe`(Windows) |
| 채널 | `channel_open_direct_tcpip`(터널), `exec`, keepalive(`keepalive_interval`·`keepalive_max`) |
| `russh-config` 0.65.3 | `Host`만 읽고 `Match`·`Include`가 없다. 줄을 첫 공백으로만 나눈다(`키=값`을 읽지 못한다). 모르는 지시어를 로그만 남기고 넘긴다. `StrictHostKeyChecking`을 `no`와 그 밖의 둘로만 읽는다. `ProxyCommand`를 `tokio::process`로 실행한다. 그래서 쓰지 않는다 |
| 모바일 target 빌드 | 확인하지 않음. `ring`이 Android에서 NDK clang을 요구하는 것은 rendezvous 행과 같다 |

## 앱 수명

모바일 OS는 앱이 백그라운드에 들어가면 곧 실행을 멈추고 소켓을 끊을 수 있고, Wi-Fi와 셀룰러가 바뀌면 로컬 주소가 바뀐다. 라이브러리는 OS 이벤트를 직접 듣지 않는다(플랫폼 API가 앱 쪽에 있다). 앱이 이벤트를 라이브러리에 알린다.

### 앱이 알리는 API

```text
enum AppEvent { Foreground, Background, NetworkChanged, NetworkLost }

P2pEndpoint::app_event(AppEvent)      // tasty-net-p2p
ConnectionTable::app_event(AppEvent)  // tasty-net-transport (SSH 터널, Redialer)
TastyNet::app_event(AppEvent)         // tasty-net-ffi: 위 둘에 차례로 넘긴다
```

- `AppEvent`는 `tasty-net-p2p`가 정의하고 transport가 다시 내보낸다(`P2pCloseReason`과 같은 방식, [크레이트 구성](crates.md#p2p-크레이트-격리)).
- 이벤트는 연결 목록과 P2P 상태 기계의 전이 원인(`app_background`, `app_foreground`, `network_changed`, `network_lost`)으로 기록된다.
- 데스크톱 소비자는 부르지 않아도 된다. 부르지 않으면 지금의 동작 그대로다.
- 이 API의 구현은 후속 작업이다. P2P 상태 기계에 아래 `Paused` 상태를 더하는 일과 [P2P](p2p.md#상태-기계) 표를 고치는 일도 그 작업이 한다.

### 동작

| 이벤트 | P2P endpoint(rendezvous 등록) | P2P 연결 | SSH 터널과 `Redialer` |
|---|---|---|---|
| `Background` | `lifecycle.background_registration = pause`(기본)면 `lifecycle.background_grace` 뒤 서버 연결을 코드 0으로 닫고 새 상태 `Paused`(원인 `app_background`)로 간다. 재연결 backoff도 멈춘다. `keep`이면 등록을 유지한다 | 그대로 둔다. OS가 실행을 멈추면 heartbeat·idle timeout으로 끊기고 원인이 남는다 | `lifecycle.background_redial = pause`(기본)면 대기 중인 재연결을 멈춘다. `home` 표시가 있는 대상은 `lifecycle.home_background`를 따른다 |
| `Foreground` | `Paused` → `Connecting`(원인 `app_foreground`). backoff 시도 번호를 1로 되돌린다 | 바뀌지 않는다 | 멈춘 재연결을 바로 한 번 시도하고 backoff를 처음 간격으로 되돌린다 |
| `NetworkChanged` | 등록 상태면 반사 주소 재확인과 NAT 재판별(흐름 3)을 바로 한다. 서버 연결이 `lifecycle.network_change_probe` 안에 답하지 않으면 backoff 없이 다시 연결한다 | `lifecycle.on_network_change = reconnect`(기본)면 이 기기의 연결을 원인 `network_changed`로 닫는다. transport가 새 펀칭으로 다시 맺는다. `keep`이면 [P2P](p2p.md#주소-바뀜-quic-경로-이동) 규칙에 맡긴다 | `reconnect`면 SSH 터널을 닫고 `Redialer`가 바로 다시 건다. `keep`이면 ssh keepalive가 끊김을 본다 |
| `NetworkLost` | 재연결 시도를 멈춘다(backoff를 쓰지 않는다). 다음 `NetworkChanged`·`Foreground`에서 다시 시작한다 | 그대로 둔다 | 재연결 대기를 멈추고 `give_up_after` 시계도 멈춘다 |

- p2p.md "주소 바뀜" 규칙과의 관계: 그 규칙은 OS 알림이 없을 때의 동작이다(A 쪽 재매핑은 QUIC 경로 이동으로 이어지고, B 쪽 재매핑은 idle timeout 뒤 새 펀칭). 모바일에서 망이 바뀌면 NAT 매핑도 바뀌고, 상대 NAT의 필터가 새 주소를 막으면 경로 이동도 실패한다. 그래서 OS가 알려 준 망 바뀜에는 idle timeout(기본 45 s)을 기다리지 않고 바로 다시 맺는 것을 기본으로 둔다. `keep`은 그 규칙을 그대로 쓰는 선택이다.
- 홈 서버 연결: 앱이 늘 붙어 있으려는 대상(보통 사용자의 데스크톱 tasty)을 연결 옵션 `ConnectOptions::home(true)`로 표시한다. `lifecycle.home_background = keep`이면 백그라운드에서도 그 대상의 재연결을 이어 가고 P2P 등록도 그 대상이 있는 동안 유지한다. 이것이 실제로 이어지는지는 OS가 앱 실행을 허락하는지(Android 포그라운드 서비스 등)에 달렸고, 그 허가는 앱의 몫이다. 기본 `pause`는 다른 대상과 같다.

### 수명 설정값

[크레이트 구성](crates.md#설정값-규칙)을 따른다. p2p 쪽 키는 `P2pSettings`, transport 쪽 키는 `ConnectionTableConfig`·`RedialerConfig`로 받고 같은 방식으로 조회된다. 구현 작업이 [P2P](p2p.md#설정값)·[전송 추상화](transport.md#보관-상한과-재연결-도우미) 표로 옮긴다(그 전에는 이 표가 정본이다).

| 키 | 크레이트 | 기본 | 범위 | 근거 |
|---|---|---|---|---|
| `lifecycle.background_registration` | p2p | `pause` | `pause`·`keep` | 백그라운드 앱은 곧 멈추므로 등록을 붙들어 둘 수 없다. 멈춘 기기의 등록은 서버 `idle_timeout`(기본 45 s)까지 남고, 그동안 이 기기로 온 펀칭은 서버 `offer_timeout` 뒤 `not_found`로 끝난다([rendezvous](rendezvous.md#서버-상태)). 코드 0(`closed`, 기기가 P2P를 끔)으로 먼저 닫으면 등록이 바로 지워지고 원인이 연결 정보에 남는다 |
| `lifecycle.background_grace` | p2p | 10 s | 0 s ~ 10 min | 앱 전환처럼 곧 돌아오는 경우에 등록을 다시 하지 않게 한다. OS가 백그라운드 앱에 실행을 허락하는 시간이 얼마인지는 확인하지 않음. 그보다 짧게 두어야 닫기가 실제로 나간다. 펀칭 실험(P1) 때 기기에서 다시 본다 |
| `lifecycle.on_network_change` | p2p, transport | `reconnect` | `reconnect`·`keep` | 위 "p2p.md 주소 바뀜 규칙과의 관계" |
| `lifecycle.network_change_probe` | p2p | 3 s | 100 ms ~ 1 min | 서버까지 한 번 왕복과 재전송 몇 번(`quic.initial_rtt` 333 ms 기준) |
| `lifecycle.background_redial` | transport | `pause` | `pause`·`keep` | 실행이 멈출 앱에서 backoff 시도를 쓰지 않고, 복귀 때 바로 시도한다 |
| `lifecycle.home_background` | transport | `pause` | `pause`·`keep` | OS 허가가 앱 몫이라 기본은 다른 대상과 같다 |

## 바인딩 층

새 크레이트 `tasty-net-ffi`(`crates/tasty-net-ffi`)가 Kotlin·Swift 바인딩의 경계다. UniFFI로 Kotlin과 Swift 코드를 생성한다. 크레이트를 만드는 일은 후속 작업이다.

- 의존: transport(`embedded-ssh`, `system-ssh` 없음), p2p, identity, topology. 다른 크레이트는 UniFFI에 의존하지 않는다. FFI용 레코드·enum은 이 크레이트가 따로 정의하고 코어 타입에서 옮긴다(코어 크레이트의 공개 API가 FFI 제약을 따르지 않게 한다).
- 크레이트 종류: `crate-type = ["lib", "cdylib", "staticlib"]`. Android는 `cdylib`(`.so`), iOS는 `staticlib`. `lib`는 바인딩 생성과 시험용이다.
- UniFFI 판: 2026-11-27 crates.io 최신은 0.33.0(2026-11-25 게시), 그 앞은 0.32.2(2026-09-23). 라이선스 MPL-2.0(`deny.toml` 허용). 구현 작업이 그때의 판을 정확히 고정한다. proc-macro 방식(`uniffi::setup_scaffolding!`, `#[uniffi::export]`)과 라이브러리 모드 생성(`uniffi-bindgen generate --library`)을 쓴다. 이 이름들은 그 판의 문서로 다시 확인한다.
- 동기 API: 코어 API가 동기이므로 FFI 함수도 동기로 내보낸다. 막히는 호출(연결, 읽기)은 앱이 UI 스레드가 아닌 스레드에서 부른다. 생성된 문서 주석에 적는다. UniFFI의 async 내보내기는 쓰지 않는다(코어에 async API가 생기면 다시 본다).
- 이벤트 전달: 끌어오기 방식이다. `EventStream::next(timeout) -> Option<Event>`(막히는 호출)를 앱이 자기 스레드에서 돌린다. 코어의 `events()` 큐와 `Lagged{dropped}` 규칙을 그대로 쓴다. 라이브러리 스레드에서 앱 코드를 부르는 콜백은 두지 않는다(앱 런타임 스레드 규칙과 재진입 문제를 피한다).
- 콜백이 필요한 한 곳: 연결 도중 사용자 답이 필요한 `SshPrompt`(passphrase, 비밀번호, keyboard-interactive, `StrictHostKeyChecking ask`)는 UniFFI의 외부 구현 trait(callback interface)로 받는다. 연결을 연 앱 스레드가 막혀 있는 동안 불리고, 앱은 UI 스레드로 넘겨 답을 받는다. 답을 기다리는 시간은 `embedded.prompt_timeout`이다([내장 백엔드 설정값](#설정값)).
- 오류 변환: 코어 오류 enum마다 같은 변형을 가진 FFI 오류 enum(`#[derive(uniffi::Error)]`)으로 옮긴다. 변형 이름이 앱의 i18n 키 분기에 쓰이고, 메시지는 코어의 영어 `Display`다([크레이트 구성](crates.md#오류-규칙)). 문자열 비교로 분기하는 일이 없게 한다. `is_retryable()`은 FFI 메서드로 함께 낸다.
- 이 레포가 갖는 것: `tasty-net-ffi` 크레이트, 바인딩 생성 스크립트 `scripts/gen-bindings.sh <kotlin|swift> <출력 폴더>`(호스트용 라이브러리를 빌드해 라이브러리 모드로 생성한다. 생성물은 커밋하지 않는다), 모바일 cross check와 생성 검사(아래).
- 갖지 않는 것: Android·iOS 앱, Gradle·Xcode 프로젝트, AAR·XCFramework 묶기, 앱 서명. Android 쪽은 tasty-android 레포가, iOS 앱은 지금 범위가 아니다(사용자 결정 "구조만").

## 모바일 cross check

CI 잡 둘을 더한다. 구현은 후속 작업이고, 자세한 잡 설정은 그때 [CI](../dev-guide/ci.md)와 [CI와 시험 환경](testing.md)에 적는다.

| 잡 | 러너 | target | 준비 |
|---|---|---|---|
| `mobile-android` | `ubuntu-latest` | `aarch64-linux-android`, `x86_64-linux-android` | `rustup target add`, NDK 설치(판 고정), `CC_<target>`·`AR_<target>`·`CARGO_TARGET_<TARGET>_LINKER`를 NDK의 API 29 clang으로 |
| `mobile-ios` | `macos-latest` | `aarch64-apple-ios`, `aarch64-apple-ios-sim` | `rustup target add` |

- 검사 단계 이름: `mobile-check`(`scripts/check.sh mobile-check <target>`). 로컬에 그 target이 없으면 단계는 실패하지 않고 "target 없음"을 한 줄로 알리고 건너뛴 target을 적는다. CI 잡은 건너뛰기를 실패로 본다(환경 변수로 구분).
- 명령(target마다, 모두 `--locked`):

  | 크레이트 | feature | 명령 |
  |---|---|---|
  | `tasty-net-frame`, `tasty-net-identity`, `tasty-net-topology` | 기본 | `cargo check --locked --target <t> -p <크레이트> --lib` |
  | `tasty-net-ssh` | 내장만 | `cargo check --locked --target <t> -p tasty-net-ssh --lib --no-default-features --features embedded-ssh` |
  | `tasty-net-transport` | SSH 없음 | `cargo check --locked --target <t> -p tasty-net-transport --lib --no-default-features` |
  | `tasty-net-transport` | 내장만 | `cargo check --locked --target <t> -p tasty-net-transport --lib --no-default-features --features embedded-ssh` |
  | `tasty-net-rendezvous` | 기본 | `cargo check --locked --target <t> -p tasty-net-rendezvous --lib` |
  | `tasty-net-p2p` | 기본 | `cargo check --locked --target <t> -p tasty-net-p2p --lib` |
  | `tasty-net-ffi` | 기본 | `cargo check --locked --target <t> -p tasty-net-ffi --lib` |

- 같은 잡이 정적 검사 `no-process-spawn`과, `tasty-net-ffi`가 생긴 뒤에는 `scripts/gen-bindings.sh`로 Kotlin(`mobile-android` 잡)·Swift(`mobile-ios` 잡) 생성이 성공하는지도 본다.
- 아직 없는 크레이트(p2p는 T3 반영 전, ffi는 후속 작업 전)는 생긴 커밋부터 목록에 든다. 목록에서 빼는 것은 이 문서가 모바일 대상이 아니라고 정한 것(`tasty-net-demo`, rendezvous 실행 파일)뿐이다.
- `cargo check`는 링크하지 않으므로 NDK 링커 설정이 맞는지는 보지 않는다. C 코드 컴파일(`ring`)만 NDK clang을 쓴다. 링크까지 보는 검사(`cargo build --target`)는 바인딩 생성과 앱 쪽 묶기에서 처음 필요해진다.

## 시험의 자리 (내장 백엔드)

구현 작업이 만든다. 정확한 조건은 시험 파일이 정본이다.

| 시험 | 자리 | 확인하는 것 |
|---|---|---|
| 내장 백엔드 attach | `crates/tasty-net-demo/tests/ssh_attach.rs`·`ssh_reverse_attach.rs`의 D2·D3 흐름을 내장 백엔드로 한 번 더(시험 이름 끝 `_embedded`) | OpenSSH 클라이언트를 `PATH`에서 뺀 조건(시험이 자식 환경의 `PATH`를 ssh 없는 폴더로 준다)과 `ssh.backend = auto`에서 내장으로 붙고, 같은 연결·끊기·재연결·거절 결과가 나온다. 서버 쪽은 지금과 같은 시험 sshd |
| 폴백 선택 | `tasty-net-ssh` `tests/backend_select.rs` | `PATH`에 가짜 `ssh` 파일이 있으면 `found{path}`, 없으면 `not_found{searched}`, 설정 `system`·`embedded`는 `configured`, `system`인데 못 찾으면 `BackendUnavailable`. 다른 프로그램을 실행하지 않는다(가짜 파일은 실행 권한만 있다). 연결 목록의 `via`와 설정 조회에 같은 값이 보인다 |
| `ssh -G` 대조 | `tasty-net-ssh` `tests/config_matches_openssh.rs` | 같은 설정 파일 묶음(Host·Match·Include·토큰·첫 값 우선을 섞은 시험 자료)으로 시스템 `ssh -G <대상>`이 내는 `hostname`·`user`·`port`·`identityfile`·`userknownhostsfile`·`proxyjump`와 내장 해석 결과가 같다. `ssh -G`는 시험이 OpenSSH가 있는 러너에서만 띄운다(없으면 실패가 아니라 미실행으로 남기고 로그에 그 사실을 찍는다) |
| 미지원 표시 | `tasty-net-ssh` 단위 시험 | 표시 지시어는 `{directive, file, line, action: shown}`으로 보이고 연결은 계속된다 |
| 안전한 실패 | `tasty-net-ssh` 단위 시험과 demo 시험 하나 | `StrictHostKeyChecking no`, `KnownHostsCommand`, `ProxyCommand`, `Match exec`, `CanonicalizeHostname yes`, KRL `RevokedHostKeys`가 네트워크에 나가기 전에 `UnsupportedDirective`로 끝난다 |
| 호스트 키 | `tasty-net-ssh` `tests/known_hosts.rs`, demo 시험 하나 | 바뀐 키 거절, `@revoked` 거절, `@cert-authority` 인증서 수락·서명 틀린 인증서 거절, 해시·와일드카드·`[호스트]:포트` 일치, `accept-new`의 기록 |

## D 기준과의 관계

[동작 수준 기준](done-criteria.md)은 이 문서가 고치지 않는다. D11(모바일 target 빌드 검사)과 내장 백엔드 attach 시험을 기준에 넣을지는 판단을 따로 올렸다(director가 정한다). 정해지면 그 문서와 이 절을 같은 커밋에서 고친다.

## 후속 작업과 미정

| 항목 | 상태 |
|---|---|
| transport `ssh` feature 분리(`system-ssh`·`embedded-ssh`) | 후속 작업. 이 문서 "feature 계약" |
| `tasty-net-ssh` 내장 백엔드·선택 규칙·지시어 해석 | 후속 작업. 이 문서 "SSH 백엔드" |
| 모바일 cross check CI와 `no-process-spawn` | 후속 작업. 이 문서 "모바일 cross check" |
| 앱 수명 API(`AppEvent`)와 P2P `Paused` 상태 | 후속 작업. 이 문서 "앱 수명" |
| `tasty-net-ffi`와 바인딩 생성 | 후속 작업. 이 문서 "바인딩 층" |
| `File::lock`이 Android·iOS에서 잠그는지 | 확인하지 않음. 기기나 에뮬레이터에서 identity·p2p 잠금 시험을 돌려야 안다. 안 잠그면 대안(잠금 대신 원자적 생성과 같은 프로세스 안 잠금)을 그때 정한다 |
| 의존(tokio·quinn·ring·russh)의 최소 iOS 배포 대상 | 확인하지 않음 |
| `x86_64-linux-android`·iOS 두 target의 빌드 | 확인하지 않음(원격·작업 머신에 target 없음) |
| tasty 설정 화면의 SSH 항목 | tasty 전환 때. 이 레포 범위 밖 |
