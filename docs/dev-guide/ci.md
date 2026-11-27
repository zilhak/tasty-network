# CI

워크플로는 `.github/workflows/ci.yml`과 `.github/workflows/rendezvous.yml` 두 개다. 둘 다 `develop`·`release`·`main` push, 이 세 브랜치를 base로 한 PR, 수동 실행(`workflow_dispatch`)에서 돈다. `ci.yml`의 검사는 `scripts/check.sh`의 단계를, `rendezvous.yml`의 검사는 `rendezvous/check-config.sh`와 `rendezvous/container-check.sh --self-test`를 부르므로 로컬과 훅에서 같은 명령으로 다시 돌릴 수 있다.

## 잡

| 잡 | 러너 | 단계 |
|---|---|---|
| `fmt` | `ubuntu-latest` | `scripts/check.sh fmt` |
| `clippy-test` | `ubuntu-latest`·`macos-latest`·`windows-latest` (`fail-fast: false`) | `scripts/check.sh lint`, sshd 준비(아래 "SSH 통합 시험의 sshd"), `ssh -G` 기준 클라이언트 준비(아래 "`ssh -G` 대조의 기준 클라이언트", Linux·macOS), `scripts/check.sh test --no-fail-fast`, `scripts/check.sh transport-no-ssh`(SSH 없는 transport), `cargo test -p tasty-net-rendezvous --locked`·`cargo test -p tasty-net-p2p --locked`(단독 실행), `scripts/check.sh p2p-example-pair`(P2P 예제 두 프로세스) |
| `repo-checks` | `ubuntu-latest` | `shellcheck`(`scripts/check.sh shell`), `no-tasty-deps`(`scripts/check.sh no-tasty-deps`), `p2p-isolation`(`scripts/check.sh p2p-isolation`), `no-hidden-constants`(`scripts/check.sh no-hidden-constants`) |
| `deny` | `ubuntu-latest` | `scripts/check.sh deny` |
| `rendezvous-check` (`rendezvous.yml`) | `ubuntu-latest` | `bash rendezvous/check-config.sh --self-test`, `bash rendezvous/check-config.sh`, `bash rendezvous/container-check.sh --self-test`(D5 (b) 검사의 판정 함수만. docker 없이 돈다) |

- 러너는 GitHub 호스트 러너다([시험](../design/testing.md)). 잡마다 `timeout-minutes`가 있고 권한은 `contents: read`뿐이다.
- `rendezvous.yml`은 `rendezvous/` 밖만 바뀐 커밋에서도 돈다. `develop`의 어느 커밋이든 D5 (a)의 실행 ID를 남기기 위해서다. 아래 concurrency 규칙은 두 워크플로가 같고 그룹 이름의 앞머리(`ci-`·`rendezvous-`)만 다르다.
- `rendezvous-check`는 서버를 이미지가 아니라 그 커밋에서 빌드한 실행 파일로 띄운다(`cargo build --locked -p tasty-net-rendezvous --bin tasty-rendezvous --example stun_probe`). D5 (a)가 보는 것(예시 설정으로 뜸, 주·보조 포트의 STUN 응답, 설정값 조회)은 이미지 안과 같은 실행 파일의 동작이고, 이미지 빌드는 러너에서 몇 분을 더 쓴다. STUN 응답은 이 레포의 STUN 코덱을 쓰는 `stun_probe` 예제로 보므로 외부 STUN 클라이언트가 없다. 이미지로 띄운 서버는 CI가 아니라 원격 빌드 환경의 `rendezvous/container-check.sh`(D5 (b))가 본다. 그 스크립트는 `CONTAINER_CHECK_IMAGE`를 주지 않으면 실행마다 이미지를 빌드한다.
- push와 수동 실행은 브랜치와 커밋마다 다른 concurrency 그룹을 쓴다. 그래서 서로 다른 커밋의 push는 각자 실행이 남는다. 같은 ref·같은 커밋의 push와 수동 실행은 그룹을 함께 쓰므로, 그 그룹에서 대기 중인 앞 실행은 같은 ref·커밋의 새 요청에 밀려 취소될 수 있다.
- PR은 PR마다 그룹 하나를 쓰고, 새 커밋이 오면 앞 커밋의 실행을 취소한다.
- Windows 잡은 Git Bash(`shell: bash`)로 돈다.

## SSH 없는 transport

