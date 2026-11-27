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
| Linux(`ubuntu-latest`) | 준비 단계(`scripts/prepare-test-sshd.sh`)가 `sshd -t -f <설정>`으로 설정을 검사해 로그에 남긴다. 시험 도우미도 띄우기 전에 같은 검사를 하고, 준비 확인 뒤 `ssh -o BatchMode=yes` 로그인과 `-L` 포워딩 한 번을 확인한다 | `sshd`가 없거나, 설정 검사·기동·준비 확인·로그인·포워딩 확인 중 하나가 실패하면 아래 Windows 2의 시험 전용 Rust SSH 서버로 돈다(`sshd=russh`) |
| macOS(`macos-latest`) | Linux와 같다. 시스템 sshd를 사용자 권한으로 전경 실행한다 | Linux와 같다 |

CI 첫 실행의 결과(어느 OS가 어느 서버로 돌았는지)를 이 절에 적는다. OpenSSH도 대체 서버도 돌지 못한 OS의 SSH 통합 시험은 실패로 끝나며(로그 `sshd=none`) 미실행·통과로 바꾸지 않는다. 세 OS 모두 같다. 그 OS의 D2·D3는 실행 상태 "실패", 판정 "미충족"으로 기록한다.

시험 도우미(`crates/tasty-net-demo/tests/support/`)가 임시 디렉터리에 아래를 만들고 `sshd -D -e -f <설정>`을 띄운다.

- 호스트 키: `ssh-keygen -t ed25519 -N ''`
- 사용자 키와 `authorized_keys`
- 설정: 빈 포트, `ListenAddress 127.0.0.1`, `HostKey`, `AuthorizedKeysFile`, `PidFile`, `StrictModes no`, `UsePAM no`, `AllowTcpForwarding yes`, `PubkeyAuthentication yes`, `PasswordAuthentication no`, `KbdInteractiveAuthentication no`. Windows에서는 `PidFile`·`UsePAM`을 넣지 않는다. sshd가 `PerSourcePenalties`를 알면(OpenSSH 9.8 이후) `PerSourcePenalties no`를 더한다. 같은 주소에서 연결을 자주 다시 여는 시험이 벌점으로 거절되지 않게 하려는 것이다. sshd가 이 줄을 모르면 `sshd -t`가 실패하므로 그 줄을 빼고 다시 검사한다.
- 클라이언트: `SshTarget.identity_file`과 `extra_options`로 `-o UserKnownHostsFile=<임시>`·`-o GlobalKnownHostsFile=<임시>`·`-o StrictHostKeyChecking=accept-new`·`-o IdentitiesOnly=yes`·`-o PreferredAuthentications=publickey`·`-o PasswordAuthentication=no`·`-o KbdInteractiveAuthentication=no`. 비밀번호 질문으로 시험이 멈추지 않게 한다.

준비 확인은 sshd가 stderr에 `Server listening on`을 남겼는지로 한다. 포트에 TCP로 붙어 보는 방식은 인증 없이 끊는 연결이 되어 `PerSourcePenalties`의 벌점 대상이 되므로 쓰지 않는다. 시험은 준비 확인 뒤 시작하고, 끝나면 클라이언트가 모두 끝난 뒤 sshd에 세션 자식 프로세스가 남지 않았는지 보고 sshd를 PID로 끝내며, 임시 디렉터리를 지운 뒤 자식 프로세스가 남지 않았는지 확인한다. sshd가 없거나(`sshd` 경로를 못 찾음) 준비 확인이 실패하면 시험 도우미는 대체 서버로 넘어간다(위 표). 대체 서버도 시작하지 못하면 시험은 통과가 아니라 실패로 끝난다. CI는 준비 단계에서 sshd가 있는지 확인하고 결과를 로그에 남긴다. 서버 선택은 `TASTY_NET_TEST_SSHD`(`auto` 기본, `openssh`, `russh`)로 고정할 수 있다. 도우미는 `tests/support/sshd.rs`(OpenSSH와 선택), `tests/support/russh_server.rs`(대체 서버)다.

