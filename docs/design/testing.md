# CI와 시험 환경

## CI 러너

GitHub 호스트 러너(`ubuntu-latest`·`macos-latest`·`windows-latest`)를 쓴다.

- 공개 레포라 호스트 러너를 쓸 수 있다. 러너를 등록할 머신 작업이 없다.
- SSH 통합 시험은 시험 안에서 sshd를 띄우고, P2P 시험은 루프백 포트에 rendezvous 서버를 띄운다. 공유 빌드 머신의 self-hosted 러너보다 일회용 VM에서 하는 편이 다른 작업과 겹치지 않는다.
- 세 OS가 모두 기본으로 제공된다. 이 레포의 기준(세 OS CI 통과)을 같은 실행 하나로 판정할 수 있다.

workflow 파일:

| 파일 | 잡 |
|---|---|
| `.github/workflows/ci.yml` | fmt(한 OS), clippy·test(세 OS 매트릭스, `fail-fast: false`), SSH·P2P 통합 시험 준비 단계, P2P 크레이트 단독 시험·격리 검사, 설정값 정적 검사 |
| `.github/workflows/rendezvous.yml`(후속, 지금의 `relay.yml`을 바꾼다) | rendezvous 서버 설정 검사와 STUN 응답 확인(Linux) |

CI 밖에서 돌리는 시험(같은 망의 실제 서버 확인, 무거운 반복 측정)은 프로젝트의 원격 빌드 환경에서 한다. 그 환경의 위치는 이 레포에 적지 않는다. 결과는 같은 sha 기준으로 [동작 수준 기준](done-criteria.md)의 증거 표에 옮긴다.

## SSH 통합 시험의 sshd

시험은 시스템 `ssh` 클라이언트를 그대로 쓴다. 이 레포의 SSH 크레이트가 하는 일이 시스템 `ssh` 실행이기 때문이다. 서버 쪽만 시험이 띄운다.

### 기본: 비특권 OpenSSH sshd (Linux·macOS)

전제: root가 아닌 sshd도 사전 인증 모니터와 자식 분리는 그대로 한다. 건너뛰는 것은 root가 있어야 하는 chroot와 UID 변경이다(OpenSSH Portable `sshd-session.c`의 privilege separation 자식 처리). 그래서 다른 사용자로 전환하지 못하고, sshd를 띄운 사용자 자신으로만 로그인할 수 있다. 시험은 그 사용자의 키로 같은 사용자에게 로그인하고 `-L` 포워딩만 쓴다. 러너 이미지에 sshd가 없으면 준비 단계에서 설치한다(Linux 패키지, macOS는 기본 `/usr/sbin/sshd`).

OS별 성립 근거와 대체:

| OS | 성립 근거(확인할 것) | 성립하지 않을 때 |
|---|---|---|
| Linux(`ubuntu-latest`) | 준비 단계가 `sshd -t -f <설정>`으로 설정을 검사하고, 시험 도우미가 준비 확인 뒤 `ssh -o BatchMode=yes` 로그인과 `-L` 포워딩 한 번을 확인한다 | `sshd`가 없거나, 설정 검사·준비 확인이 실패하면 아래 Windows 2의 시험 전용 Rust SSH 서버로 돈다(`sshd=russh`) |
| macOS(`macos-latest`) | Linux와 같다. 시스템 sshd를 사용자 권한으로 전경 실행한다 | Linux와 같다 |

CI 첫 실행의 결과(어느 OS가 어느 서버로 돌았는지)를 이 절에 적는다. OpenSSH도 대체 서버도 돌지 못한 OS의 SSH 통합 시험은 실패로 끝나며(로그 `sshd=none`) 미실행·통과로 바꾸지 않는다. 세 OS 모두 같다. 그 OS의 D2·D3는 실행 상태 "실패", 판정 "미충족"으로 기록한다.

시험 도우미(`crates/tasty-net-demo/tests/support/`)가 임시 디렉터리에 아래를 만들고 `sshd -D -e -f <설정>`을 띄운다.