`clippy-test` 세 잡의 `transport without ssh` 단계가 `scripts/check.sh transport-no-ssh`를 부른다. `tasty-net-transport`를 `--no-default-features`(SSH 없음)로 clippy(`-D warnings`)와 시험, 문서(`cargo doc --no-deps`, `RUSTDOCFLAGS=-D rustdoc::broken_intra_doc_links`라 SSH 항목을 무조건 가리키는 문서 링크는 실패)를 돌리고, 그 빌드의 `cargo tree`(모든 target, 일반·빌드·dev 의존)에 `tasty-net-ssh`가 없는지 본다. 같은 `cargo tree`를 기본 feature로 먼저 읽어 `tasty-net-ssh`가 보이는지 확인하므로, 판정이 그 줄을 찾지 못하는 고장은 실패로 드러난다. `cargo tree`가 실패하면(종료 코드가 0이 아니면) 출력과 관계없이 실패다. 판정은 먼저 자체 시험(가짜 `cargo tree`의 출력과 종료 코드 여섯 사례)을 돈다. 조합의 정본은 [SSH 백엔드](../design/ssh-backends.md#feature-계약)다. 세 OS에서 도는 것은 `cfg(windows)` 코드도 SSH 없이 컴파일되는지 보기 위해서다.

## SSH 통합 시험의 sshd

`clippy-test` 잡은 시험 단계 전에 SSH 통합 시험(`crates/tasty-net-demo/tests/ssh_attach.rs`, `ssh_reverse_attach.rs`)이 쓸 OpenSSH sshd를 준비하고 결과를 로그에 남긴다. 규칙의 정본은 [CI와 시험 환경](../design/testing.md) "SSH 통합 시험의 sshd"다.

| 단계 | OS | 하는 일 |
|---|---|---|
| `Install OpenSSH server (Linux)` | Linux | `/usr/sbin/sshd`가 없으면 `apt-get install openssh-server` |
| `Install OpenSSH server (Windows)` | Windows | `%WINDIR%\System32\OpenSSH\sshd.exe`가 없으면 `Add-WindowsCapability`로 OpenSSH 서버를 설치 |
| `Check sshd for the SSH tests` | 세 OS | `scripts/prepare-test-sshd.sh`: sshd 경로·판을 `sshd-prep ...` 줄로 남기고, 시험 도우미와 같은 설정을 `sshd -t`로 검사 |

- 설치 단계는 `continue-on-error`다. 설치가 안 되거나 sshd가 시험에 쓸 수 없으면 시험이 시험 전용 Rust SSH 서버로 돈다. 둘 다 안 되면 시험이 `sshd=none`으로 실패한다. 준비 단계가 시험을 통과시키거나 건너뛰게 하지 않는다.
- 어느 서버로 돌았는지는 시험 단계 로그의 `[ssh_attach] sshd=openssh|russh|none` 줄(역할 반전 시험은 `[ssh_reverse_attach] sshd=...`)로 본다. 이 줄과 PID 줄은 시험이 통과해도 stderr에 남는다([예제](../examples.md) "통합 시험").
- macOS는 기본 `/usr/sbin/sshd`를 쓰고 설치 단계가 없다.

## `ssh -G` 대조의 기준 클라이언트

`tasty-net-ssh-config`의 `tests/matches_openssh.rs`는 이식 기준 커밋(OpenSSH portable 10.6p2)에서 빌드한 `ssh`와 출력을 대조한다([SSH 백엔드](../design/ssh-backends.md#비교-시험의-기준-클라이언트)).

| 단계 | OS | 하는 일 |
|---|---|---|
| `Cache OpenSSH reference client` | Linux·macOS | `target/tools/openssh-10.6p2`를 기준 커밋(`d449e662`)·OS·아키텍처·`scripts/install-openssh-ref.sh` 내용을 키로 캐시한다 |
| `Build OpenSSH reference client (Linux)` | Linux | 캐시가 없으면 autoconf·OpenSSL·zlib·Kerberos 개발 패키지를 받고 `scripts/install-openssh-ref.sh`로 빌드한다 |
| `Build OpenSSH reference client (macOS)` | macOS | 캐시가 없으면 brew로 autoconf·openssl@3을 받고 같은 스크립트로 빌드한다 |

- 시험 단계는 Linux·macOS에서 `TASTY_OPENSSH_REF_REQUIRED=1`로 돈다. 그래서 기준 클라이언트가 없으면 대조 시험이 실패한다. 그 변수가 없는 로컬 실행은 대조를 건너뛰고 그 사실을 한 줄로 알린다. Windows는 대조하지 않는다.
- 시험은 `PATH`의 ssh를 쓰지 않는다. `ssh -V`가 `OpenSSH_10.6p2`로 시작하지 않으면 실패한다.

## 툴체인

잡마다 `scripts/install-toolchain.sh`가 `rust-toolchain.toml`의 채널을 rustfmt·clippy와 함께 설치한다. `RUSTUP_AUTO_INSTALL=0`이라 그 밖의 툴체인을 몰래 받지 않는다.

## 동작 수준 기준과의 관계

- D1(세 OS CI)은 이 워크플로 실행 하나의 `fmt` 잡과 `clippy-test` 세 잡의 결론으로 본다. P2P 예제 두 프로세스 부분은 `clippy-test` 세 잡의 `p2p example pair` 단계 결론으로 본다. 이 단계는 서버 실행 파일과 `p2p_peer` 예제를 빌드해 루프백에 서버 하나와 예제 두 개를 띄우고, echo와 세 프로세스의 종료를 확인한다.
- D2(SSH 정방향 attach)는 `clippy-test` 세 잡의 시험 단계 로그에서 `ssh_attach` 시험마다의 `test <이름> ... ok` 줄과 `[ssh_attach] sshd=` 줄로 본다.
- D3(SSH 역할 반전 attach)는 같은 로그에서 `ssh_reverse_attach` 시험마다의 `test <이름> ... ok` 줄과 `[ssh_reverse_attach] sshd=` 줄로 본다.
- D7(tasty 의존 없음)은 `repo-checks` 잡의 `no-tasty-deps` 단계 결론으로 본다.
- D8(P2P 크레이트 격리)의 격리 부분은 `repo-checks` 잡의 `p2p-isolation` 단계 결론으로 본다. 단독 실행 부분은 `clippy-test` 세 잡의 `test tasty-net-rendezvous alone`·`test tasty-net-p2p alone` 단계 로그로 본다. 의존 그래프는 OS와 무관하게 `cargo metadata` 하나로 정해지므로(선택 의존과 모든 target을 넣어 읽는다) ubuntu 잡 하나에서 돈다.
- D10(설정값 노출)의 정적 검사 부분은 `repo-checks` 잡의 `no-hidden-constants` 단계 결론으로 본다. 소스와 설계 문서만 읽으므로 ubuntu 잡 하나에서 돈다. 대상·허용 목록·표 대응은 `scripts/no-hidden-constants.list` 한 파일이다.
- D5 (a)(rendezvous 설정 기동)는 `rendezvous.yml`의 `rendezvous-check` 잡 결론과 로그의 `rendezvous-check result=ok` 줄로 본다.
- D5 (b)(이미지로 띄운 서버로 두 기기 연결)는 CI 잡이 아니다. 원격 빌드 환경에서 돌린 `bash rendezvous/container-check.sh`의 종료 코드와 출력(`rendezvous-check result=ok` 줄, 예제의 연결 정보 줄, 실행 sha)으로 본다. 증거는 `image=built peer=built`인 실행이다(이미지나 예제를 덮어쓴 실행은 `given`으로 찍힌다, [rendezvous 패키지 안내](../../rendezvous/README.md#연결-확인)).
- 기준과 증거 형식의 정본은 [동작 수준 기준](../design/done-criteria.md)이다.

## cargo deny

- `deny.toml`의 licenses·bans·sources를 본다. 라이선스 허용 목록은 tasty `deny.toml`과 같고, `SSH-short`(OpenSSH 이식 크레이트 `tasty-net-ssh-config`) 하나만 더 있다. git 의존과 crates.io 밖의 레지스트리는 받지 않는다.
- advisories는 보지 않는다. 새 보안 공지는 커밋과 무관하게 나와 바뀐 것이 없는 커밋의 실행도 실패시키기 때문이다. 외부 의존(quinn·rustls 등)이 늘어나는 작업에서 따로 잡을 둘지 정한다.
- cargo-deny는 공식 릴리스 아카이브를 받아 워크플로에 고정한 SHA-256과 대조한 뒤 쓴다.

## shellcheck

`scripts/install-shellcheck.sh`가 러너에 shellcheck가 없을 때만 고정 판(SHA-256 대조)을 잡의 임시 폴더에 받는다.
