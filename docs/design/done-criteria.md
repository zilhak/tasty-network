# "제대로 동작하는 수준" 기준

사용자 결정(2026-11-27): tasty 본체에서 네트워크 코드를 걷어내는 일은 이 라이브러리가 제대로 동작하는 수준일 때 시작한다. 이 문서가 그 수준의 판정 기준이다. 기준이 모두 충족되면 판정 기록을 넘기고, tasty 전환은 그 뒤에 정한다.

각 항목은 시험이나 CI 잡 이름을 가리킨다. 무엇을 확인하는지는 한 줄 요약만 적고, 정확한 조건은 시험 파일이 정본이다. 시험 이름을 바꾸면 같은 커밋에서 이 표를 고친다.

## 기준

| # | 항목 | 판정 대상 | 확인하는 것(요약) | 증거 형태 |
|---|---|---|---|---|
| D1 | 세 OS CI | `.github/workflows/ci.yml`의 실행 하나: fmt 잡, clippy·test 잡 × `ubuntu-latest`·`macos-latest`·`windows-latest` | workspace 전체 fmt·clippy(`-D warnings`)·test 통과 | CI 실행 ID, 잡별 결론 |
| D2 | SSH 정방향 attach | `crates/tasty-net-demo/tests/ssh_attach.rs`의 `connect_and_echo`, `reconnect_after_tunnel_kill`, `reconnect_after_server_restart`, `wrong_token_is_rejected_without_retry` (세 OS) | 별도 OS 프로세스 두 개가 실제 `ssh` 터널로 연결·끊기·재연결하고, 틀린 토큰은 재시도 없이 끝남 | D1 실행의 잡 로그에서 시험마다 `test <이름> ... ok` 줄과 `sshd=` 줄 |
| D3 | SSH 역할 반전 attach | `crates/tasty-net-demo/tests/ssh_reverse_attach.rs`의 `reversed_holder_echo`, `reconnect_after_tunnel_kill`, `refused_proposal_stops_without_retry`, `self_attach_is_rejected` (세 OS) | SSH를 띄운 쪽이 아닌 쪽이 holder가 되고, 터널이 끊기면 SSH를 띄운 쪽이 복구함 | D2와 같음 |
| D4 | P2P(iroh) 직접 연결 | `crates/tasty-net-demo/tests/p2p_attach.rs`의 `direct_path_on_loopback`, `required_direct_path_times_out`, `direct_path_lost_closes_link`, `relay_allowed_keeps_relayed_link`, `unpaired_peer_is_closed_before_handshake`, `different_relays_connect_with_pairing_address`, `bulk_transfer_matches_bytes` (세 OS) | 별도 OS 프로세스 두 개가 relay를 거쳐 직접 경로로 바뀌고, 1안 정책과 페어링 거절이 동작함 | D1 실행의 잡 로그에서 시험마다 `ok` 줄 |
| D5 | relay 서버 동작 | (a) `.github/workflows/relay.yml`의 `relay-config-check` 잡. (b) 원격 빌드 환경 기록: `n0computer/iroh-relay:v1.5.0` 컨테이너를 `relay/` 설정 두 개로 각각 띄우고 `p2p-peer` 두 개를 붙임 | (a) 두 설정으로 relay가 뜨고 HTTP·metrics가 응답함. (b) 실제 relay 프로세스를 거쳐 연결되고 선택 경로가 직접으로 바뀜 | (a) CI 실행 ID. (b) `p2p-peer` 로그 발췌(경로 바뀜 줄)와 실행 sha |
| D6 | 원격 빌드 환경 전체 시험 | 같은 sha에서 `cargo test --workspace --locked` | CI와 다른 환경에서도 전체 시험 통과 | 종료 코드, 로그 위치(판정 기록에) |
| D7 | tasty 의존 없음 | CI 단계 `no-tasty-deps`(`scripts/check-no-tasty-deps.sh`, 레포 기반 작업이 만든다) | `cargo metadata`에서 tasty 레포를 가리키는 git 의존과, `manifest_path`가 이 workspace 루트 밖인 path 의존이 모두 0개(이 workspace 안의 path 의존은 제외) | D1 실행의 단계 결론 |

기준에 넣지 않은 것과 이유:

- 토폴로지: 지금 tasty에 없는 새 기능이고 전송 API에 영향을 주지 않는다. 단위 시험은 D1에 포함되어 돈다.
- 펀칭 실험: [P2P](p2p.md) "동작 수준 기준과의 관계"에 근거를 적었다. 아래 P1로 둔다.

## 판정 규칙

1. 모든 증거는 한 커밋 sha(이하 S)에서 나온다. S는 `develop`의 커밋이다. 서로 다른 sha의 결과를 모으지 않는다.
2. 시험의 실행 상태(통과·실패·미실행)와 기준 판정(충족·미충족)을 따로 적는다.
3. 미실행은 통과가 아니다. 미실행인 항목은 미충족이다. 예외는 하나뿐이다: SSH 시험(D2·D3)이 어느 OS에서든 OpenSSH sshd 대신 시험 전용 SSH 서버로 돈 경우([CI와 시험 환경](testing.md) "SSH 통합 시험의 sshd")는 통과로 센다. 클라이언트는 그때도 시스템 `ssh`다. 로그의 `sshd=` 줄로 어느 쪽인지 남긴다.
4. 시험 소스에 이름이 있는지만으로 판정하지 않는다. 잡 로그에 그 시험의 `ok` 줄이 있어야 한다.
5. 판정 기록에 S, 각 항목의 증거 값, 실행 상태, 판정을 적는다. 미충족이면 남은 일을 적는다.

## 판정 기록

판정 작업이 아래 표를 채운다.

| # | 증거 값 | sha | 실행 상태 | 판정 |
|---|---|---|---|---|
| D1 | | | | |
| D2 | | | | |
| D3 | | | | |
| D4 | | | | |
| D5 | | | | |
| D6 | | | | |
| D7 | | | | |

## P2P를 사용자에게 내놓기 전 기준

이 절은 tasty 전환 시작 판단에 쓰지 않는다. hub relay 배포와 P2P 사용자 공개의 선행 조건이다.

| # | 항목 | 증거 형태 |
|---|---|---|
| P1 | 펀칭 실험으로 `direct_wait` 기본값과 `punch-only` 수신 상한을 확정하고 `relay/punch-only.toml`·P2P 기본값에 반영 | [P2P](p2p.md) "결과" 표, 반영 커밋 sha |
