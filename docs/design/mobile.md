# 모바일 (Android·iOS)

이 레포의 크레이트가 Android와 iOS에서 빌드되고 동작하는 데 필요한 경계를 정한다. 다루는 것은 대상 target, 크레이트마다의 판정, 다른 프로그램 실행 금지 범위, 모바일에서 쓰는 SSH 조합, 앱 수명, 바인딩 층, 모바일 cross check다. SSH 백엔드 자체(선택 규칙, 내장 백엔드, OpenSSH 해석 이식, exec·SFTP)는 [SSH 백엔드](ssh-backends.md)가 정본이다. 크레이트 이름과 의존 방향의 정본은 [크레이트 구성](crates.md)이다.

## 사용자 결정

| 날짜 | 원문 또는 결정 | 이 문서의 결과 |
|---|---|---|
| 2026-11-27 | "tasty 전환과 함께, 안드로이드에도 구현이 같이 들어가야해 안드로이드나 ios 등에서도 동작 가능한 형태로 만들 수 있는 형태의 베이스가 만들어지는거 맞나?" | 이 문서 전체 |
| 2026-11-27 | iOS 범위 "구조만 iOS 가능하게" | iOS는 빌드 검사와 Swift 바인딩 생성 검사까지. iOS 앱은 만들지 않는다 |
| 2026-11-27 | SSH 폴백("설치된 openssh가 없다면 자체 ssh 모듈로"), 설정값으로 받기, OpenSSH와 같은 방식으로 파일 읽기, 추가 결정 4(russh + OpenSSH 해석 이식) | [SSH 백엔드](ssh-backends.md#사용자-결정). 모바일은 `system-ssh` 없이 빌드하므로 내장 백엔드로 간다 |
| 2026-11-27 | director 2852: tasty-android는 원격 파일을 SFTP로, Git 정보를 원격 git 명령(exec 채널)으로 가져오고, UniFFI로 Rust를 공유한다 | [바인딩 층](#바인딩-층), [SSH 백엔드의 터널·exec·SFTP](ssh-backends.md#터널execsftp) |
| 2026-11-27 | director 2856: 바인딩 크레이트는 tasty-network 안 `tasty-net-ffi`, Kotlin·Swift 생성은 앱 레포에서 한다(추천) | [바인딩 층](#바인딩-층)의 위치와 경계 |

## 대상 target

| target | 종류 | 최소 OS·툴체인 | 근거·확인 상태 |
|---|---|---|---|
| `aarch64-linux-android` | 실제 기기 | NDK API 수준 29 | tasty-android 앱의 `minSdk = 29`(`tasty-android` 레포 `app/build.gradle.kts`)에 맞춘다. 앱보다 낮은 수준으로 빌드할 까닭이 없다. NDK 판은 CI 작업이 고정하고 [CI](../dev-guide/ci.md)에 근거를 적는다. director 확인은 NDK 30.0.16248370으로 했다(아래 판정 표) |
| `x86_64-linux-android` | 에뮬레이터 | NDK API 수준 29 | 위와 같다 |
| `aarch64-apple-ios` | 실제 기기 | iOS 배포 대상: rustc 1.96.0 기본값 10.0을 그대로 둔다 | `rustc --print deployment-target --target aarch64-apple-ios` = `IPHONEOS_DEPLOYMENT_TARGET=10.0`(2026-11-27 확인). 의존(tokio·quinn·ring·russh)이 그보다 높은 판을 요구하는지는 확인하지 않음. iOS 앱을 만들지 않으므로 앱의 배포 대상은 없다 |
| `aarch64-apple-ios-sim` | 시뮬레이터 | rustc 기본값 14.0 | 같은 명령으로 `IPHONEOS_DEPLOYMENT_TARGET=14.0`(2026-11-27 확인) |

- 툴체인은 레포의 `rust-toolchain.toml`(1.96.0)이다. 네 target 모두 그 판의 std를 `rustup target add`로 받는다.
- Android target은 C 코드를 빌드하는 의존(`ring`)이 있어 NDK가 필요하다. 환경 변수는 NDK의 `toolchains/llvm/prebuilt/<호스트>/bin/` 아래를 가리킨다.

  | 변수 | 값 |
  |---|---|
  | `CC_<target>` | `<target><API 수준>-clang`(예: `aarch64-linux-android29-clang`. Windows 호스트는 `.cmd`) |
  | `AR_<target>` | `llvm-ar`(아카이브를 만드는 도구다. clang이 아니다) |
  | `CARGO_TARGET_<TARGET>_LINKER` | `CC_<target>`과 같은 clang |

- iOS target은 macOS의 Xcode 툴체인이 필요하다.
- 32비트 Android(`armv7-linux-androideabi`, `i686-linux-android`)는 대상이 아니다. 요청의 네 target에 없다.

## 크레이트별 판정

develop b103e90을 `aarch64-linux-android`로 두 번 확인했다.

- 원격 빌드 환경(rbuild 칸 2, 로그 `mob-design-targets`): 모바일 target은 `aarch64-linux-android` 하나만 설치돼 있었고 NDK가 없었다. 설치된 target은 `aarch64-linux-android`, `x86_64-pc-windows-gnu`, `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`다.
- director 확인(2026-11-27, 보조 머신): NDK 30.0.16248370, `CC_aarch64_linux_android=<NDK>/toolchains/llvm/prebuilt/<호스트>/bin/aarch64-linux-android26-clang`. 여섯 크레이트(frame·identity·rendezvous·ssh·topology·transport)가 모두 성공했고 error 0이다. demo는 뺐고 p2p는 develop에 없다.

나머지 세 target은 "확인하지 않음"이다. 설치는 하지 않았다.

| 크레이트 | 모바일 대상 | `aarch64-linux-android` `cargo check` (b103e90) | 막거나 주의할 것 (근거) |
|---|---|---|---|
| `tasty-net-frame` | 대상 | 통과(원격·director) | 없음. 외부 의존 없이 std만 쓴다 |
| `tasty-net-identity` | 대상 | 통과(원격·director) | 파일 잠금 `File::lock`(`crates/tasty-net-identity/src/secret_file.rs:281`), `O_NOFOLLOW`·`geteuid`(`src/secret_file/unix.rs:123`, `:187`). Android·iOS 모두 `cfg(unix)` 경로를 탄다. `File::lock`이 두 OS에서 실제로 잠그는지(`Unsupported`를 돌려주지 않는지)는 확인하지 않음. 키 디렉터리는 소비자가 준다([기기 키·토큰](identity.md#저장-위치)). 앱은 자기 전용 폴더(Android `filesDir`, iOS Application Support)를 준다. `std::process::id()`(`src/secret_file.rs:172`)는 임시 파일 이름용이고 다른 프로그램을 실행하지 않는다 |
| `tasty-net-topology` | 대상 | 통과(원격·director) | 없음 |
| `tasty-net-ssh` | 대상(내장 백엔드 작업 뒤 `--no-default-features --features embedded-ssh`만) | 통과(원격·director). 시스템 백엔드 소스가 컴파일된 결과다 | 시스템 `ssh` 실행 코드: `src/spawn.rs:7`(`std::process::{Child, Command}`)·`:14`(`SpawnFn`)·`:158`(`Command::new`)·`:172`(`std::os::windows::process::CommandExt`), `src/discovery.rs:9`, `src/tunnel.rs:7`·`:110`(`std::process::Command`을 돌려주는 함수), `src/cancel.rs:5`(`std::process::Child`). `src/lib.rs:438`–`:441`(`#[cfg(all(test, unix))]`)과 `:459`–`:766`(`#[cfg(test)] mod test_helper`)은 시험 코드다. std에 `std::process`가 있으므로 모바일 target에서도 컴파일된다. 그래서 빌드 통과는 금지 범위를 지킨다는 증거가 아니다(아래 "다른 프로그램 실행 금지"). 이 코드는 `system-ssh` feature의 `src/system/`으로 옮긴다 |
| `tasty-net-ssh-config` | 대상 | 아직 없음 | 다른 프로그램을 실행하지 않는다([SSH 백엔드](ssh-backends.md#openssh-해석-이식-크레이트)) |
| `tasty-net-transport` | 대상(SSH 없음, 또는 `embedded-ssh`만) | 통과(원격·director) | 라이브러리 `src/`에 `std::process` 없음. 통합 시험 `tests/ssh.rs`에는 있다. 소켓 깨우기 `shutdown(Both)`(`src/wake.rs:15`)는 Unix 경로를 탄다. 지금은 `tasty-net-ssh`를 무조건 의존한다(`Cargo.toml`) |
| `tasty-net-rendezvous` 라이브러리 | 대상(p2p가 쓰는 형식·코덱·TLS 설정). 서버 본체도 같은 라이브러리라 함께 컴파일된다 | 통과(director, `ring` 0.17.14가 NDK 30 clang으로 컴파일). 원격은 NDK가 없어 `ring` 빌드 스크립트가 `aarch64-linux-android-clang`을 찾지 못했다(`ToolNotFound`). 원격 실패는 NDK가 없어서였다 | `ring`(C, `quinn`·`rustls` 경유). tokio `signal` feature(`Cargo.toml`)는 실행 파일용이다. `std::process`는 실행 파일 `src/bin/tasty-rendezvous.rs`에만 있다(라이브러리 쪽 0건). 보내기 오류 번호는 `cfg(unix)`의 `EMSGSIZE`(`src/packet_io.rs:244`) |
| `tasty-net-rendezvous` 실행 파일 `tasty-rendezvous` | 대상 아님 | 해당 없음 | 서버다. 모바일 기기에서 띄우지 않는다. 모바일 cross check는 `--lib`로 본다 |
| `tasty-net-p2p` | 대상 | 확인하지 않음(develop에 없다. 코드 근거는 T3 lane 커밋 c11f671) | `ring`(위와 같다). 키 디렉터리의 잠금 파일 `p2p.lock`을 `try_lock`으로 잡는다(`crates/tasty-net-p2p/src/endpoint.rs:494`–`:498`@c11f671). identity와 같은 `File::lock` 확인이 필요하다. `socket2`로 `IPV6_V6ONLY` 끄기·버퍼 크기(`src/socket.rs:122`–`:130`@c11f671). 소유 런타임은 멀티스레드 tokio(`src/runtime.rs:113`@c11f671, 워커 수는 설정값). 라이브러리 `src/`에 `std::process` 0건. 예제의 `std::process`는 `ExitCode`·`exit`(`examples/p2p_peer.rs:22`·`:239`, `examples/punch_probe.rs:26`)이고 시험 `tests/lifecycle.rs:1123`은 `id()`라 금지 대상이 아니다. `Command`·`Child`는 시험 `tests/two_process.rs:7`·`:32`에만 있다. 예제와 시험은 금지 범위 밖이다 |
| `tasty-net-demo` | 대상 아님 | 해당 없음 | 예제 바이너리와 두 프로세스 시험, sshd를 띄운다 |
| `tasty-net-ffi`(새 크레이트, 아래 "바인딩 층") | 대상 | 아직 없음 | 위 대상 크레이트만 의존한다 |

- director 확인은 API 수준 26 clang으로 했다. 이 문서의 대상 수준 29로 C 코드를 컴파일한 확인은 모바일 cross check가 처음 한다.
- 판정의 정본은 모바일 cross check(아래)다. 이 표는 그 단계가 생기기 전의 확인 기록이고, 그 단계가 처음 성공하면 "확인하지 않음" 칸을 그 실행으로 고친다.

## 다른 프로그램 실행 금지

모바일 target에서 컴파일되는 라이브러리 코드는 다른 프로그램을 실행하지 않는다. iOS는 앱이 자식 프로세스를 만들 수 없고, Android도 앱 안에 `ssh` 같은 실행 파일이 없다.

- 금지 대상: `std::process`의 `Command`·`Child`·`Stdio`·`ChildStdin`·`ChildStdout`·`ChildStderr`·`ExitStatus`, `std::os::<os>::process::CommandExt`, `tokio::process`, 그리고 이것들을 쓰는 의존(예: `russh-config`의 `ProxyCommand` 실행). `std::process::id()`·`exit()`·`abort()`는 다른 프로그램을 실행하지 않으므로 해당하지 않는다.
- 경계:

  | 자리 | 다른 프로그램 실행 |
  |---|---|
  | `tasty-net-ssh`의 `src/system/`(`system-ssh` feature) | 허용. 모바일 빌드는 이 feature를 켜지 않는다 |
  | 금지 크레이트의 라이브러리 target 코드: frame, identity, topology, ssh(`src/system/` 밖), ssh-config, transport, p2p, rendezvous 라이브러리, ffi | 금지 |
  | 실행 파일(`src/bin/`, `src/main.rs`), `tests/`, `examples/`, `benches/`, 빌드 스크립트(`build.rs`), `tasty-net-demo` 전체, 라이브러리 안의 시험 항목(아래) | 허용. 모바일 기기에서 컴파일되지 않거나 기기 밖(호스트)에서 돈다 |

### 정적 검사 `no-process-spawn`

`scripts/check.sh no-process-spawn`(모바일 cross check 작업이 만든다).

1. 보는 파일: 금지 크레이트의 `src/**/*.rs`에서 `src/bin/**`와 `src/main.rs`를 뺀 것.
2. `tasty-net-ssh`의 `src/system/**`는 빼고 본다. 단 `src/lib.rs`에 `#[cfg(feature = "system-ssh")]`가 붙은 `mod system;` 선언이 있어야 한다. 없으면 이 단계가 실패한다(폴더 예외가 feature로 보호되는지 확인한다).
3. 시험 항목을 지운 뒤 본다.
   - `#[cfg(test)]`와 `#[cfg(all(test, …))]`가 붙은 항목(그 뒤의 `mod`·`fn`·`use`·`impl` 블록, 중괄호 짝으로 끝을 찾는다)을 지운다.
   - 그런 속성이 붙은 파일 모듈 선언(`#[cfg(test)] mod tests;`)이 가리키는 파일(`<이름>.rs`, `<이름>/mod.rs`, 그 아래 폴더)은 보지 않는다. 지금 해당하는 것은 identity의 `src/device_key/tests.rs`(`device_key.rs:319`–`:321`이 선언)와 `src/record/tests.rs`(`record.rs:467`–`:469`)다.
   - `#[cfg(any(test, …))]`는 지우지 않는다. 시험 밖에서도 컴파일되기 때문이다.
   - 기존 `no-hidden-constants`의 `strip_cfg_test`(`scripts/check-no-hidden-constants.sh:245`)는 정확히 `#[cfg(test)]`만 지운다. 이 단계는 `all(test, …)`까지 지우므로 같은 함수를 넓혀 함께 쓴다.
4. 남은 코드의 `use` 선언을 나무로 펼친다. 묶음(`use std::{io, process::{Command, id}};`), 중첩 중괄호, 별칭(`as`), `self`(`use std::process::{self};`), 여러 줄에 걸친 선언, 앞의 `::`를 모두 풀어 `가져온 경로 → 지역 이름` 목록을 만든다.
   - 모듈 자체를 가져오는 것은 별칭과 관계없이 실패다: `std::process`(예: `use std::process;`, `use std::process as proc;`, `use std::{process as proc};`, `use std::process::{self as p};`)와 `tokio::process`(같은 모양들). 모듈을 지역 이름으로 들이면 그 뒤 `proc::Command`를 이름만으로는 가를 수 없기 때문이다. `std::process` 아래에서 가져와도 되는 것은 이름을 적은 `id`·`exit`·`abort`·`ExitCode`뿐이다. 글롭(`use std::process::*;`)도 실패다.
   - 금지 모듈의 윗 경로를 지역 이름으로 들이는 것도 별칭과 관계없이 실패다: 크레이트 루트 `std`·`::std`·`tokio`·`::tokio`(`use std as s;`, `use ::std as s;`, `use std::{self as s};`, `use tokio as t;`), `std::os`와 `std::os::<os>`(`use std::os::unix as u;`, `use std::os::{self as o};`). edition 2018부터는 `extern crate` 없이 `use`만으로 크레이트에 별칭을 줄 수 있어서, 이것을 두면 `s::process::Command`·`u::process::CommandExt`를 경로 문자열로 찾지 못하기 때문이다. 규칙을 한 줄로 쓰면: 펼친 `use` 항목의 가져온 경로가 금지 경로(`std::process`, `tokio::process`, `std::os::<os>::process`)와 같거나 그 앞부분이면, 그리고 이름을 적은 `id`·`exit`·`abort`·`ExitCode`가 아니면 실패다. 별칭 없이 `use std::os::unix;`처럼 윗 경로를 그대로 들이는 것도 같다. `std::os::<os>::prelude`에서의 글롭도 실패다.
   - 금지 경로의 윗 경로(크레이트 루트 포함)에서의 글롭도 실패다: `use std::*;`, `use ::std::*;`, `use std::os::*;`, `use std::os::unix::*;`, `use std::{os::*};`, `use tokio::*;`. 글롭은 그 경로 아래의 공개 모듈을 지역 이름으로 들이므로(`use std::*;` 뒤의 `process::Command`, `use std::os::*;` 뒤의 `unix::process::CommandExt`) 경로 문자열로 찾지 못한다. 펼친 항목이 `<경로>::*`이면 `<경로>`를 가져온 경로로 보고 위 한 줄 규칙을 적용한다(`<경로>`가 금지 경로와 같거나 그 앞부분이면 실패).
   - 금지 이름을 직접 가져오는 것은 별칭과 관계없이 실패다(`use std::process::Command as Cmd;`, `use std::os::unix::process::CommandExt as _;`).
   - `use` 없이 적은 경로도 찾는다: `std::process::`·`::std::process::`·`tokio::process::` 뒤의 금지 이름, `std::os::<os>::process::CommandExt`. `extern crate std as s;`·`extern crate tokio as t;`처럼 크레이트에 별칭을 주는 선언은 실패다(별칭 경로를 다시 찾지 않아도 되게).
   - 하나라도 있으면 파일:줄과 펼친 경로를 찍고 실패한다. 펼치기는 `rustc`를 부르지 않는 스크립트 안의 작은 파서다. 문자열·주석 안의 글자는 먼저 지운다.
5. 검사 자체의 시험을 둔다(`no-hidden-constants`와 같은 방식). 실패해야 하는 입력: `std::process::Command::new`, `use std::process::Command;`, `use std::process as p;`, `use std::{process as proc};`, `use std::{io, process::{self, id}};`, `use tokio::{process as p};`, `use tokio::process::Command as C;`, `use std::process::*;`, `extern crate std as s;`, `use std as s;`, `use ::std as s;`, `use std::{self as s};`, `use tokio as t;`, `use std::os::unix as u;`, `use std::os::unix;`, `use std::os::unix::prelude::*;`, `use std::*;`, `use ::std::*;`, `use std::os::*;`, `use std::os::unix::*;`, `use std::{os::*};`, `use tokio::*;`, 여러 줄로 나눈 묶음 선언, `mod system;`의 feature 속성을 뺀 입력. 통과해야 하는 입력: `use std::process::{id, exit};`, `use std::os::unix::fs::PermissionsExt;`, `use std::io;`, `std::process::abort()`, `cfg(test)`·`cfg(all(test, unix))` 안에만 둔 금지 경로, `src/system/` 안에만 둔 금지 경로, 주석·문자열 안의 `std::process::Command`.

### 의존 쪽 검사

모바일 cross check의 각 조합 그대로 feature가 보이는 `cargo tree`로 본다.

```text
cargo tree --locked --target <t> -p <크레이트> <그 조합의 feature 인자> -e normal -f '{p} {f}' --prefix none
```

- `-f '{p} {f}'`가 패키지마다 켜진 feature 목록을 찍는다. `-e normal`은 일반 의존만 따른다(빌드 의존은 호스트에서 돌므로 빼고, dev 의존은 시험용이라 뺀다).
- 실패 조건: `tokio` 줄의 feature에 `process`가 있다, `russh-config` 줄이 있다, `tasty-net-ssh` 줄의 feature에 `system-ssh`가 있다.
- russh 0.65.4의 일반 의존 tokio feature는 `io-util`·`sync`·`time`과 비 wasm `io-util`·`rt-multi-thread`·`time`·`net`이고 `process`는 dev 의존에만 있다. russh-sftp 4.0.0은 `io-util`·`rt`·`sync`·`time`·`macros`다. 그래서 지금 판에서는 이 검사가 통과해야 한다.
- 빌드 성공만으로 판정하지 않는 근거: std의 `std::process`는 Android·iOS target에서도 컴파일된다(위 표의 ssh 행이 지금 통과한다).
- 앱의 최종 의존 그래프(android 요구 #15): 같은 기준을 앱 작업 공간에서도 돌릴 수 있게 이 절을 공개 기준으로 둔다. 대상 target은 `aarch64-linux-android`·`x86_64-linux-android`이고, 명령은 앱 작업 공간에서 앱 cdylib 크레이트를 `-p`로 준 같은 `cargo tree`다(앱의 배포 빌드 feature 인자를 붙인다). 실패 조건 가운데 `russh-config` 줄과 `tasty-net-ssh`의 `system-ssh`는 그대로다. `tokio`의 `process`는 앱 자신이 켤 수 있으므로(다른 프로그램 실행 금지는 앱 코드에 적용되지 않는다, 아래 "바인딩 층") 그 줄만으로 실패로 보지 않는다. 같은 앱 패키지·target·배포 feature 인자·일반 의존 범위로 거꾸로 본 `cargo tree --locked --target <t> -p <앱 cdylib 크레이트> <배포 feature 인자> -e normal,features -i tokio`(feature를 켠 쪽을 거꾸로 보인다)에서 `process`를 켠 경로에 `tasty-net-*` 크레이트가 있을 때만 실패다. `-e features`만 주면 Cargo가 빌드·dev 의존도 함께 보이므로 `normal`을 같이 준다([cargo tree](https://doc.rust-lang.org/cargo/commands/cargo-tree.html) `--edges`).
- 이 검사를 하는 스크립트는 모바일 cross check 작업이 만들고, 앱 작업 공간의 `Cargo.toml` 경로와 크레이트 이름을 인자로 받아 이 레포 밖에서도 같은 조건으로 돈다. 인자 모양은 그 작업이 정한다.

## 모바일의 SSH

- 모바일 조합은 `tasty-net-transport --no-default-features --features embedded-ssh`다. `ssh.backend`의 기본 `auto`가 `system_not_built`로 내장 백엔드를 고른다([선택 규칙](ssh-backends.md#선택-규칙)).
- 사용자의 SSH 파일은 앱 저장소의 `ssh/` 폴더에 둔다. 바인딩 층이 그 경로를 `embedded.ssh_dir`로 넣는다(tasty 설정으로 덮어쓸 수 있다, [내장 백엔드가 읽는 파일](ssh-backends.md#내장-백엔드가-읽는-파일)).
- 모바일에는 보통 ssh-agent가 없다. 키 passphrase·비밀번호·호스트 키 확인은 앱이 구현한 `SshPrompt`가 묻는다(아래 "바인딩 층").
- exec와 SFTP는 내장 백엔드의 같은 세션 위 채널이다([터널·exec·SFTP](ssh-backends.md#터널execsftp)).

### 직접 LAN 연결 (Android 지역망 권한)

Android 17(API 37)부터 SDK 37 이상을 대상으로 하는 앱은 지역망 주소로 TCP 연결을 맺을 때 런타임 권한 `ACCESS_LOCAL_NETWORK`이 필요하다. 제한은 네트워크 스택에서 걸리므로 Rust 소켓에도 적용되고, `.local` 이름 풀이에도 권한이 필요하다. 권한이 없으면 TCP는 보통 시간 초과로 끝난다([Android 지역망 권한](https://developer.android.com/privacy-and-security/local-network-permission), 2026-11-27 확인). android 요구 #14를 받는 자리다.

- 경계: 권한 확인·요청·설정 화면 안내는 앱(Kotlin) 몫이다. 이 레포는 판정 결과를 받는 콜백과 두 오류 갈래만 둔다(아래 "경계" 표).
- 판정: 내장 백엔드가 첫 SSH 홉(`ProxyJump`가 있으면 첫 점프 호스트)의 소켓을 열기 전에 한다. 뒤의 홉은 첫 홉의 SSH 안으로 가므로 기기의 소켓이 닿는 곳은 첫 홉뿐이다. 유효 이름이 `.local`로 끝나면 이름 풀이 전에, 아니면 풀이한 주소가 지역망 주소일 때 앱의 지역망 판정 콜백을 부른다. 콜백은 그 홉의 이름과 (있으면) 주소를 받고 허용·거절을 돌려준다. 지역망 주소 범위는 후속 작업이 정한다.
- (a) 콜백이 거절하면 소켓을 열지 않고 `LocalNetworkDenied`로 끝난다. 재시도하지 않는다.
- (b) 허용했는데 소켓 연결이 실패하면, 그 소켓을 가진 내장 백엔드가 닫기 전에 NDK `android_getnetworkblockedreason(sockfd)`(`android/multinetwork.h`, 그 소켓에 기록된 차단 사유를 돌려주고 지운다)를 부른다. `ANDROID_NETWORK_BLOCKED_REASON_LNP`이면 `LocalNetworkBlocked`로, 아니면 원래 연결 오류로 끝난다([NDK 네트워킹 참조](https://developer.android.com/ndk/reference/group/networking)). FFI 오류로 바뀐 뒤에는 그 FD가 없으므로 앱이 나중에 읽을 수 없다.
- API 29 빌드를 유지하는 호출 경계: NDK 참조 문서에는 이 함수가 어느 API 레벨부터 있는지 적혀 있지 않다(같은 쪽의 다른 함수에는 적혀 있다, 2026-11-27 확인). 그래서 빌드 때 링크하지 않고 실행 중에 심벌을 찾아 있을 때만 부른다. 없으면 (b)를 판정하지 않고 원래 연결 오류다. 대상 수준은 위 "대상 target"의 29 그대로다. 이 OS 호출은 `tasty-net-ssh`의 `cfg(target_os = "android")` 모듈 하나가 갖는다(director 2937, [크레이트 구성](crates.md#unsafe-규칙)).
- FFI: 두 사유는 FFI 오류 enum의 서로 다른 변형이다. 콜백은 아래 "API 원칙"의 연결 처리 안 콜백이다.

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
- 이벤트는 연결 목록과 P2P 상태 기계의 전이 원인(`app_background`, `app_foreground`, `network_changed`, `network_lost`, 붙들기의 `background_hold`·`background_hold_released`, 설정 바뀜의 `setting_changed`)으로 기록된다.
- 데스크톱 소비자는 부르지 않아도 된다. 부르지 않으면 지금의 동작 그대로다.
- 이 API의 구현은 후속 작업이다. P2P 상태 기계에 아래 `Paused` 상태와 전이를 더하는 일, [P2P](p2p.md#상태-기계) 표를 고치는 일도 그 작업이 한다.

### 이벤트 처리 규칙

- 이벤트는 들어온 순서대로 두 값을 바꾼다: 앞·뒤(`Foreground`·`Background`), 망 있음·없음(`NetworkChanged`는 있음, `NetworkLost`는 없음). 처음 값은 앞·망 있음이다. 같은 값이 다시 오면 값은 그대로이고, `NetworkChanged`만은 값이 같아도 "망 바뀜" 처리를 한다.
- endpoint는 이벤트마다 두 값과 붙들기 값 하나로 목표를 다시 계산한다. 목표가 "등록"인 조건은 `망 있음 && (앞 || lifecycle.background_registration = keep || 붙들기)`이고, 아니면 "멈춤"이다. 늦게 온 `NetworkChanged`가 백그라운드에서 등록을 다시 켜지 않는 것은 이 규칙 때문이다.
- 붙들기 값은 transport가 준다. p2p는 어느 대상이 `home`인지 모르기 때문이다(`home`은 transport의 연결 옵션이다). transport는 `home` 표시가 있는 P2P 대상이 하나라도 있고 `lifecycle.home_background = keep`일 때 `P2pEndpoint::hold_background_registration(true)`를, 그 조건이 풀리면 `false`를 부른다. 처음 값은 `false`다. 이 호출도 이벤트처럼 목표를 다시 계산하고, 전이 원인은 `background_hold`(켜짐)·`background_hold_released`(꺼짐)다.
- 전이는 목표로 정한다. 아래 표의 원인은 모두 "그 이벤트 뒤 다시 계산한 목표"로 걸러진다: 목표가 "멈춤"인 동안에는 어떤 원인으로도 `Connecting`·`Backoff`로 가지 않고(재시도할 오류도 `Paused`로 간다), 목표가 "등록"인 동안에는 `Paused`로 가지 않는다. 실행 중에 `lifecycle.background_registration`이 어느 방향으로 바뀌든 목표를 다시 계산하고 그 목표의 기존 행을 따른다(원인 `setting_changed`).
- 예정된 닫기: `Registered`에서 목표가 "멈춤"이 되면(`app_background`, `background_hold_released`, `setting_changed`) 바로 닫지 않고 `lifecycle.background_grace` 뒤로 닫기를 예약한다(`network_lost`는 예약 없이 바로 `Paused`). 예약이 끝나기 전에 목표가 다시 "등록"이 되면(`app_foreground`, `background_hold`, 설정 바뀜) 예약을 취소한다. endpoint는 `Registered` 그대로이고 이력에는 취소한 원인만 남는다. 예약이 끝나는 때에 목표를 한 번 더 보고, 그때도 "멈춤"일 때만 닫는다.

### endpoint 상태 (p2p.md에 더할 것)

| 전이 | 원인(그 뒤 목표) |
|---|---|
| `Registered` → `Paused` | 예약한 닫기가 끝남(목표 "멈춤", 서버 연결을 코드 0으로 닫음), `network_lost`(바로) |
| `Registered`(닫기 예약 중, 목표 "멈춤") → `Paused` | 조정 연결이 예약보다 먼저 끊김(`idle_timeout`, `io` 등 P2P 정본이 `Backoff`로 보내는 원인). 목표가 "멈춤"이므로 `Backoff`로 가지 않는다. 예약을 지우고 backoff 대기도 만들지 않는다. 원인은 끊긴 원인 그대로 남긴다. `Stopped`로 가는 재시도하지 않는 원인(`superseded`·`access_denied` 등)은 P2P 정본 그대로 `Stopped`다 |
| `Registered`(닫기 예약 중, 목표 "멈춤") → `Paused` | `network_changed`. 옛 망의 조정 연결을 예약 끝까지 둘 까닭이 없으므로 예약을 지우고 바로 코드 0으로 닫는다. 다시 등록은 목표가 "등록"이 될 때(`Paused → Connecting`)다 |
| `Connecting`·`Backoff` → `Paused` | `app_background`, `background_hold_released`, `setting_changed`, `network_lost`(모두 목표 "멈춤"일 때, 바로). 하던 시도와 backoff 대기를 멈춘다 |
| `Paused` → `Connecting` | `app_foreground`, `background_hold`, `network_changed`, `setting_changed`(목표 "등록"일 때). backoff 시도 번호를 1로 되돌린다. `network_lost` 뒤 망이 없으면 `app_foreground`로도 나가지 않는다 |
| `Backoff` → `Connecting` | `network_changed`(목표 "등록"일 때, 대기를 끝내고 바로, 시도 번호 1). 목표가 "멈춤"이면 위 `Paused` 행이다 |
| `Registered` → `Connecting` | `network_changed`(목표 "등록"일 때, 아래 "망 바뀜") |

- `Paused`가 싣는 값: 원인, 멈춘 시각. 소켓 `L`은 닫지 않는다(P2P를 끄는 것이 아니다).

### 망 바뀜 (`NetworkChanged`)

P2P 정본의 흐름 번호([P2P 흐름](p2p.md#흐름))로 적는다.

| 상태 | 하는 일 |
|---|---|
| `Registered` | 목표가 "등록"이면 조정 연결(서버와의 QUIC 연결)을 코드 0(`closed`)으로 닫고 backoff 없이 흐름 1~4(흐름 14와 같다, 시도 번호 1)를 한다. 옛 연결은 로컬에서 먼저 닫고 퇴역시킨다. 옛 경로가 이미 죽어 CLOSE가 서버에 가지 않으면 서버는 새 등록 때 옛 연결에 `superseded`(코드 2)를 보낼 수 있다([rendezvous](rendezvous.md#메시지-표)). 퇴역한 조정 연결에서 오는 종료는 원인과 관계없이 endpoint 전이에 반영하지 않으므로, 그 `superseded`가 endpoint를 `Stopped`로 보내지 않는다. 서버는 등록 제거를 연결로 가려서 하므로 새 등록은 지워지지 않는다. 조정 연결이 새 망에서 살아 있는지 판정하지 않고 늘 다시 맺는다. 근거: rendezvous 프로토콜에는 등록 뒤 서버가 답하는 요청이 없어(흐름 4 `nat_report`도 응답이 없다) 새 망으로 보낸 것이 전달됐다는 증거를 받을 수 없다. quinn `Connection`에는 ping API가 없고, `Connection::stats()`의 `udp_rx.datagrams`는 패킷 인증 전에 늘고 같은 연결의 아무 패킷이나 세므로 증거가 아니다. 흐름 3(STUN)은 조정 연결 밖이다. 다시 맺는 비용은 QUIC handshake와 `hello`·`welcome` 한 번이고 망 바뀜은 드물다 목표가 "멈춤"(닫기 예약 중)이면 위 전이 표의 `network_changed` → `Paused` 행대로 바로 닫고 다시 맺지 않는다 |
| `Connecting` | 하던 시도를 그대로 둔다(새 망에서 흐름 1부터 하고 있다) |
| `Backoff`·`Paused` | 위 전이 표대로 흐름 1~4를 한다 |

- 소켓 `L`은 다시 열지 않는다. wildcard 바인딩이면 새 망에서도 같은 소켓으로 보낼 수 있다. 보내기가 io 오류로 실패하면 기존 `Registered → Backoff`(`io`) 규칙을 탄다.
- 진행 중인 펀칭(`Requesting`~`Handshaking`)은 원인 `network_changed`로 끝낸다. A는 남은 시도가 있으면 `RetryWait`, B는 `Closed`다. 옛 망의 후보로 프로브를 보내지 않기 위해서다.
- 새 펀칭(흐름 6)은 새 망의 흐름 4(`nat_report`)가 끝난 뒤에만 시작한다. 그 전에 온 연결 요청은 흐름 4가 끝날 때까지 기다린다.
- `Connected` P2P 연결은 `lifecycle.p2p_on_network_change`를 따른다. `reconnect`(기본)면 원인 `network_changed`로 닫고 transport가 위 조건(새 `nat_report` 뒤)에서 새 펀칭으로 다시 맺는다. `keep`이면 [주소 바뀜](p2p.md#주소-바뀜-quic-경로-이동) 규칙에 맡긴다.
- 그 규칙과의 관계: 그 규칙은 OS 알림이 없을 때의 동작이다(A 쪽 재매핑은 QUIC 경로 이동, B 쪽 재매핑은 idle timeout 뒤 새 펀칭). 모바일에서 망이 바뀌면 NAT 매핑도 바뀌고, 상대 NAT의 필터가 새 주소를 막으면 경로 이동도 실패한다. 그래서 OS가 알려 준 망 바뀜에는 idle timeout(기본 45 s)을 기다리지 않고 바로 다시 맺는 것을 기본으로 둔다.

### SSH 터널과 `Redialer`

| 이벤트 | 동작 |
|---|---|
| `Background` | 아래 재연결 조건이 거짓이 된 대상의 대기 중인 재연결을 멈춘다. 열린 터널은 그대로 둔다 |
| `Foreground` | 재연결 조건이 참이 된 대상의 멈춘 재연결을 바로 한 번 시도하고 backoff를 처음 간격으로 되돌린다(망이 없으면 시도하지 않는다) |
| `NetworkChanged` | `lifecycle.ssh_on_network_change = reconnect`(기본)면 SSH 터널(내장 세션 포함)을 원인 `network_changed`로 닫는다. 재연결 조건이 참인 대상만 `Redialer`가 바로 다시 걸고, 거짓인 대상은 조건이 참이 될 때(`Foreground` 등)까지 기다린다. `keep`이면 keepalive가 끊김을 본다 |
| `NetworkLost` | 재연결 대기를 멈추고 `give_up_after` 시계도 멈춘다. 다음 `NetworkChanged`에서 다시 시작한다 |

- 재연결 조건: `Redialer`는 대상마다 `망 있음 && (앞 || lifecycle.background_redial = keep || (그 대상이 home && lifecycle.home_background = keep))`일 때만 시도한다. 두 값(앞·뒤, 망 있음·없음)은 위 "이벤트 처리 규칙"의 값과 같다. 그래서 P2P 등록과 SSH 재연결이 같은 이벤트에 같은 방향으로 움직이고, `home` 대상만 다르게 남길 수 있다.
- 홈 서버 연결: 앱이 늘 붙어 있으려는 대상(보통 사용자의 데스크톱 tasty)을 연결 옵션 `ConnectOptions::home(true)`로 표시한다. `lifecycle.home_background = keep`이면 백그라운드에서도 그 대상의 재연결을 이어 가고(위 조건), 그 대상이 P2P면 transport가 붙들기(`hold_background_registration(true)`)로 P2P 등록도 유지한다. 실제로 이어지는지는 OS가 앱 실행을 허락하는지(Android 포그라운드 서비스 등)에 달렸고, 그 허가는 앱의 몫이다. 기본 `pause`는 다른 대상과 같다.

### 수명 설정값

[크레이트 구성](crates.md#설정값-규칙)을 따른다. p2p 쪽 키는 `P2pSettings`, transport 쪽 키는 `ConnectionTableConfig`·`RedialerConfig`로 받고 같은 방식으로 조회된다. 키마다 한 크레이트만 갖는다. 구현 작업이 [P2P](p2p.md#설정값)·[전송 추상화](transport.md#보관-상한과-재연결-도우미) 표로 옮긴다(그 전에는 이 표가 정본이다).

| 키 | 크레이트 | 기본 | 범위 | 근거 |
|---|---|---|---|---|
| `lifecycle.background_registration` | p2p | `pause` | `pause`·`keep` | 백그라운드 앱은 곧 멈추므로 등록을 붙들어 둘 수 없다. 멈춘 기기의 등록은 서버 `idle_timeout`(기본 45 s)까지 남고, 그동안 이 기기로 온 펀칭은 서버 `offer_timeout` 뒤 `not_found`로 끝난다([rendezvous](rendezvous.md#서버-상태)). 코드 0(`closed`, 기기가 P2P를 끔)으로 먼저 닫으면 등록이 바로 지워지고 원인이 연결 정보에 남는다 |
| `lifecycle.background_grace` | p2p | 10 s | 0 s ~ 10 min | 앱 전환처럼 곧 돌아오는 경우에 등록을 다시 하지 않게 한다. OS가 백그라운드 앱에 실행을 허락하는 시간이 얼마인지는 확인하지 않음. 그보다 짧게 두어야 닫기가 실제로 나간다. 펀칭 실험(P1) 때 기기에서 다시 본다 |
| `lifecycle.p2p_on_network_change` | p2p | `reconnect` | `reconnect`·`keep` | 위 "망 바뀜" |
| `lifecycle.ssh_on_network_change` | transport | `reconnect` | `reconnect`·`keep` | SSH 터널은 TCP라 로컬 주소가 바뀌면 이어지지 않는다. keepalive가 끊김을 볼 때까지(`ssh.server_alive_interval × ssh.server_alive_count_max`) 기다리지 않는다 |
| `lifecycle.background_redial` | transport | `pause` | `pause`·`keep` | 실행이 멈출 앱에서 backoff 시도를 쓰지 않고, 복귀 때 바로 시도한다 |
| `lifecycle.home_background` | transport | `pause` | `pause`·`keep` | OS 허가가 앱 몫이라 기본은 다른 대상과 같다 |

## 바인딩 층

### 위치: tasty-network 안의 `tasty-net-ffi`

director 추천(2856)은 바인딩 크레이트를 이 레포 안 `tasty-net-ffi`에 두고 Kotlin·Swift 생성(`uniffi-bindgen` 실행과 생성물)은 앱 레포(tasty-android 등)에서 하는 것이다. 이 설계도 그렇게 정한다. director 2865(android 요청 R1, 선택 A)가 링크 방식을 정했다.

- 앱은 자기 cdylib(`tasty-mobile-core`) 하나에 `tasty-net-ffi`를 rlib으로 링크해 `.so` 하나로 싣는다. `tasty-net-ffi`를 따로 cdylib으로 싣지 않는다.
- Kotlin 생성은 앱 레포의 Gradle 작업이 그 최종 `.so`를 입력으로 라이브러리 모드로 한다. 그래서 한 번의 생성으로 앱의 UniFFI export와 `tasty-net-ffi`의 export가 함께 나온다.
- 판정 기준: 앱·네트워크 양쪽 export가 최종 `.so`에 남아 생성 Kotlin에서 호출된다, 모든 UniFFI 크레이트와 생성기가 같은 판, 모든 tasty-network 의존이 같은 rev, 최종 Android 의존 그래프에서 `system-ssh`가 꺼져 있다. 다른 프로그램 실행 금지는 tasty-network 크레이트에만 적용되고 앱 자신의 코드(로컬 터미널 PTY 등)는 대상이 아니다.

근거:

1. FFI 레코드와 오류 enum은 코어 크레이트의 공개 타입을 옮긴 것이다. 같은 workspace에 있으면 코어 API가 바뀌는 커밋에서 FFI 변환도 함께 고쳐지고, 고치지 않으면 이 레포의 clippy·test가 바로 막는다. 앱 레포에 두면 그 깨짐을 앱 레포가 다음에 판을 올릴 때에야 안다.
2. 모바일 cross check와 `no-process-spawn`이 ffi를 포함한 의존 그래프 전체를 이 레포의 CI에서 본다. ffi가 다른 레포에 있으면 ffi가 켜는 feature 조합(`embedded-ssh`만)을 이 레포가 검사할 수 없다.
3. `uniffi` 판은 Rust 쪽 scaffolding(이 레포)과 생성기(앱 레포), 그리고 앱의 cdylib에 함께 링크되는 다른 UniFFI 크레이트가 모두 같아야 한다. 두 레포가 같은 판을 쓰고, 이 레포가 그 판을 정확히 고정해 기록한다. 판은 앱과 같은 판이다(director 2931, 아래 "크레이트"의 UniFFI 판).
4. 생성물(Kotlin·Swift 소스)과 그것을 묶는 빌드(Gradle의 cargo-ndk, AAR, Xcode)는 앱의 빌드 체계에 붙어 있다. 앱 레포가 생성하면 앱 빌드가 쓰는 판과 생성물이 늘 같다.
5. 이 레포는 공개 레포이고 앱 쪽 구성은 이 레포의 범위가 아니다.

추천과 다르게 볼 근거로 확인한 것:

- 생성을 앱 레포만 하면, 이 레포의 변경이 UniFFI 생성 단계에서만 드러나는 오류(이름 충돌, 지원하지 않는 타입, Kotlin 예약어)를 앱 레포가 판을 올릴 때에야 본다. 그래서 이 레포도 생성 검사를 갖는다. 생성은 하되 결과는 버리고 커밋하지 않는다(아래 표의 "생성 검사"). 위치를 바꿀 근거는 아니고 검사를 하나 더하는 근거다.
- iOS 앱 레포가 없다(사용자 결정 "구조만"). Swift 생성물을 쓸 곳이 없으므로 Swift는 이 레포의 생성 검사까지만 한다.
- 앱 레포가 둘 이상(tasty-android, 나중의 iOS)이면 각자 생성한다. 생성 명령과 판이 같으므로 결과도 같다.

### 경계

| 이 레포(tasty-network)가 갖는 것 | 앱 레포(tasty-android 등)가 갖는 것 |
|---|---|
| `crates/tasty-net-ffi`: proc-macro 정의(`uniffi::setup_scaffolding!`, `#[uniffi::export]`, `#[derive(uniffi::Record, uniffi::Enum, uniffi::Error, uniffi::Object)]`). UDL 파일은 두지 않는다 | `uniffi-bindgen generate` 실행과 생성된 Kotlin·Swift 소스(생성물을 앱 레포에 커밋할지는 앱 레포가 정한다) |
| `uniffi` 정확한 판 고정과 기록, `uniffi.toml`(Kotlin 패키지 이름, Swift 모듈 이름) | 그 판의 생성기를 쓰는 Gradle 작업(라이브러리 모드, 앱 cdylib의 `.so`를 입력으로), 앱 쪽 UniFFI 크레이트를 같은 판으로 맞추기 |
| FFI 레코드·오류 enum·객체와 코어 타입 사이 변환, Rust 쪽 FFI 시험 | Rust 라이브러리 빌드(cargo-ndk 등, target별 `.so`), AAR·APK 묶기, 앱 서명 |
| `crate-type = ["lib", "cdylib", "staticlib"]`. `lib`는 앱의 cdylib에 rlib으로 링크되는 정식 경로다(아래 "rlib 링크 계약"). `cdylib`·`staticlib`은 이 레포의 생성 검사와 앱 없이 쓰는 경우용 | 앱 cdylib(`tasty-mobile-core`)이 `tasty-net-ffi`에 의존하고 링크하는 일, 앱 수명 이벤트를 `TastyNet::app_event`로 넘기는 코드, `SshPrompt`·`PeerKeyStore`·지역망 판정 콜백 구현(권한 확인·요청), 포그라운드 서비스 |
| 모바일 cross check의 ffi 행, `no-process-spawn`, 생성 검사(호스트 라이브러리를 빌드해 Kotlin·Swift를 임시 폴더에 생성하고 성공만 본다) | 앱 저장소의 키 디렉터리와 `ssh/` 폴더 경로를 `TastyNet` 생성 인자로 주는 일 |
| 바인딩 API 문서(생성되는 문서 주석) | 앱의 라이선스 고지(OpenSSH, MPL-2.0 UniFFI 런타임) |

### 크레이트

- 의존(`Cargo.toml`):
  - `tasty-net-transport = { path, default-features = false, features = ["embedded-ssh"] }`
  - `tasty-net-ssh = { path, default-features = false, features = ["embedded-ssh"] }`(`SshConnector`·`SshSettings`·`SshTarget`·`SshPrompt`·`SshInspection`·exec·SFTP 타입을 직접 쓴다)
  - `tasty-net-p2p`, `tasty-net-identity`, `tasty-net-topology`
  - `uniffi`(정확한 판)

  ffi 자신은 feature가 없다. 모바일 cross check가 ffi를 `-p tasty-net-ffi`로 빌드하면 resolver 3이 이 크레이트가 켠 feature만 쓰므로 `system-ssh`는 들어오지 않는다(의존 쪽 검사가 확인한다).
- 순서: ffi는 내장 백엔드 작업(2단계 feature) 뒤에 만든다. 그 전에 SSH 없는 ffi를 먼저 내지 않는다. 바인딩의 SSH 모양이 나중에 바뀌지 않게 하기 위해서다.
- 다른 크레이트는 UniFFI에 의존하지 않는다. FFI용 타입은 이 크레이트가 따로 정의하고 코어 타입에서 옮긴다(코어 크레이트의 공개 API가 FFI 제약을 따르지 않게 한다).
- unsafe: [크레이트 구성](crates.md#unsafe-규칙)의 `deny(unsafe_code)` 분류다.
- UniFFI 판: 앱과 같은 판을 쓴다. 앱은 지금 0.32다. 판을 고정할 때 director에게 알린다(director 2931). 이 규칙이 director 2865의 "이 레포가 그때의 최신 안정판을 먼저 고정하고 앱이 따라 올린다"를 바꿨다. 판 고정(`=` 판)은 바인딩 크레이트 작업이 한다. 참고로 2026-11-27 crates.io 최신 안정판은 0.33.0(2026-11-25 게시), 그 앞은 0.32.2(2026-09-23)다. 라이선스 MPL-2.0(`deny.toml` 허용). 0.33.0의 `uniffi-bindgen`은 라이브러리 모드가 기본이고 `--library` 플래그는 남아 있지만 deprecated다. 생성 명령은 고정한 판의 문서로 다시 확인한다.
- Kotlin 패키지 이름(android 요구 #12): UniFFI 0.33.0의 Kotlin 생성기는 `uniffi.toml`의 `[bindings.kotlin] package_name`이 없으면 `uniffi.<네임스페이스>`를 쓴다(`uniffi_bindgen` 0.33.0 `src/bindings/kotlin/mod.rs:94`–`:96`). 그래서 이 크레이트는 기본이면 `uniffi.tasty_net`이다(0.32.2도 같은 동작이다. 아래 줄 번호는 0.33.0의 것이라 판을 고정할 때 그 판으로 다시 적는다). 라이브러리 모드도 크레이트마다 그 크레이트 루트의 `uniffi.toml`을 읽으므로(`src/bindgen_paths.rs:141`–`:144`) `tasty-net-ffi`의 이름은 이 레포의 `uniffi.toml`이 정하고, 앱은 덮어쓸 필요가 없다. 앱이 바꾸고 싶으면 생성 명령의 `--config <파일>`에 `[crates.<크레이트>]` 표를 두면 크레이트의 `uniffi.toml`보다 이긴다(`src/global_config.rs:96`–`:121`). 값은 바인딩 크레이트 작업이 정한다.
- 그 `uniffi.toml`에 `cdylib_name`은 두지 않는다. 병합된 Kotlin 설정(앱 `--config`의 `[defaults]`, 이 레포의 `uniffi.toml`, 앱 `--config`의 `[crates.<크레이트>]` 순으로 뒤가 이긴다)에 `cdylib_name`이 없으면 라이브러리 모드는 입력으로 준 라이브러리 파일 이름(앱의 `tasty_mobile_core`)을 생성 Kotlin이 싣는 이름으로 쓴다(`src/bindings/kotlin/mod.rs:31`·`:97`–`:101`, `src/loader.rs:314`–`:322`). 어느 층이든 값을 넣으면 그 값이 쓰이고, 그 값이 앱 라이브러리 이름과 같을 때만 같은 라이브러리를 싣는다. 앱 설정으로 넣는 것은 앱의 몫이다.

### rlib 링크 계약

`tasty-net-ffi`는 다른 크레이트의 cdylib에 rlib으로 링크돼도 그대로 동작해야 한다.

- 네임스페이스: `uniffi::setup_scaffolding!("tasty_net")`처럼 네임스페이스를 이름으로 준다. 앱 크레이트는 자기 네임스페이스를 쓴다. UniFFI가 만드는 C 심벌은 크레이트 이름이 들어간 이름(`uniffi_<크레이트>_fn_…`)이라 같은 `.so` 안에서 겹치지 않는다. 앱 쪽 크레이트 이름이나 네임스페이스가 `tasty_net`과 같으면 안 된다는 것을 앱 레포에 알린다.
- 전역 상태: `tasty-net-ffi`는 프로세스 전역 초기화(전역 로거 설치, 전역 tokio 런타임, `#[no_mangle]` 진입점 손수 정의)를 하지 않는다. 런타임은 `TastyNet` 객체가 소유한다. 그래서 한 `.so` 안의 다른 크레이트와 전역을 두고 다투지 않는다.
- 링크 유지: 앱 크레이트가 `tasty-net-ffi`의 항목을 하나도 쓰지 않으면 rlib이 링크되지 않아 export가 빠질 수 있다. 앱이 그것을 막는 방법(`use tasty_net_ffi as _;` 등)은 구현 작업이 고정판에서 확인해 앱 레포에 알린다.
- 시험: 이 레포에 시험용 cdylib 크레이트 하나(`publish = false`, 자기 UniFFI export 하나)를 두고, 그것이 `tasty-net-ffi`를 rlib으로 링크한 `.so`에서 라이브러리 모드 생성이 두 네임스페이스의 Kotlin을 모두 만들고 양쪽 export 심벌이 남아 있는지 본다. 생성 검사와 같은 단계에서 돈다.

### API 원칙

- 동기 API: 코어 API가 동기이므로 FFI 함수도 동기로 내보낸다. 막히는 호출(연결, 읽기, exec 대기, SFTP 읽기·쓰기)은 앱이 UI 스레드가 아닌 스레드에서 부른다. 생성된 문서 주석에 적는다. UniFFI의 async 내보내기는 쓰지 않는다(코어에 async API가 생기면 다시 본다).
- 이벤트 전달: 끌어오기 방식이다. `EventStream::next(timeout) -> Option<Event>`(막히는 호출)를 앱이 자기 스레드에서 돌린다. 코어의 `events()` 큐와 `Lagged{dropped}` 규칙을 그대로 쓴다. 라이브러리 스레드에서 앱 코드를 부르는 콜백은 두지 않는다(앱 런타임 스레드 규칙과 재진입 문제를 피한다).
- 콜백이 필요한 세 곳: 모두 연결을 연 앱 스레드가 막혀 있는 동안 그 연결 처리 안에서만 불린다. 라이브러리 스레드가 앞의 호출과 무관하게 앱 코드를 부르는 일은 없다.
  - 연결 도중 사용자 답이 필요한 `SshPrompt`(passphrase, 비밀번호, keyboard-interactive, `StrictHostKeyChecking ask`)는 UniFFI의 callback interface로 받는다. 앱은 UI 스레드로 넘겨 답을 받는다. 답을 기다리는 시간은 `embedded.prompt_timeout`이다([SSH 설정값](ssh-backends.md#설정값)).
  - 상대 기기 키 신뢰 저장소 `PeerKeyStore`(아래 "android 요구의 자리" #7b).
  - 지역망 판정(위 [직접 LAN 연결](#직접-lan-연결-android-지역망-권한), #14).
- 오류 변환: 코어 오류 enum마다 같은 변형을 가진 FFI 오류 enum(`#[derive(uniffi::Error)]`)으로 옮긴다. 변형 이름이 앱의 i18n 키 분기에 쓰이고, 메시지는 코어의 영어 `Display`다([크레이트 구성](crates.md#오류-규칙)). 문자열 비교로 분기하는 일이 없게 한다. `is_retryable()`은 FFI 메서드로 함께 낸다. 코어 enum이 `#[non_exhaustive]`라서 FFI enum에는 `Other { message }` 변형을 하나 둔다.

### 바인딩 API (요구를 받을 자리)

바인딩 API의 목록은 tasty-android의 요구(바인딩 API, exec, SFTP)를 받아 정한다. 받은 요구는 아래 "android 요구의 자리"에 경계만 적었고, 메서드 모양은 바인딩 크레이트 작업이 정한다. 그 전에는 아래 뼈대만 둔다.

| 영역 | 뼈대 | 코어 쪽 정본 |
|---|---|---|
| 시작 | `TastyNet::new(config)`(키 디렉터리, `ssh/` 폴더, 설정값), `TastyNet::app_event(AppEvent)` | 위 "앱 수명" |
| 연결 | 연결 열기·닫기, 연결 목록 조회, SSH 세션 목록 조회, `EventStream` | [전송 추상화](transport.md#연결-목록), [SSH 세션 목록](ssh-backends.md#ssh-세션-목록) |
| SSH exec | 명령 실행, 표준 입력 쓰기, stdout·stderr 읽기, 종료 상태 | [터널·exec·SFTP](ssh-backends.md#터널execsftp) |
| SFTP | 열기·읽기·쓰기·목록·정보·이름 바꾸기·지우기 | 같다 |
| 요청 채널 | `RequestClient` 객체(호출·닫기), 상대 `caps` 조회 | [요청 클라이언트](transport.md#요청-클라이언트-dialer-쪽) |
| 설정 | 설정값 쓰기·조회(`{key, value, source}`), SSH 설정 조회(`SshConnector::inspect`: 백엔드와 선택 이유, 읽은 파일, 거부·미지원 지시어) | 각 크레이트의 설정값 표, [설정 조회](ssh-backends.md#설정-조회) |

### android 요구의 자리

tasty-android의 요구 목록(director 2865, 앱 쪽 보고서 R2의 번호)을 받을 자리다. 여기에는 FFI 경계와 계약만 적고, 레코드·메서드 모양은 바인딩 크레이트 작업이 정한다. Rust API 쪽 계약은 [SSH 백엔드의 android 요구](ssh-backends.md#android-요구의-자리)에 있다.

| # | 요구 | FFI 경계와 계약 |
|---|---|---|
| 1 | 앱 폴더의 OpenSSH 파일로 내장 SSH 접속, 사용자 답은 콜백 | `TastyNet::new`가 앱 저장소의 `ssh/` 경로를 받아 `embedded.ssh_dir`로 넣는다. 사용자 답은 `SshPrompt` 콜백 하나다 |
| 2 | 메모리 바이트 키 | FFI는 키 바이트를 `Vec<u8>`로 한 번 받아 바로 Rust 쪽 소유로 옮긴다. 그 뒤 FFI 쪽 사본을 지운다. 키 바이트를 다시 돌려주는 메서드는 없고, 조회는 지문만 낸다. Kotlin 쪽 배열을 지우는 것은 앱의 몫이다 |
| 3·4·6 | exec·SFTP·세션 공유 | Rust API를 그대로 옮긴다. SFTP 실패 사유(`PermissionDenied`·`NotFound`·`SubsystemUnavailable` 등)는 FFI 오류 enum의 변형으로 낸다(문자열로 가르지 않는다, 위 "오류 변환") |
| 5 | SFTP 쓰기 | 앱 쪽 결정 뒤. 보류 |
| 6b | attach 없는 SSH 세션의 상태 | Rust 쪽은 `tasty-net-ssh`의 [SSH 세션 목록](ssh-backends.md#ssh-세션-목록)이다(transport 연결 목록은 바꾸지 않는다). ffi는 transport 연결 목록과 SSH 세션 목록의 이벤트를 앱에 하나의 `EventStream`으로 합쳐 내고, 이벤트마다 어느 목록의 것인지 종류를 붙인다. 현재 상태를 다시 읽는 조회도 낸다: 연결 목록 조회와 나란히 `SshConnector::sessions()`의 스냅샷을 FFI 조회 API로 낸다. 앱이 이벤트 흐름을 늦게 열었거나 `Lagged{dropped}`를 받은 뒤에는 두 조회로 상태를 다시 맞춘다. 메서드 이름·레코드 필드·보관 상한은 바인딩 크레이트 작업이 정한다 |
| 7 | 원격 Tasty attach 경로의 노출 범위 | FFI가 내는 것: 대상으로 연결 열기(포트 발견 → `direct-tcpip` → handshake v2는 Rust 안에서 한다), 열린 연결 위 `attach`·`request`·`bulk` 채널 열기, 채널 위 프레임 읽기·쓰기(바이트), 닫기, 연결 레코드의 포트·인스턴스 ID·상대 기기 ID. 프레임의 도메인 해석은 앱·tasty 쪽이다. FFI가 내지 않는 것: 연결 토큰. 토큰은 Rust 안에서 포트 발견 결과에서 handshake로만 간다. FFI 레코드·오류 메시지·`Debug`·이벤트·설정 조회 어디에도 토큰을 싣지 않고, 시험이 그 출력들에 토큰 문자열이 없는지 본다. `PeerTooOld`(숫자 포트만 돌려주는 상대)는 FFI 오류 변형이다 |
| 7+ | 요청 클라이언트, 상대 `caps`, `ResponseTooLarge`·`PeerTooOld`의 `evidence`, 발견과 터널의 대상 일치 | FFI는 transport의 [요청 클라이언트](transport.md#요청-클라이언트-dialer-쪽)를 객체로 감싸 호출과 닫기를 낸다. 상대 `caps`는 handshake 뒤 그 객체의 조회로 낸다(attach 링크 쪽 조회는 후속 작업이 정한다). `dial_request`가 `net.request`를 열기 조건으로 보지 않으므로 첫 호출 전 확인은 앱이 한다. `RequestError`는 변형마다 FFI 변형으로 옮기고 `ResponseTooLarge`를 따로 둔다. `PeerTooOld`는 두 경로(포트 발견의 `ConnectError::PeerTooOld`, handshake의 `HandshakeError::PeerTooOld`. `HandshakeError::Connect`에 싸인 앞의 것도) 모두 같은 FFI 변형에 `evidence`(`PortOnlyDiscovery`·`V1Ack`)를 싣는다. 발견과 터널의 대상이 다르면 [SSH 백엔드](ssh-backends.md#포트-발견과-터널의-대상-일치)의 `TargetMismatch`를 같은 이름의 FFI 변형으로 낸다. 객체·메서드 이름과 값 표현은 바인딩 크레이트 작업이 정한다 |
| 7b | `PeerKeyStore` | 앱이 구현하는 callback interface로 받는다([identity](identity.md)의 `get`·`pin_if_absent`·`replace`와 같은 세 메서드). 최초 등록의 원자성(`pin_if_absent`)은 앱 저장소가 지킨다. 키가 바뀌면 연결은 `PeerKeyMismatch`로 끝나고 라이브러리가 `replace`를 스스로 부르지 않는다. 다시 등록은 사용자가 확인한 뒤 앱이 부르는 FFI 메서드(`replace`를 거친다)로만 한다. 저장소 오류는 `HandshakeError::Store`처럼 앱의 오류 문자열을 담아 돌아온다 |
| 8 | 연결 목록·사유·`is_retryable()`·`EventStream::next(timeout)`·`AppEvent` | 위 "API 원칙"과 "앱 수명" |
| 9 | 앱 폴더의 기기 키 | `TastyNet::new`가 키 디렉터리를 받아 `DeviceKey::load_or_create`로 만든다. 비밀키는 FFI로 내지 않고 기기 ID만 낸다. `File::lock`이 Android에서 잠그는지는 연결 레코드 경로의 문제라 따로 확인하고 결과만 기록한다 |
| 10 | Android용 태그 | 앱이 소비하는 기준은 tasty 전환 태그(tasty가 이 레포 크레이트로 옮겨 가며 고정하는 태그)다. 앱은 따로 태그를 받지 않고 그 태그의 rev를 쓴다. 그 태그는 모바일 cross check(`aarch64-linux-android`·`x86_64-linux-android`, NDK API 29)가 통과한 커밋에만 붙인다. 정본은 [버전·호환](versioning.md#태그와-브랜치)의 태그 규칙이다. 태그를 붙인 사실을 앱 쪽에 알리는 경로는 이 설계 밖이다 |
| 11 | 내장 백엔드의 PowerShell·cmd 포트 발견 | Rust 쪽 시험이 맡는다. FFI에는 더할 것이 없다 |
| 12 | 라이브러리 모드의 Kotlin 패키지 이름 | 위 "크레이트". 이 레포의 `uniffi.toml`이 정하고 앱은 덮어쓸 필요가 없다. `cdylib_name`은 두지 않는다 |
| 13 | 메모리 키와 `IdentityFile`·`IdentitiesOnly`, 세션 재사용 키 | Rust 쪽이다([SSH 백엔드의 android 요구](ssh-backends.md#android-요구의-자리) #2). FFI에는 더할 것이 없다 |
| 14 | 직접 LAN 연결 판정과 두 차단 사유 | 위 [직접 LAN 연결](#직접-lan-연결-android-지역망-권한). 앱이 구현하는 지역망 판정 콜백 하나와 FFI 오류 변형 둘(`LocalNetworkDenied`·`LocalNetworkBlocked`) |
| 15 | 의존 쪽 검사를 앱의 최종 그래프에서 | 위 [의존 쪽 검사](#의존-쪽-검사)의 앱 그래프 기준 |

## 모바일 cross check

CI 잡 둘을 더한다. 구현은 후속 작업이고, 자세한 잡 설정은 그때 [CI](../dev-guide/ci.md)와 [CI와 시험 환경](testing.md)에 적는다.

| 잡 | 러너 | target | 준비 |
|---|---|---|---|
| `mobile-android` | `ubuntu-latest` | `aarch64-linux-android`, `x86_64-linux-android` | `rustup target add`, NDK 설치(판 고정), 위 "대상 target"의 `CC_<target>`·`AR_<target>`(`llvm-ar`)·`CARGO_TARGET_<TARGET>_LINKER` |
| `mobile-ios` | `macos-latest` | `aarch64-apple-ios`, `aarch64-apple-ios-sim` | `rustup target add` |

- 검사 단계 이름: `mobile-check`(`scripts/check.sh mobile-check <target>`). 로컬에 그 target이 없으면 단계는 실패하지 않고 "target 없음"을 한 줄로 알리고 건너뛴 target을 적는다. CI 잡은 건너뛰기를 실패로 본다(환경 변수로 구분).
- 명령(target마다, 모두 `--locked`): `cargo check --locked --target <t> -p <크레이트> --lib <feature 인자>`. 조합마다 위 "의존 쪽 검사"의 `cargo tree`를 같은 인자로 돌린다.

  | 크레이트 | feature 인자 | 목록에 드는 때 |
  |---|---|---|
  | `tasty-net-frame`, `tasty-net-identity`, `tasty-net-topology` | 없음(기본) | 모바일 cross check 작업부터 |
  | `tasty-net-transport` | `--no-default-features`(SSH 없음) | 같다 |
  | `tasty-net-rendezvous` | 없음(기본, `--lib`) | 같다 |
  | `tasty-net-p2p` | 없음(기본) | T3가 develop에 들어간 커밋부터 |
  | `tasty-net-ssh-config` | 없음 | 이식 크레이트가 생긴 커밋부터 |
  | `tasty-net-ssh` | `--no-default-features --features embedded-ssh` | 내장 백엔드 작업(2단계 feature)이 들어간 커밋부터 |
  | `tasty-net-transport` | `--no-default-features --features embedded-ssh` | 같다 |
  | `tasty-net-ffi` | 없음 | ffi 크레이트가 생긴 커밋부터 |

- 그 전 단계에서 `tasty-net-ssh`는 목록에 없다. 1단계의 ssh에는 `system-ssh`만 있어 모바일 조합이 없기 때문이다. 목록에서 빼는 것은 이 문서가 모바일 대상이 아니라고 정한 것(`tasty-net-demo`, rendezvous 실행 파일)과 아직 없는 크레이트·조합뿐이다.
- 같은 잡이 정적 검사 `no-process-spawn`과, ffi가 생긴 뒤에는 생성 검사(Kotlin은 `mobile-android` 잡, Swift는 `mobile-ios` 잡)를 본다.
- `cargo check`는 링크하지 않으므로 NDK 링커 설정이 맞는지는 보지 않는다. C 코드 컴파일(`ring`)만 NDK clang과 `llvm-ar`을 쓴다. 링크까지 보는 검사(`cargo build --target`)는 앱 쪽 묶기에서 처음 필요해진다.

## D 기준과의 관계

결정됨(director 2931): D11(모바일 target 빌드 검사)을 [동작 수준 기준](done-criteria.md)에 넣는다. 내장 백엔드 attach 시험은 tasty 전환 기준에 넣지 않고 [모바일 공개 전 기준](done-criteria.md#모바일-공개-전-기준)으로 따로 둔다. D11을 판정할 `mobile-android`·`mobile-ios` 잡은 아직 없다. 모바일 cross check 작업이 만든다.

- 태그 규칙([버전·호환](versioning.md#태그와-브랜치))도 tasty 전환 태그를 모바일 cross check 통과 커밋에만 붙이게 하므로, 모바일 cross check CI(모바일 cross check 작업)가 tasty 전환 태그보다 먼저 있어야 한다.
- D11은 판정 sha마다 위 cross check 표에서 "목록에 드는 때"를 채운 행 전체를 본다. T3가 들어온 뒤의 `tasty-net-p2p`, 이식 크레이트가 생긴 뒤의 `tasty-net-ssh-config`, 내장 백엔드 작업 뒤의 내장 조합 두 행도 그 커밋부터 든다.

## 후속 작업과 미정

| 항목 | 상태 |
|---|---|
| transport ssh feature 분리(1단계) | 후속 작업. [feature 계약](ssh-backends.md#feature-계약) |
| 모바일 cross check CI와 `no-process-spawn` | 후속 작업. 이 문서 "모바일 cross check", "다른 프로그램 실행 금지" |
| OpenSSH 해석 이식 크레이트 | 후속 작업. [SSH 백엔드](ssh-backends.md#openssh-해석-이식-크레이트) |
| 내장 백엔드·선택 규칙·exec·SFTP(2단계 feature) | 후속 작업. [SSH 백엔드](ssh-backends.md) |
| 앱 수명 API(`AppEvent`, `hold_background_registration`)와 P2P `Paused` 상태 | 후속 작업. 이 문서 "앱 수명" |
| `tasty-net-ffi`, 생성 검사, rlib 링크 시험, UniFFI 판 고정과 director 알림 | 후속 작업(내장 백엔드 뒤, 바인딩 크레이트). 이 문서 "바인딩 층" |
| hotfix 태그의 판정 순서 | 후속 작업(모바일 cross check). hotfix는 버전 커밋과 태그를 한 번에 push하므로 태그 조건([버전·호환](versioning.md#태그와-브랜치))을 볼 CI가 태그 전에 없다. 태그 없이 `main`을 먼저 push해 CI를 볼지, 다른 ref에서 판정할지 |
| tasty 전환 뒤 태그의 모바일 조건 | 후속 작업(모바일 cross check). tasty 전환 태그 뒤의 태그에도 모바일 cross check 통과를 요구할지 |
| FFI의 요청 클라이언트와 오류 | 후속 작업(바인딩 크레이트). #7+: 객체·메서드 이름, `params`와 결과의 표현(JSON 문자열 또는 바이트), `Remote` 오류 값의 표현, attach 링크 쪽에도 상대 `caps` 조회를 둘지, `PeerTooOld`·`TargetMismatch` 변형의 모양 |
| Kotlin 패키지 이름의 값 | 후속 작업(바인딩 크레이트). #12: `uniffi.toml`의 `package_name` 값, rlib 링크 시험에서 두 네임스페이스의 Kotlin이 모두 `tasty_mobile_core`를 싣는지 볼지 |
| 지역망 판정 콜백과 두 오류의 FFI 모양 | 후속 작업(바인딩 크레이트, 내장 SSH 백엔드). #14: 콜백 이름과 인자, 두 변형의 필드, 콜백 안에서 앱이 권한을 요청할 때의 대기 상한, 실행 중 심벌을 찾을 라이브러리와 방법 |
| 앱 그래프의 의존 쪽 검사 스크립트 | 후속 작업(모바일 cross check). #15: 인자 모양(`Cargo.toml` 경로·크레이트·target·feature), `tokio` `process`를 켠 경로 판정의 출력, 그래프에 tokio가 여러 판일 때(`-i tokio@<판>`)와 없을 때(`cargo tree`가 실패한다)의 처리, 앱 레포 CI에서 부르는 방법 |
| FFI의 SSH 세션 조회 | 후속 작업(바인딩 크레이트). 경계는 "android 요구의 자리" #6b(이벤트 병합과 `sessions()` 스냅샷 조회). 메서드 이름·레코드 필드·보관 상한 |
| 바인딩 API 목록 | 요구 받음(director 2865). 경계는 "android 요구의 자리", 메서드 모양은 바인딩 크레이트 작업이 정한다 |
| `File::lock`이 Android·iOS에서 잠그는지 | 확인하지 않음. 기기나 에뮬레이터에서 identity·p2p 잠금 시험을 돌려야 안다. 안 잠그면 대안을 그때 정한다. 기준은 identity의 파일 갱신과 P2P 키 디렉터리에서 필요한 프로세스 사이 배타성을 지키는지다 |
| 의존(tokio·quinn·ring·russh)의 최소 iOS 배포 대상 | 확인하지 않음 |
| `x86_64-linux-android`·iOS 두 target의 빌드 | 확인하지 않음(원격·작업 머신에 target 없음) |
| tasty 설정 화면의 SSH 항목 | tasty 전환 때. 이 레포 범위 밖 |