- 호스트 키: `ssh-keygen -t ed25519 -N ''`
- 사용자 키와 `authorized_keys`
- 설정: 빈 포트, `ListenAddress 127.0.0.1`, `HostKey`, `AuthorizedKeysFile`, `PidFile`, `StrictModes no`, `UsePAM no`, `AllowTcpForwarding yes`
- 클라이언트: `SshTarget.identity_file`과 `extra_options`로 `-o UserKnownHostsFile=<임시>`·`-o StrictHostKeyChecking=accept-new`

시험은 준비 확인(포트가 열렸는지) 뒤 시작하고, 끝나면 sshd를 PID로 끝내고 임시 디렉터리를 지운 뒤 자식 프로세스가 남지 않았는지 확인한다. sshd가 없거나(`sshd` 경로를 못 찾음) 준비 확인이 실패하면 시험 도우미는 대체 서버로 넘어간다(위 표). 대체 서버도 시작하지 못하면 시험은 통과가 아니라 실패로 끝난다. CI는 준비 단계에서 sshd가 있는지 확인하고 결과를 로그에 남긴다.

### Windows

1. GitHub Windows 러너에서 OpenSSH 서버(`sshd.exe`)를 준비해 같은 방식으로 띄운다. 러너는 관리자 권한이라 설치 단계를 둘 수 있다. 사용자 계정 아래에서 서비스가 아닌 전경 실행이 되는지는 CI 첫 실행으로 판정한다.
2. 1이 안 되면 대체: 시험 전용 Rust SSH 서버(`russh` 크레이트, dev-dependency)를 시험 도우미에 둔다. 받아야 하는 요청은 두 가지다. `direct-tcpip`(로컬 포워딩)과 `exec`(포트 발견 명령. 받은 명령을 시험 사용자 권한으로 OS 셸에서 실행하고 stdout·종료 코드를 돌려준다). 서버 재시작 뒤 새 인스턴스의 연결 레코드를 다시 발견해 연결하는 경로(D2 `reconnect_after_server_restart`)가 `exec`에 기대기 때문이다. 그 시험은 재연결이 새로 발견한 레코드(재시작 전과 다른 `instance_id`)로 이뤄졌는지 확인하고, 아니면 실패한다. 그래서 `exec`를 못 하는 서버에서는 그 시험이 통과로 나오지 않는다. 클라이언트는 여전히 시스템 `ssh`다. 역할 반전도 `-L`만 쓰므로 포워딩은 `direct-tcpip`로 충분하다. 다른 OS에서 같은 대체를 쓸 때도 이 두 요청을 같게 받는다.
3. 어느 쪽을 썼는지 시험 로그에 한 줄로 남긴다(`sshd=openssh`, `sshd=russh`, 둘 다 안 되면 `sshd=none`).

1과 2가 모두 안 되면 Windows의 SSH 통합 시험도 다른 OS와 같이 실패(`sshd=none`)로 끝나고 기준은 미충족이다([동작 수준 기준](done-criteria.md) "판정 규칙").

## P2P 시험

CI의 P2P 시험은 외부 네트워크를 쓰지 않는다. rendezvous 서버는 시험 프로세스 안에서 `tasty-net-rendezvous` 라이브러리로 루프백의 빈 포트에 띄운다. NAT는 모의 NAT로 흉내 낸다.

### 크레이트 단독

사용자 결정(2026-11-27): P2P 크레이트는 단독으로 시험과 예제가 돌아야 한다.

