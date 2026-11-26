# CI와 시험 환경

## CI 러너

GitHub 호스트 러너(`ubuntu-latest`·`macos-latest`·`windows-latest`)를 쓴다.

- 공개 레포라 호스트 러너를 쓸 수 있다. 러너를 등록할 머신 작업이 없다.
- SSH 통합 시험은 시험 안에서 sshd를 띄우고, relay 시험은 포트를 연다. 공유 빌드 머신의 self-hosted 러너보다 일회용 VM에서 하는 편이 다른 작업과 겹치지 않는다.
- 세 OS가 모두 기본으로 제공된다. 이 레포의 기준(세 OS CI 통과)을 같은 실행 하나로 판정할 수 있다.

workflow 파일:

| 파일 | 잡 |
|---|---|
| `.github/workflows/ci.yml` | fmt(한 OS), clippy·test(세 OS 매트릭스, `fail-fast: false`), SSH·P2P 통합 시험 준비 단계 |
| `.github/workflows/relay.yml` | relay 설정 기동 검사(Linux) |

CI 밖에서 돌리는 시험(컨테이너 relay, 무거운 반복 측정)은 프로젝트의 원격 빌드 환경에서 한다. 그 환경의 위치는 이 레포에 적지 않는다. 결과는 같은 sha 기준으로 [동작 수준 기준](done-criteria.md)의 증거 표에 옮긴다.

## SSH 통합 시험의 sshd

시험은 시스템 `ssh` 클라이언트를 그대로 쓴다. 이 레포의 SSH 크레이트가 하는 일이 시스템 `ssh` 실행이기 때문이다. 서버 쪽만 시험이 띄운다.

### 기본: 비특권 OpenSSH sshd (Linux·macOS)

전제: root가 아닌 sshd도 사전 인증 모니터와 자식 분리는 그대로 한다. 건너뛰는 것은 root가 있어야 하는 chroot와 UID 변경이다(OpenSSH Portable `sshd-session.c`의 privilege separation 자식 처리). 그래서 다른 사용자로 전환하지 못하고, sshd를 띄운 사용자 자신으로만 로그인할 수 있다. 시험은 그 사용자의 키로 같은 사용자에게 로그인하고 `-L` 포워딩만 쓴다. 러너 이미지에 sshd가 없으면 준비 단계에서 설치한다(Linux 패키지, macOS는 기본 `/usr/sbin/sshd`).

OS별 성립 근거와 대체:

| OS | 성립 근거(확인할 것) | 성립하지 않을 때 |
|---|---|---|
| Linux(`ubuntu-latest`) | 준비 단계가 `sshd -t -f <설정>`으로 설정을 검사하고, 시험 도우미가 준비 확인 뒤 `ssh -o BatchMode=yes` 로그인과 `-L` 포워딩 한 번을 확인한다 | 아래 Windows 2의 시험 전용 Rust SSH 서버로 돈다(`sshd=russh`) |
| macOS(`macos-latest`) | Linux와 같다. 시스템 sshd를 사용자 권한으로 전경 실행한다 | Linux와 같다 |

CI 첫 실행의 결과(어느 OS가 어느 서버로 돌았는지)를 이 절에 적는다. OpenSSH도 대체 서버도 돌지 못한 OS의 SSH 통합 시험은 실패로 끝나며 미실행·통과로 바꾸지 않는다.

시험 도우미(`crates/tasty-net-demo/tests/support/`)가 임시 디렉터리에 아래를 만들고 `sshd -D -e -f <설정>`을 띄운다.

- 호스트 키: `ssh-keygen -t ed25519 -N ''`
- 사용자 키와 `authorized_keys`
- 설정: 빈 포트, `ListenAddress 127.0.0.1`, `HostKey`, `AuthorizedKeysFile`, `PidFile`, `StrictModes no`, `UsePAM no`, `AllowTcpForwarding yes`
- 클라이언트: `SshTarget.identity_file`과 `extra_options`로 `-o UserKnownHostsFile=<임시>`·`-o StrictHostKeyChecking=accept-new`

시험은 준비 확인(포트가 열렸는지) 뒤 시작하고, 끝나면 sshd를 PID로 끝내고 임시 디렉터리를 지운 뒤 자식 프로세스가 남지 않았는지 확인한다. sshd가 없으면(`sshd` 경로를 못 찾음) 시험은 통과가 아니라 실패로 끝난다. CI는 준비 단계에서 sshd가 있는지 확인한다.

### Windows

1. GitHub Windows 러너에서 OpenSSH 서버(`sshd.exe`)를 준비해 같은 방식으로 띄운다. 러너는 관리자 권한이라 설치 단계를 둘 수 있다. 사용자 계정 아래에서 서비스가 아닌 전경 실행이 되는지는 CI 첫 실행으로 판정한다.
2. 1이 안 되면 대체: 시험 전용 Rust SSH 서버(`russh` 크레이트, dev-dependency)가 `direct-tcpip`(로컬 포워딩)을 받는 최소 서버를 시험 도우미에 둔다. 클라이언트는 여전히 시스템 `ssh.exe`다. 역할 반전도 `-L`만 쓰므로 이 대체로 충분하다.
3. 어느 쪽을 썼는지 시험 로그에 한 줄로 남긴다(`sshd=openssh` 또는 `sshd=russh`).

1과 2가 모두 안 되면 Windows의 SSH 통합 시험은 "미실행"이고, 미실행은 통과로 세지 않는다([동작 수준 기준](done-criteria.md) "판정 규칙").

## P2P 통합 시험의 relay

CI의 P2P 시험은 iroh `test-utils`의 프로세스 안 relay(`run_relay_server`)를 시험 프로세스에서 띄우고, 별도 OS 프로세스로 띄운 `p2p-peer` 두 개가 그 relay에 붙는다. 외부 네트워크가 필요 없다. 실제 relay 프로세스(컨테이너)에 붙는 확인은 원격 빌드 환경에서 한다.

## 공통 규칙

- 고정 포트를 쓰지 않는다. 빈 포트를 잡는다.
- 시험이 띄운 프로세스와 임시 파일은 시험 안에서 정리하고 정리를 확인한다.
- heartbeat 같은 긴 시간 값은 연결별 설정으로 짧게 준다.
- 문서는 시험이 무엇을 확인하는지 요약만 한다. 정확한 조건은 시험 파일이 정본이다.
