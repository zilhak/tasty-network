# "제대로 동작하는 수준" 기준

사용자 결정(2026-11-27): tasty 본체에서 네트워크 코드를 걷어내는 일은 이 라이브러리가 제대로 동작하는 수준일 때 시작한다. 이 문서가 그 수준의 판정 기준이다. 기준이 모두 충족되면 판정 기록을 넘기고, tasty 전환은 그 뒤에 정한다.

각 항목은 시험이나 CI 잡 이름을 가리킨다. 무엇을 확인하는지는 한 줄 요약만 적고, 정확한 조건은 시험 파일이 정본이다. 시험 이름을 바꾸면 같은 커밋에서 이 표를 고친다.

## 기준

| # | 항목 | 판정 대상 | 확인하는 것(요약) | 증거 형태 |
|---|---|---|---|---|
| D1 | 세 OS CI | `.github/workflows/ci.yml`의 실행 하나: fmt 잡, clippy·test 잡 × `ubuntu-latest`·`macos-latest`·`windows-latest`. clippy·test 잡은 검사 단계 `p2p-example-pair`(후속 작업이 만든다)도 부른다 | workspace 전체 fmt·clippy(`-D warnings`)·test 통과, P2P 예제 두 프로세스가 서버를 거쳐 붙어 echo함([CI와 시험 환경](testing.md#크레이트-단독)) | CI 실행 ID, 잡별 결론, 단계 결론 |
| D2 | SSH 정방향 attach | `crates/tasty-net-demo/tests/ssh_attach.rs`의 `connect_and_echo`, `reconnect_after_tunnel_kill`, `reconnect_after_server_restart`, `wrong_token_is_rejected_without_retry` (세 OS) | 별도 OS 프로세스 두 개가 실제 `ssh` 터널로 연결·끊기·재연결하고, 틀린 토큰은 재시도 없이 끝남 | D1 실행의 잡 로그에서 시험마다 `test <이름> ... ok` 줄과 `sshd=` 줄 |
| D3 | SSH 역할 반전 attach | `crates/tasty-net-demo/tests/ssh_reverse_attach.rs`의 `reversed_holder_echo`, `reconnect_after_tunnel_kill`, `refused_proposal_stops_without_retry`, `self_attach_is_rejected` (세 OS) | SSH를 띄운 쪽이 아닌 쪽이 holder가 되고, 터널이 끊기면 SSH를 띄운 쪽이 복구함 | D2와 같음 |
| D4 | P2P(quinn) 연결 | (a) `tasty-net-p2p`의 `tests/punch.rs`·`tests/lifecycle.rs`·`tests/two_process.rs`와 `tasty-net-rendezvous`의 `tests/server.rs`([CI와 시험 환경](testing.md#시험-목록-요약-정확한-조건은-시험-파일이-정본)) (세 OS). (b) `tasty-net-transport`의 `tests/p2p_adapter.rs` (세 OS). (c) `crates/tasty-net-demo/tests/p2p_attach.rs`의 `p2p_attach_echo`, `unpaired_peer_is_refused`, `bulk_transfer_matches_bytes` (세 OS) | (a) 프로세스 안 서버와 모의 NAT에서 NAT 조합별 펀칭·실패 사유(서버에 닿는 UDP 막힘과 닿지 않는 막힘을 따로), 정책(꺼짐, 다른 서버로 나간 패킷 0, 페어링 거절, 자기 연결, 펀칭 세션 밖 QUIC 거절), 취소·기한 경로, 재연결·TLS 단계 정지·서버 한계값 불일치·keepalive·주소 바뀜의 비대칭·상태 기계, 서버의 중계 없음·프레임 경계·광고 주소, 두 프로세스 연결. (b) 어댑터 위 채널 계약. (c) 별도 OS 프로세스 두 개의 transport 위 P2P 링크 | D1 실행의 잡 로그에서 시험마다 `ok` 줄 |
| D5 | rendezvous 서버 동작 | (a) `.github/workflows/rendezvous.yml`의 `rendezvous-check` 잡(지금의 `relay.yml`·`relay-config-check`를 바꾼다, 후속 작업이 만든다). (b) 원격 빌드 환경에서 `rendezvous/container-check.sh` 실행: 이 레포가 빌드한 서버 이미지를 띄우고 같은 망의 두 기기(또는 같은 머신의 두 프로세스)가 `p2p_peer` 예제로 붙음 | (a) 예시 설정으로 서버가 뜨고 주·보조 포트가 STUN에 응답하며 설정값 조회가 동작함. (b) 실제 서버 프로세스로 소개받아 직접 연결되고 서버가 기기 사이 패킷을 받지 않음 | (a) CI 실행 ID. (b) 스크립트 종료 코드, `rendezvous-check result=ok` 줄, 예제의 연결 정보 출력(경로·주소쌍·서버), 실행 sha. 다른 망 확인은 D5에 넣지 않는다(P1) |
| D6 | 원격 빌드 환경 전체 시험 | 같은 sha에서 `cargo test --workspace --locked` | CI와 다른 환경에서도 전체 시험 통과 | 종료 코드, 로그 위치(판정 기록에) |
| D7 | tasty 의존 없음 | CI 단계 `no-tasty-deps`(`scripts/check-no-tasty-deps.sh`, 레포 기반 작업이 만든다) | `cargo metadata`에서 tasty 레포를 가리키는 git 의존과, `manifest_path`가 이 workspace 루트 밖인 path 의존이 모두 0개(이 workspace 안의 path 의존은 제외) | D1 실행의 단계 결론 |
| D8 | P2P 크레이트 격리·단독 실행 | CI 단계 `p2p-isolation`(후속 작업이 `scripts/check.sh`에 만든다)과 `cargo test -p tasty-net-p2p --locked`·`cargo test -p tasty-net-rendezvous --locked` 단독 실행 (세 OS) | 두 크레이트의 workspace 안 의존이 identity(와 p2p → rendezvous)뿐이고, 각 크레이트가 단독으로 시험을 통과함([크레이트 구성](crates.md#p2p-크레이트-격리)) | D1 실행의 단계 결론과 단독 실행 잡 로그 |
| D9 | 연결 목록 | [CI와 시험 환경](testing.md#연결-목록-시험)의 `connection_table_reports_ssh_forward`, `connection_table_reports_ssh_role_reversed`, `connection_table_reports_p2p`, `both_ends_report_one_edge`, `history_is_capped`, `closed_entries_are_capped`, `event_queue_reports_lag`, `ids_are_stable_and_never_reused`, `redialer_follows_settings` (세 OS) | 연결 목록이 세 경로 각각에서 기기·경로·방향·주소 구간·서버·상태·전이 원인을 맞게 보고하고, 재연결은 새 ID가 옛 ID를 가리키며, 한 링크는 엣지 하나임([전송 추상화](transport.md#연결-목록)) | D1 실행의 잡 로그에서 시험마다 `ok` 줄 |
| D10 | 설정값 노출 | `settings_list_matches_query`(p2p, rendezvous, transport 연결 목록), `defaults_match_design_tables`(같은 범위), `dependency_defaults_match_source`·`dependency_setters_are_covered`·`dependency_settings_are_applied`·`connection_reports_applied_settings`(p2p), `ssh_connection_reports_applied_sources`(transport), CI 단계 `no-hidden-constants`와 그 검사 자체의 시험(후속 작업이 만든다) | 설정 항목 전체와 조회 결과가 같고 출처가 맞으며, 코드 기본값의 단일 원천이 설계 표와 같고, 고정판의 공개 setter가 모두 표에 있고, 의존성 설정과 고정값이 실제로 들어가며, 연결별 출처(`server` 포함)가 맞고, 허용 목록 밖의 숨은 리터럴이 없음([CI와 시험 환경](testing.md#설정값-시험)). 대상은 P2P·rendezvous 크레이트와 transport 연결 목록 값이고, 기존 크레이트의 다른 상수는 규칙을 맞추는 후속 작업 때 더한다 | D1 실행의 잡 로그와 단계 결론 |

기준에 넣지 않은 것과 이유:

- 토폴로지: 지금 tasty에 없는 새 기능이고 전송 API에 영향을 주지 않는다. 단위 시험은 D1에 포함되어 돈다.
- 다른 망 사이의 펀칭 실험: [P2P](p2p.md#동작-수준-기준과의-관계)에 근거를 적었다. 아래 P1로 둔다.

## 판정 규칙

1. 모든 증거는 한 커밋 sha(이하 S)에서 나온다. S는 `develop`의 커밋이다. 서로 다른 sha의 결과를 모으지 않는다.
2. 시험의 실행 상태(통과·실패·미실행)와 기준 판정(충족·미충족)을 따로 적는다.
3. 미실행은 통과가 아니다. 미실행인 항목은 미충족이다. SSH 시험(D2·D3)이 어느 OS에서든 OpenSSH sshd 대신 시험 전용 SSH 서버로 돈 경우([CI와 시험 환경](testing.md) "SSH 통합 시험의 sshd")는 그 시험의 결과를 그대로 쓴다(통과면 통과). 클라이언트는 그때도 시스템 `ssh`다. 로그의 `sshd=` 줄로 어느 쪽인지 남긴다. 두 서버가 모두 안 된 경우(`sshd=none`)는 미실행이 아니라 실패다.
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
| D8 | | | | |
| D9 | | | | |
| D10 | | | | |

## P2P를 사용자에게 내놓기 전 기준

이 절은 tasty 전환 시작 판단에 쓰지 않는다. hub rendezvous 배포와 P2P 사용자 공개의 선행 조건이다.

| # | 항목 | 증거 형태 |
|---|---|---|
| P1 | hub rendezvous 배포 때 다른 망 사이 펀칭 실험으로 서버 설정 `punch_window`·`quic_handshake_timeout`과 기기 설정 `punch.probe_interval`의 기본값을 확정하고 반영. 망 조합별 성공률을 기록 | [P2P](p2p.md#결과-실험-뒤-채운다) 표, 반영 커밋 sha |