- `cargo test -p tasty-net-p2p --locked`와 `cargo test -p tasty-net-rendezvous --locked`가 각각 다른 workspace 크레이트의 시험 도구 없이 통과해야 한다(의존 범위는 [크레이트 구성](crates.md#p2p-크레이트-격리)).
- 크레이트 안의 두 프로세스 시험은 시험 바이너리가 자기 자신을 자식 프로세스로 다시 실행해 두 번째 기기를 만든다. 서버는 부모 시험 프로세스 안에 있다.
- 예제: `cargo run -p tasty-net-rendezvous --bin tasty-rendezvous -- --config <예시 설정>`으로 서버를 띄우고, `cargo run -p tasty-net-p2p --example p2p_peer -- --listen ...`과 `-- --dial <기기 ID> ...` 두 프로세스가 펀칭·연결·echo를 한다. 예제는 같은 머신 루프백과 같은 망의 두 기기에서 돈다. 예제가 빌드되는지는 D1의 clippy(`--all-targets`)가 보고, 두 프로세스 실행은 D4 (a)의 `example_pair_punches_and_echoes` 시험이 예제 바이너리를 빌드해 띄우는 방식으로 본다.

### 모의 NAT

- P2P 크레이트의 패킷 입출력은 trait(`PacketIo`: 보내기·받기·로컬 주소) 뒤에 있다. 기본 구현은 OS UDP 소켓이고, 시험은 프로세스 안 모의 NAT 구현을 `P2pConfig`로 넣는다.
- 모의 NAT는 내부 주소마다 바깥 포트를 정하는 매핑 규칙(EIM, APDM)과 들어오는 패킷을 거르는 필터 규칙(endpoint-independent, address and port-dependent)을 가진다([RFC 4787](https://www.rfc-editor.org/rfc/rfc4787) §4.1·§5). "나가는 UDP 막힘"도 하나의 규칙이다. 매핑 만료 시간도 설정값이라 keepalive 시험에 쓴다.
- 서버는 모의 망의 "공인" 쪽에 둔다. 두 번째 서버 IP 시험은 Linux에서 127.0.0.2를 쓰고, 다른 OS는 모의 망 주소로만 본다.

### 시험 목록 (요약, 정확한 조건은 시험 파일이 정본)

| 크레이트·파일 | 시험 | 확인하는 것 |
|---|---|---|
| `tasty-net-p2p` `tests/punch.rs` | `eim_pair_punches`, `same_lan_uses_host_candidate`, `one_sided_dependent_uses_peer_reflexive`, `dependent_pair_is_refused_without_probes`, `blocked_udp_fails_after_all_attempts` | NAT 조합별로 펀칭이 서거나 정해진 사유로 끝나고, 시도 횟수·시간이 설정값을 따름 |
| 같은 파일 | `declined_peer_gets_no_address`, `peer_on_other_server_is_not_found`, `disabled_endpoint_sends_nothing`, `self_dial_is_rejected_locally`, `identity_mismatch_fails_handshake`, `unpaired_quic_is_closed_with_not_paired` | 정책: 페어링 거절, 다른 서버, 꺼짐(보낸 패킷 0), 자기 연결, 키 확인 |
| `tasty-net-p2p` `tests/lifecycle.rs` | `server_reconnect_follows_backoff`, `superseded_endpoint_stops`, `keepalive_keeps_mapping_alive`, `remote_address_change_is_recorded`, `state_events_follow_the_tables` | 재연결, 밀어내기, keepalive, 경로 이동 기록, 상태 기계 전이와 이벤트 원인 |
| `tasty-net-p2p` `tests/two_process.rs` | `two_process_punch_and_echo`, `example_pair_punches_and_echoes` | 별도 OS 프로세스 두 개(자기 재실행, 예제 바이너리)의 펀칭·연결 |
| `tasty-net-rendezvous` `tests/server.rs` | `server_never_forwards_device_bytes`, `stun_answers_only_the_sender`, `access_policy_rejects`, `superseded_registration_is_closed`, `limits_return_busy`, `punch_session_expires` | 중계 없음, 반사, 접근 정책, 상태 지우기, 상한 |
| `tasty-net-transport` `tests/p2p_adapter.rs` | `p2p_ordered_stream_preserves_mixed_tag_order`, `rejected_first_stream_ack_is_read`, `bidirectional_last_frames_are_read` | 어댑터 위 채널 계약([전송 추상화](transport.md#p2p의-닫기-범위)) |
| `tasty-net-demo` `tests/p2p_attach.rs` | `p2p_attach_echo`, `unpaired_peer_is_refused`, `bulk_transfer_matches_bytes`, `connection_table_reports_p2p` | transport 위 P2P 링크를 별도 OS 프로세스 두 개로 |

### 실제 망 확인

- 사용자 결정(2026-11-27, Q1): 펀칭 실험용 기기·망·공인 서버는 hub rendezvous 배포 때 함께 준비한다. 그 전에는 같은 망과 로컬 서버로만 확인한다.
- 같은 망 확인: 원격 빌드 환경에서 `rendezvous/container-check.sh`가 이미지로 서버를 띄우고, 같은 망의 기기 두 대(또는 같은 머신의 두 프로세스)가 `p2p_peer` 예제로 붙는다(D5 (b)). 같은 공유기 안이므로 호스트 후보로 연결되는 경로를 본다.
- 다른 망 확인은 [P2P](p2p.md#펀칭-실험)의 실험이고 P1이다.

## 설정값 시험

[크레이트 구성](crates.md#설정값-규칙)의 규칙을 확인한다. 대상은 `tasty-net-p2p`와 `tasty-net-rendezvous`다. 다른 크레이트는 후속 작업으로 규칙을 맞출 때 같은 시험을 더한다.

- `settings_list_matches_query`(두 크레이트): 설정 모듈의 항목 목록과 `settings()` 조회 결과가 키 단위로 같고, 값을 설정하면 그 항목의 출처가 `configured`로, 안 하면 `default`로 나온다.
- `connection_reports_applied_settings`(p2p): 연결 정보의 `applied`가 그 연결에 쓰인 값과 같고, `RetryWait` 중에는 시도 번호와 남은 시간이 나온다.
- 정적 검사 `no-hidden-constants`(`scripts/check.sh`의 단계, 후속 작업이 만든다): 두 크레이트의 `src/`에서 설정 모듈과 와이어 형식 모듈 밖의 숫자 리터럴(0과 1 제외)과 `Duration` 생성 리터럴을 찾는다. 찾으면 실패한다. 시간·횟수가 아닌 값은 그 줄에 `// not-a-setting: <이유>`를 달아 뺀다. 시험 코드(`tests/`, `#[cfg(test)]`)는 보지 않는다.
- 설계 문서 표와의 대조: 같은 검사가 [P2P](p2p.md#설정값)·[rendezvous](rendezvous.md#서버-설정값) 표의 키 이름 집합과 코드의 항목 목록을 비교한다. 값은 비교하지 않는다(기본값의 정본은 코드다).

## 연결 목록 시험

[전송 추상화](transport.md#연결-목록)의 연결 목록이 세 경로에서 맞게 보고하는지 본다.

| 경로 | 시험 | 확인하는 것 |
|---|---|---|
| SSH 정방향 | `crates/tasty-net-demo/tests/ssh_attach.rs`의 `connection_table_reports_ssh_forward` | 기기, `ssh_forward`, 로컬 포워딩 포트 ↔ SSH 대상·원격 포트, `ssh` 경유, 상태 `connected`, 터널을 죽였을 때 `reconnecting` → `connected`와 원인 |
| SSH 역할 반전 | `ssh_reverse_attach.rs`의 `connection_table_reports_ssh_role_reversed` | 같은 항목과 holder, 받는 쪽의 `ssh_inbound` 항목 |
| P2P | `p2p_attach.rs`의 `connection_table_reports_p2p` | 기기, `p2p_udp`, 소켓 주소 ↔ 상대 주소, 소개한 서버 주소·공개키, 성공 후보 종류, 상태와 닫힌 뒤 원인 |
| 공통 | `tasty-net-transport` `tests/connection_table.rs`의 `history_is_capped`, `closed_entries_are_capped` | 이력·끊긴 항목 상한 |

## 공통 규칙

- 고정 포트를 쓰지 않는다. 빈 포트를 잡는다.
- 시험이 띄운 프로세스와 임시 파일은 시험 안에서 정리하고 정리를 확인한다.
- heartbeat 같은 긴 시간 값은 연결별 설정으로 짧게 준다.
- 문서는 시험이 무엇을 확인하는지 요약만 한다. 정확한 조건은 시험 파일이 정본이다.