알려진 한계: 이미 끝난 자식의 PID를 나중에 PID만으로 조회·종료하므로 OS가 번호를 재사용하면 무관한 프로세스를 볼 수 있다.
알려진 한계: 실패한 시험이 dialer를 끝낼 때 자식을 열거한 직후 dialer가 새 ssh를 띄우고 `ssh-spawned` 줄을 쓰기 전에 끝나면, 그 ssh의 PID는 얻지 못한다.

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
- 예제: `cargo run -p tasty-net-rendezvous --bin tasty-rendezvous -- --config <예시 설정>`으로 서버를 띄우고, `cargo run -p tasty-net-p2p --example p2p_peer -- --listen ...`과 `-- --dial <기기 ID> ...` 두 프로세스가 펀칭·연결·echo를 한다. 예제는 같은 머신 루프백과 같은 망의 두 기기에서 돈다. 예제가 빌드되는지는 D1의 clippy(`--all-targets`)가 본다.
- 예제 두 프로세스 실행은 `cargo test`의 시험이 아니라 검사 단계 `bash scripts/check.sh p2p-example-pair`(`scripts/check-p2p-example-pair.sh`)가 본다. 시험 바이너리는 예제 경로를 받을 길이 없기 때문이다(`CARGO_BIN_EXE_*`는 `[[bin]]`에만 생긴다, [크레이트 구성](crates.md#크레이트-표)). 이 단계는 `cargo build --locked -p tasty-net-rendezvous --bin tasty-rendezvous`와 `cargo build --locked -p tasty-net-p2p --example p2p_peer`를 `--message-format=json`으로 돌려 출력의 `executable` 값에서 두 실행 파일 경로를 얻는다. 그래서 OS마다 다른 경로(`target/debug/examples/p2p_peer.exe` 등)를 스크립트가 짐작하지 않는다. 그 뒤 서버 하나와 예제 두 개를 루프백에 띄워 echo를 확인하고, 셋 모두 끝났는지 확인한다. 예제는 `--timeout` 안에서 스트림 읽기·쓰기에 기한을 두고, 기한이 없는 단계에 막혀도 `--timeout`을 조금 넘기면 스스로 끝난다. 스크립트도 예제 두 개를 상한까지만 기다리고 넘으면 실패로 끝낸다(상한 대기는 자체 시험이 본다). CI는 세 OS의 clippy·test 잡에서 이 단계를 부른다.

### 모의 NAT

- P2P 크레이트의 패킷 입출력은 trait(`PacketIo`: 보내기·받기·로컬 주소) 뒤에 있다. 기본 구현은 OS UDP 소켓이고, 시험은 프로세스 안 모의 NAT 구현을 `P2pConfig`로 넣는다.
- 모의 NAT는 내부 주소마다 바깥 포트를 정하는 매핑 규칙(EIM, APDM)과 들어오는 패킷을 거르는 필터 규칙(endpoint-independent, address and port-dependent)을 가진다([RFC 4787](https://www.rfc-editor.org/rfc/rfc4787) §4.1·§5). "나가는 UDP 막힘"도 하나의 규칙이다. 매핑 만료 시간도 설정값이라 keepalive 시험에 쓴다.
- 서버는 모의 망의 "공인" 쪽에 둔다. 두 번째 서버 IP 시험은 Linux에서 127.0.0.2를 쓰고, 다른 OS는 모의 망 주소로만 본다.
- 올바른 서버는 보내지 않는 값(한계값을 벗어난 `punch_go`)이나 정한 때의 `punch_cancel`이 필요한 시험은 시험 안의 각본 서버를 쓴다. 각본 서버는 같은 조정 프로토콜을 모의 망 위에서 말하고 평문 STUN에 답하며, 기기 사이 바이트를 나르지 않는다(`tests/support/fake.rs`).

### 시험 목록 (요약, 정확한 조건은 시험 파일이 정본)

| 크레이트·파일 | 시험 | 확인하는 것 |
|---|---|---|
| `tasty-net-p2p` `src/` 단위 시험 | `demux_classifies_first_byte`, `nat_mapping_follows_stun_results`, `probe_response_needs_txid_address_and_token`, `received_candidates_are_refiltered` | 첫 바이트 분류 경계값, RFC 5780 비교 표(응답 누락·IP2 없음 포함), 프로브 응답의 세 조건, 받은 후보의 주소를 같은 규칙으로 다시 거름(입력: 허용 밖 주소가 섞인 상한 이하의 후보. 기대: 허용 밖 주소만 빠지고 버린 수가 세어짐). 후보 수가 상한을 넘는 입력은 이 시험이 아니라 `invalid_punch_go_is_reported_by_both`가 본다 |
| `tasty-net-rendezvous` `src/` 단위 시험 | `candidate_rule_matches_device_rule`, `candidates_follow_kind_priority_and_limit` | 서버와 기기가 같은 허용 규칙 함수를 쓰고 경계 주소(RFC 1918 끝, `2000::/3` 밖, 링크 로컬, 포트 0)를 같게 판정함, 종류별 우선순위·중복 제거·상한(입력: 서버 반사 1, 보고 반사 5, 호스트 7, 일부 중복. 기대: 서버 반사 → 보고 반사 3 → 호스트 4 순서의 8개. 서버 쪽 정상 절단이다) |
| `tasty-net-p2p` `tests/punch.rs` | `eim_pair_punches`, `same_lan_uses_host_candidate`, `one_sided_dependent_uses_peer_reflexive`, `dependent_pair_tries_host_candidates_only`, `dependent_pair_without_host_path_is_unsupported`, `peer_udp_blocked_fails_after_all_attempts`, `retry_supersedes_previous_session`, `server_unreachable_is_rendezvous_unavailable` | NAT 조합별로 펀칭이 서거나 정해진 사유로 끝나고, 시도 횟수·시간이 설정값을 따름. UDP 막힘은 둘로 나눈다: 서버에는 닿고 기기 사이만 막히면 시도를 다 쓴 뒤 `PunchFailed`, 서버에도 닿지 않으면 등록을 못 해 펀칭 없이 `RendezvousUnavailable` |
| 같은 파일 | `declined_peer_gets_no_address`, `peer_on_other_server_is_not_found`, `disabled_endpoint_sends_nothing`, `self_dial_is_rejected_locally`, `identity_mismatch_fails_handshake`, `unpaired_quic_is_closed_with_not_paired`, `quic_initial_without_session_is_refused`, `quic_from_other_device_gets_session_mismatch`, `second_session_for_same_pair_is_busy` | 정책: 페어링 거절, 다른 서버(`PeerNotFound`이고 상대가 붙은 다른 서버로 나간 패킷 0), 꺼짐(보낸 패킷 0), 자기 연결, 키 확인. 세션 밖 QUIC은 둘로 본다: 원격 주소가 어느 활성 세션에도 맞지 않는 Initial은 TLS 전에 `CONNECTION_REFUSED`(상대에 TLS handshake 기록 없음). 주소는 활성 세션(다른 상대와의 세션 포함) 하나 이상에 맞지만 그 주소에 맞는 세션 중 TLS가 확인한 기기 ID를 상대로 가진 것이 없으면 연결 뒤 종료 코드 4 `session_mismatch`. 같은 주소가 두 상대의 세션에 맞을 때 기기 ID로 맞는 세션을 고르는 경우도 본다. 진행 중 세션과 겹치는 반대 방향 요청은 `busy` |
| 같은 파일 | `punch_cancel_ends_the_attempt`, `go_timeout_closes_the_offer`, `request_timeout_is_retryable`, `rendezvous_lost_before_go`, `late_probe_success_still_gets_handshake_time`, `session_ttl_covers_acceptor_deadline` | 취소·기한 경로와 그 오류 값, 창 끝 직전 성공에도 B가 `handshake_ms` 동안 받음, 기본값이 아닌 서버 값(`quic_handshake_timeout` 10 s, `punch_session_grace` 1 s)과 B 쪽 통지 지연을 준 상태에서 B가 받기 마감 직전에 시작한 handshake가 끝나고 결과 보고가 올 때까지 `punch_cancel{expired}`가 오지 않음 |
| 같은 파일 | `retry_supersedes_previous_session`(위 행) | 첫 시도가 창으로 실패한 뒤 B가 옛 세션의 받기 기한 안에 있어도 A의 두 번째 `punch_request`가 `busy`가 아니라 새 세션이 되고, B는 옛 세션에 `punch_cancel{superseded}`를 받음. 두 시도를 다 쓴 결과는 `PunchFailed`이고 전체 시간이 기본값 상한 29.5 s 안 |
| 같은 파일 | `host_only_adds_no_peer_reflexive`, `host_only_exhaustion_is_nat_unsupported` | `host_only`에서 호스트 후보 밖 주소의 프로브는 응답·후보 추가 없이 세어지고, 시도를 다 써도 오류가 `PunchFailed`가 아니라 `NatUnsupported` |
| 같은 파일 | `acceptor_with_invalid_limits_declines_with_reason`, `invalid_punch_go_is_reported_by_both` | 한계값 두 단계: B의 `welcome.limits`가 어긋나면 B는 `limits_invalid`로 거절하고 A는 `PeerLimitsInvalid`. `punch_go`가 상한을 벗어나면(창·handshake 기한·시작 지연 초과, 또는 후보가 `punch.max_candidates`보다 많이 도착) 받은 쪽은 프로브 0개이고 A면 `ServerLimitsInvalid`, B면 `Closed{server_limits_invalid}`. 상대는 `punch_cancel{peer_limits_invalid}`를 `Scheduled`·`Punching`·`Handshaking` 각 단계에서 받는 경우 모두 재시도 없이 `Closed`(A면 `PeerLimitsInvalid`, B면 `Closed{peer_limits_invalid}`) |
| `tasty-net-p2p` `tests/lifecycle.rs` | `server_reconnect_follows_backoff`, `reconnect_delay_never_exceeds_max_delay`, `superseded_endpoint_stops`, `alpn_mismatch_stops_without_retry`, `server_key_mismatch_stops_without_retry`, `server_limits_mismatch_is_reported`, `keepalive_keeps_mapping_alive`, `client_remap_is_observed_on_acceptor`, `acceptor_remap_ends_by_idle_timeout`, `reflexive_change_triggers_rejudge`, `state_events_follow_the_tables` | 재연결(무작위 폭을 더해도 상한 안), 밀어내기, TLS 단계 불일치의 정지, 서버 한계값 불일치의 알림, keepalive, 주소 바뀜의 비대칭(A 쪽 재매핑은 B에 `remote_address_observed_change`로 남고, B 쪽 재매핑은 `idle_timeout`으로 끝남. 끝나는 시각은 설정값과 같다고 보지 않고, 원인이 `idle_timeout`인지와 마지막 타이머 재시작 시각(마지막 유효 수신 또는 그 뒤 첫 ack-eliciting 송신) + 유효 idle timeout + 여유 안에 끝나는지만 본다), `reflexive_changed` 뒤 재판별, 상태 기계 전이와 이벤트 원인 |
| `tasty-net-p2p` `tests/two_process.rs` | `two_process_punch_and_echo` | 별도 OS 프로세스 두 개(시험 바이너리 자기 재실행)의 펀칭·연결 |
| `tasty-net-rendezvous` `src/` 단위 시험 | `frame_length_bounds`, `partial_and_coalesced_frames`, `oversized_length_is_rejected_before_allocation`, `server_start_rejects_inconsistent_settings` | 길이 접두의 경계값(`max_message_bytes` 범위, u32 끝), 헤더·본문 부분 도착, 두 프레임이 붙어 도착, 도중 종료, 선언 길이 검사 전 할당 없음, 서버 설정 관계가 어긋나면 시작 거절 |
| `tasty-net-p2p` `tests/lifecycle.rs`(조정 프레임) | `first_frame_over_device_limit_is_rejected`, `first_frame_timeout_is_device_setting`, `frame_limits_after_welcome` | 기기 수신 상한·기한: `welcome` 전 첫 프레임은 기기 `rendezvous.max_message_bytes`·`rendezvous.frame_timeout`으로 거절되고(할당 전), 등록 뒤에는 보내는 프레임이 서버 `max_message_bytes` 이하이고 받는 프레임은 기기 값으로 검사되며, 서버 값이 기기 값보다 크면 `server_limits_mismatch` |
| `tasty-net-rendezvous` `tests/server.rs` | `server_never_forwards_device_bytes`, `unknown_fields_are_not_forwarded`, `second_stream_is_refused`, `stun_answers_only_the_sender`, `stun_rate_limit_per_source`, `access_policy_rejects`, `superseded_registration_is_closed`, `limits_return_busy`, `punch_session_expires`, `frame_timeout_closes`, `advertised_addresses_are_used`, `wrong_alpn_fails_in_tls`, `settings_are_logged_at_start` | 중계 없음(상대에게 가는 값이 [rendezvous](rendezvous.md#중계-없음)의 닫힌 표뿐), 반사와 출발 주소별 속도 제한, 접근 정책, 상태 지우기, 상한, 프레임 기한, 광고 주소, ALPN 불일치, 시작 때 설정 로그 |
| `tasty-net-transport` `tests/p2p_adapter.rs` | `p2p_ordered_stream_preserves_mixed_tag_order`, `rejected_first_stream_ack_is_read`, `bidirectional_last_frames_are_read` | 어댑터 위 채널 계약([전송 추상화](transport.md#p2p의-닫기-범위)) |
| `tasty-net-demo` `tests/p2p_attach.rs` | `p2p_attach_echo`, `unpaired_peer_is_refused`, `bulk_transfer_matches_bytes`, `connection_table_reports_p2p` | transport 위 P2P 링크를 별도 OS 프로세스 두 개로 |

### 실제 망 확인

- 사용자 결정(2026-11-27, Q1): 펀칭 실험용 기기·망·공인 서버는 hub rendezvous 배포 때 함께 준비한다. 그 전에는 같은 망과 로컬 서버로만 확인한다.
- 같은 망 확인: 원격 빌드 환경에서 `rendezvous/container-check.sh`가 이미지로 서버를 띄우고, 같은 망의 기기 두 대(또는 같은 머신의 두 프로세스)가 `p2p_peer` 예제로 붙는다(D5 (b)). 같은 공유기 안이므로 호스트 후보로 연결되는 경로를 본다.
- 다른 망 확인은 [P2P](p2p.md#펀칭-실험)의 실험이고 P1이다.

## 설정값 시험

[크레이트 구성](crates.md#설정값-규칙)의 규칙을 확인한다. 대상은 `tasty-net-p2p`, `tasty-net-rendezvous`, `tasty-net-transport`의 새 연결 목록·`Redialer` 값, 그리고 규칙을 맞춘 `tasty-net-identity`·`tasty-net-topology`·`tasty-net-ssh`다. 다른 크레이트·모듈은 후속 작업으로 규칙을 맞출 때 같은 시험을 더한다.

- `settings_list_matches_query`(두 크레이트, transport의 연결 목록 값, identity, topology, ssh): 설정 모듈의 항목 목록과 `settings()` 조회 결과가 키 단위로 같고, 값을 설정하면 그 항목의 출처가 `configured`로, 안 하면 `default`로, OS 값을 읽어 보이는 항목은 `os_default`로, 고정값은 `fixed`로 나온다. 고정값 키를 주면 `bind`가 모르는 키로 거절한다.
- `defaults_match_design_tables`(두 크레이트, transport, identity, topology, ssh): 기본값의 단일 원천인 코드의 기본값 표(`settings::DEFAULTS`)를 설계 문서 표와 값까지 비교한다. identity·topology·ssh는 표의 범위 칸도 비교한다. 시험은 기대값을 따로 복제하지 않고 레포의 설계 문서(`docs/design/p2p.md`·`rendezvous.md`·`transport.md`·`identity.md`·`topology.md`의 설정 표, `CARGO_MANIFEST_DIR` 기준 상대 경로)를 읽어 표를 파싱한다. 그래서 문서 숫자만 바뀌거나 코드 값만 바뀌면 실패한다. 비교 대상 표는 [P2P](p2p.md#설정값)의 "이 레포가 정하는 값"·"의존성이 노출하는 값"·"고정값", [rendezvous](rendezvous.md#서버-설정값), [전송 추상화](transport.md#보관-상한과-재연결-도우미)와 그 문서의 [SSH 크레이트 설정값](transport.md#ssh-크레이트-설정값), [기기 키·토큰](identity.md#설정값), [토폴로지](topology.md#설정값)다.
- `dependency_defaults_match_source`(p2p): 표에서 quinn·rustls 기본을 그대로 쓴다고 적은 값을 의존성의 기본 객체와 비교한다. quinn-proto 설정 타입은 대부분 getter가 없으므로, 기본 객체의 `Debug` 출력에서 필드 값을 읽는다. quinn-proto 0.11.19의 `Debug`에 빠진 필드(`EndpointConfig::min_reset_interval`, `TransportConfig`의 혼잡 제어기)는 고정판 소스에서 옮긴 값을 출처 판과 함께 시험 자료로 두고, 판을 올릴 때 다시 확인한다. rustls는 공개 필드를 직접 읽는다.
- `dependency_setters_are_covered`(p2p): 고정판(quinn 0.11.12·quinn-proto 0.11.19·rustls 0.23.46)의 공개 setter·공개 필드 목록(rustdoc JSON에서 뽑아 `tests/data/`에 커밋)에서 시간·횟수·크기·상한을 받는 것이 모두 [P2P](p2p.md#의존성이-노출하는-값) 표나 "고정값" 표에 있는지 본다. 판을 올리면 목록을 다시 뽑는다.
- `dependency_settings_are_applied`(p2p): 의존성이 노출하는 값을 바꾸면 그 값이 실제 quinn·rustls 설정에 들어간다(예: `quic.initial_rtt`, `quic.max_uni_streams` 0이면 상대가 연 단방향 스트림이 거절됨). 고정값(`grease_quic_bit` 끔, 재개·티켓·0-RTT 끔)이 실제 설정에 들어갔는지도 본다.
- `connection_reports_applied_settings`(p2p): 연결 정보의 `applied`가 그 연결에 쓰인 값과 같고, 서버가 정한 값(`window_ms`·`handshake_ms`·`start_after_ms`·한계값)의 출처는 `server`이며, `RetryWait` 중에는 시도 번호와 남은 시간이 나온다. 서버를 운영자 설정 값으로 띄운 경우에도 기기 쪽 출처는 `server`이고, 같은 값의 `configured` 출처는 서버 `settings()`에 보인다.
- `ssh_connection_reports_applied_sources`(transport): SSH 항목의 `applied`가 `LinkTimeouts`·`redial.*`를 덮어쓴 값은 `configured`, 아닌 값은 `default`로 보인다.
- `the_ssh_options_come_from_the_settings`·`applied_options_show_which_value_wins`(ssh): 설정을 바꾸면 런처가 만드는 ssh 인자의 `ServerAliveInterval`·`ServerAliveCountMax`·`ConnectTimeout`이 바뀌고, `applied_options`가 ssh가 실제로 쓰는 값(앞의 둘은 설정값, `ConnectTimeout`은 대상 옵션이 있으면 그 값)과 출처(`default`·`configured`·`target`)를 보인다. 포트 발견·터널·`Backoff`가 같은 설정을 쓰는지는 `the_output_limit_comes_from_the_settings`·`the_step_bound_comes_from_the_settings`·`the_total_budget_comes_from_the_settings`·`a_slow_failure_is_judged_by_the_connect_timeout_in_effect`·`the_ready_timeout_comes_from_the_settings`·`the_line_limit_comes_from_the_settings`·`backoff_takes_its_settings`가 본다.
- 정적 검사 `no-hidden-constants`(`scripts/check.sh no-hidden-constants`, 대상·허용 목록·표 대응은 `scripts/no-hidden-constants.list`): 두 크레이트의 `src/`, `tasty-net-transport`의 연결 목록·`Redialer`·링크 ID 소스(`src/connection_table/`, `src/redial.rs`, `src/link_id.rs`), `tasty-net-identity`·`tasty-net-topology`·`tasty-net-ssh`의 `src/`에서 설정 모듈 밖의 숫자 리터럴과 `Duration` 생성 리터럴을 모두 찾는다. `Duration`의 별칭(`use … as`, `type … =`, 우변을 감싼 괄호는 벗겨 본다)과 별칭의 별칭도 따라간다. 알려진 한계: 별칭 이름은 파일 전체에서 하나로 보고 모듈별 범위를 따지지 않으므로, 같은 파일의 다른 모듈에 같은 이름의 다른 타입이 있으면 그 상수도 위반으로 나온다(오탐은 위반으로 보여 사람이 보게 되므로 놓침보다 안전하다). 정수 타입의 연관 상수 `MIN`·`MAX`(`usize::MIN`, `u16::MAX`, 한정 경로 `<usize>::MIN` 등)도 리터럴로 보고 같은 목록 규칙을 적용한다(0을 `usize::MIN`으로 써서 규칙을 비켜 가지 못하게 한다). 0과 1도 전역으로 빼지 않는다(재시도 한 번, 동시 처리 상한 1 같은 값을 놓치지 않기 위해서다). 허용은 목록(allowlist) 하나로만 한다. 목록의 각 줄은 파일·리터럴·이유이고(예: 와이어 형식 모듈의 STUN 헤더 길이 20, 배열 첨자 0), 목록에 없는 리터럴이 나오면 실패한다. 이름 있는 선언(`const`·`static`)의 허용 줄은 이름 뒤 선언 전체(타입과 값 식, 연산자·부호·괄호 포함, 공백은 정규화하고 `>>`는 닫는 꺾쇠 둘로 본다)를 적고, 코드가 하나라도 다르면 실패한다(`1 << 16`이 `1 + 16`이 되거나 `[CloseCode; 7]`이 `[CloseCode; 7 + 0]`이 되는 것처럼 숫자는 같고 값이 바뀌는 경우를 잡는다). `enum`은 판별식을 차례로 적는다. 와이어 형식 모듈도 통째로 빼지 않고, 그 안의 상수도 목록에 하나씩 올린다([P2P](p2p.md#와이어-형식-상수-설정-아님) 표와 같은 값만). 시험 코드(`tests/`, `#[cfg(test)]` 항목, `#[cfg(test)]`로 선언한 파일 모듈과 그 아래 모듈 파일)는 보지 않는다.
- 이 검사 자체의 시험: 검사 스크립트의 시험 입력에 숨은 값(예: `retries = 1`, `max_inflight = 1`, `Duration::from_millis(0)`)을 둔 파일과 목록에 오른 값만 둔 파일을 두고, 앞의 것은 실패하고 뒤의 것은 통과하는지 본다.
- 설계 문서 표와의 대조: 같은 검사가 [P2P](p2p.md#설정값)·[rendezvous](rendezvous.md#서버-설정값)·[전송 추상화](transport.md#보관-상한과-재연결-도우미)·[SSH 크레이트 설정값](transport.md#ssh-크레이트-설정값)·[기기 키·토큰](identity.md#설정값)·[토폴로지](topology.md#설정값) 표의 키 이름 집합과 코드의 항목 목록을 비교한다. 값의 대조는 위 `defaults_match_design_tables`가 한다.

## 연결 목록 시험

[전송 추상화](transport.md#연결-목록)의 연결 목록이 세 경로에서 맞게 보고하는지 본다.

| 경로 | 시험 | 확인하는 것 |
|---|---|---|
| SSH 정방향 | `crates/tasty-net-demo/tests/ssh_attach.rs`의 `connection_table_reports_ssh_forward` | 기기, `path = ssh_forward`·`direction = forward`, 구간 셋(`local_forward`·`ssh`·`remote_listener`)의 값, `ssh` 경유이고 rendezvous 없음, 상태 `connected`, 한 터널의 attach·bulk·요청 채널이 항목 하나 안에 있음. 터널을 죽였을 때 옛 항목은 `disconnected`와 원인, 새 항목은 새 ID·`previous` = 옛 ID·`reconnecting` → `connected`. 받는 쪽에는 `ssh_inbound`·`direction = forward`·`loopback` 구간의 항목(채널마다, bulk는 `parent`) |
| SSH 역할 반전 | `ssh_reverse_attach.rs`의 `connection_table_reports_ssh_role_reversed` | 같은 항목 모양에서 `path = ssh_role_reversed`·`direction = role_reversed`. 받는 쪽 항목은 `ssh_inbound`·`direction = role_reversed` |
| P2P | `p2p_attach.rs`의 `connection_table_reports_p2p` | 기기, `p2p_udp`, `udp` 구간의 `local_bind`·`local_ip` ↔ `remote_addr`. A(dialer) 쪽은 `local_bind`의 포트와 `local_reflexive`가 채워졌는지 보고 `local_ip`는 `unknown`인 것을 기대값으로 둔다(quinn 클라이언트 한계). B 쪽 `local_ip`는 플랫폼이 목적지 IP를 줄 때 채워진다, 소개한 서버 주소·공개키, 성공 후보 종류, 상태와 닫힌 뒤 원인 |
| 토폴로지 입력 | `tasty-net-demo` `tests/ssh_reverse_attach.rs`의 `both_ends_produce_matching_local_links`, `mixed_holders_on_one_tunnel` | 같은 SSH 링크의 양 끝 연결 목록에서 만든 `LocalLink`의 `link_id`·`dialer`·`acceptor`·`holder`·`transport`가 같음. 한 터널 위에 정방향 링크와 역할 반전 링크를 함께 열면 SSH를 띄운 쪽 항목의 `direction`은 `mixed`이고, 링크마다 양 끝의 `transport`가 같음(`ssh_forward` 하나, `ssh_role_reversed` 하나). demo는 topology에 의존하지 않으므로 집계는 하지 않고 입력만 비교한다 |
| 토폴로지 합치기 | `tasty-net-topology` `tests/merge.rs`의 `both_ends_report_one_edge`, `parallel_attaches_to_one_workspace_stay_apart`, `ends_with_different_exposure_merge_into_one` | 같은 `link_id`의 양 끝 보고는 엣지 하나, 같은 workspace의 다른 surface에 붙은 두 링크는 엣지 둘, 한쪽만 `workspace`를 실은 두 보고는 그 값을 가진 엣지 하나 |
| 공통 | `tasty-net-transport` `tests/connection_table.rs`의 `history_is_capped`, `closed_entries_are_capped`, `event_queue_reports_lag`, `ids_are_stable_and_never_reused`, `redialer_follows_settings` | 이력·끊긴 항목·이벤트 큐 상한, ID 안정성, `Redialer`의 간격·포기 시간·`is_retryable` 정지 |

## 공통 규칙

- 고정 포트를 쓰지 않는다. 빈 포트를 잡는다.
- 시험이 띄운 프로세스와 임시 파일은 시험 안에서 정리하고 정리를 확인한다.
- heartbeat 같은 긴 시간 값은 연결별 설정으로 짧게 준다.
- 문서는 시험이 무엇을 확인하는지 요약만 한다. 정확한 조건은 시험 파일이 정본이다.

## 모바일 cross check와 내장 SSH 백엔드 시험

- 모바일 cross check(CI 잡 `mobile-android`·`mobile-ios`, 검사 단계 `mobile-check`)의 러너·target·크레이트 목록·feature 조합은 [모바일](mobile.md#모바일-cross-check)이 정본이다. 후속 작업이 잡을 만들 때 위 "CI 러너"의 workflow 표에 행을 더한다.
- 다른 프로그램 실행 금지의 정적 검사 `no-process-spawn`은 [모바일](mobile.md#다른-프로그램-실행-금지)에 있다.
- 위 "SSH 통합 시험의 sshd"의 시험은 시스템 `ssh` 클라이언트를 쓴다. 같은 흐름을 내장 SSH 백엔드로 돌리는 시험(OpenSSH 클라이언트를 `PATH`에서 뺀 조건), 폴백 선택 시험, `ssh -G` 대조, 지원하지 않는 지시어 표시와 안전한 실패 시험의 자리는 [모바일](mobile.md#시험의-자리-내장-백엔드)에 있다. 서버 쪽은 같은 시험 sshd를 쓴다.
