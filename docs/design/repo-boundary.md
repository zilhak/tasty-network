# 레포 경계와 tasty-ssh 의존 끊기

`tasty:<경로>`는 tasty 레포 698aa49e03 기준 경로다. 줄 번호도 같은 커밋 기준이다.

## 원칙

- 이 레포는 "두 기기 사이에 인증된 바이트 채널을 만들고 유지하는 일"을 맡는다. 프레임 형식, 전송(SSH·P2P), handshake와 인증, 기기 ID, 토폴로지 질의 형식, relay 배포물이 여기에 있다.
- tasty는 "그 채널로 무엇을 주고받는가"를 맡는다. attach 도메인 메시지, 점유, mirror, 화면, 설정 저장, 번역 문구가 tasty에 남는다.
- 와이어 형식(프레임 바이트)은 바꾸지 않고 옮긴다. handshake만 v2로 바꾼다([handshake v2](handshake.md)).
- 이 레포는 tasty의 데이터 디렉터리(`tasty_home`, `TASTY_HOME`), 환경 변수, 프로필 파일 형식을 모른다. 필요한 값은 소비자가 인자로 준다.

## 이 레포가 가질 것

| 영역 | 크레이트 | 내용 |
|---|---|---|
| 프레임 | `tasty-net-frame` | `write_frame`·`read_frame`, `StreamTag`, `FRAME_HEADER_LEN`·`MAX_FRAME_LEN`, `HEARTBEAT_INTERVAL`·`HEARTBEAT_TIMEOUT`, `ReadFailure`, mesh 청크, bulk 청크, `encode_mux`·`decode_mux` |
| SSH 실행 | `tasty-net-ssh` | `ssh` 경로 찾기, 공통 옵션, `-L` 터널, 포트 발견, 셸 감지, 취소, backoff, 리스너 판정 |
| 전송·handshake | `tasty-net-transport` | 전송 trait, 논리 채널, handshake v2 양쪽 함수, 역할, 거절 사유, bulk 자격, 요청 채널, heartbeat, 시도 취소, 강제 종료와 읽기 깨우기 |
| 기기 ID·토큰 | `tasty-net-identity` | 기기 키 생성·저장·로드, 서명, 연결 토큰, 권한 걸린 파일, TOFU 판정 |
| P2P | `tasty-net-p2p` | iroh endpoint, 경로 감시와 직접 경로 정책, 페어링 메시지·허용 목록 검사 |
| 토폴로지 | `tasty-net-topology` | 노드·엣지 모델, 질의·결과 메시지, capability 이름, 탐색 알고리즘, 응답 측 공통 함수 |
| relay 배포물 | `relay/` | 설정 예시, compose 예시, 설정 검사, 운영 안내, 접속 허용 콜백의 공개 규약 |

## tasty에 남을 것

- attach 도메인 메시지: `StreamControl`, `StructuralOp`, `ForceDetachNotice`, descriptor, `*_CAPABILITY` 상수(`ipc.stream.*`), `InheritCwd`.
- 연결별 프레임 분류와 push sink(`StreamHub`), 점유(`OccupancyRegistry`), mirror, 재연결 정책(언제 다시 거는가), 자기 인스턴스 ID 생성.
- 원격 프로필 해석·저장, 프로필에서 `SshTarget`을 만드는 일, 셸 감지 결과 저장.
- 번역 문구(i18n), UI, 설정 값 저장(heartbeat 시간, 공개 설정, 기본 relay 주소).
- 일반 JSON-RPC 요청·응답 루프와 메서드 dispatch. 포워딩 포트를 attach 전용으로 좁히는 일([위협 모델](threat-model.md)).

## 파일 단위 경계 (tasty 698aa49e03)

"옮김"은 이 레포에 같은 동작을 새로 쓴다는 뜻이다(참고·복사, MIT 같은 저작자). tasty 쪽 파일을 이 레포 크레이트로 바꾸는 일은 tasty 전환 단계에서 한다.

| tasty 파일 | 이 레포로 | tasty에 남김 |
|---|---|---|
| `tasty:crates/tasty-ipc/src/stream.rs` | 프레임 헤더·상한·heartbeat 상수, `StreamTag`, `StreamFrame`, `write_frame`·`read_frame`, mux·bulk 청크 → frame. `STREAM_OPEN_METHOD`·`STREAM_PROTO`·`StreamOpenParams`·`StreamAck` → transport handshake v2가 대신함 | `StreamControl`, `StructuralOp`, `ForceDetachNotice`, `*_CAPABILITY`, descriptor 형식, `SharedHeartbeatTimeout`(heartbeat 시간 설정 값을 새 연결에 나눠 주는 저장소. 이 레포는 그 값을 연결마다 `LinkTimeouts`로 받는다, [전송 추상화](transport.md) "heartbeat와 읽기 시간 제한") |
| `tasty:crates/tasty-ipc/src/stream/read_failure.rs` | 전부 → frame | — |
| `tasty:crates/tasty-ipc/src/stream/inherit_cwd.rs` | — | 전부 |
| `tasty:crates/tasty-ipc/src/mesh_stream.rs` | 전부 → frame | — |
| `tasty:crates/tasty-ipc/src/client/stream.rs` | 소켓 열기, `stream.open` 송신, ACK 읽기, 읽기·쓰기 분리 → transport | Control 해석, bulk 파일 흐름 |
| `tasty:crates/tasty-ipc/src/stream_hub.rs` | — (bulk 자격 검사는 transport에 새로 생기는 검사다. 지금 hub에는 없다) | 연결 등록, `bulk_workspace` 결속 기록(`register_bulk`), push sink, 입력 분류, Loss 통지. bulk 시작·완료 인가는 도메인(`tasty:src/remote/server.rs`)에 그대로 남는다 |
| `tasty:crates/tasty-ipc/src/instance.rs` | — (handshake가 값을 실어 나름) | 인스턴스 ID 생성 |
| `tasty:crates/tasty-ipc/src/caller.rs` | 토큰 생성 방식(32바이트 OS 난수 → 64자 hex)을 참고해 identity에 새로 씀 | 요청 라벨 `SessionToken` |
| `tasty:crates/tasty-ipc/src/port_file.rs` | 포트·토큰·인스턴스 ID를 묶은 연결 레코드 형식과 권한 걸린 게시 함수 → identity([기기 키·토큰](identity.md) "파일 형식") | 레코드 게시 시점·위치, 인스턴스가 끝날 때 게시 핸들로 정리를 부르는 일 |
| `tasty:src/adapters/production/tcp_ipc_server.rs` | `handle_stream_connection`의 handshake 파싱·검증·거절 ACK(`validate_stream_proto`, `parse_stream_handshake`) → transport 받는 쪽 함수 | listener, 첫 줄 분기, 요청·응답 루프, attach dispatch |
| `tasty:crates/tasty-remote/src/transport.rs` | handshake, 읽기·쓰기 스레드의 소켓 부분, heartbeat 송신, half-close·강제 종료·워커 회수 → transport | 도메인 프레임 해석, mirror 이벤트, 재연결 판단 |
| `tasty:crates/tasty-remote/src/transport_end.rs` | "끝 사유는 올라가기만 하고 한 곳에서 한 번 확정" 계약 → transport `LinkCloser` | 도메인 끝 사유 |
| `tasty:crates/tasty-remote/src/outbound.rs` | 시도 취소(`register_socket`) → transport `AttemptCancel` | 앱 소유 연결 수명 |
| `tasty:crates/tasty-remote/src/client_session.rs` | 터널 소유 → transport의 공유 터널 핸들 `SshTunnel`(링크·bulk·요청 채널이 참조를 나눠 가짐, [전송 추상화](transport.md) "터널 수명과 재연결") | mirror 세션 소유 |
| `tasty:crates/tasty-remote/src/connection.rs` | — | 전부 |
| `tasty:crates/tasty-remote/src/pending_connection.rs` | — | 전부 |
| `tasty:crates/tasty-remote/src/browse.rs` | — (요청 채널을 씀) | 원격 목록 조회와 합치기 |
| `tasty:crates/tasty-remote/src/browser.rs` | — | 전부 |
| `tasty:crates/tasty-remote/src/create.rs` | — (요청 채널을 씀) | 원격 `workspace.create` 호출 |
| `tasty:crates/tasty-remote/src/self_instance.rs` | 자기 인스턴스 판정 → handshake의 인스턴스 ID 비교(`SelfAttach` 거절) | 판정 결과 기록·표시 |
| `tasty:crates/tasty-remote/src/resize_sync.rs` | — | 전부 |
| `tasty:crates/tasty-remote/src/input_receipts.rs` | — | 전부 |
| `tasty:crates/tasty-remote/src/refusal.rs` | — | 전부(거절 사유 표시는 transport 거절 enum으로 분기) |
| `tasty:crates/tasty-remote/src/profile_detection/jobs.rs` | — | 전부. 지금은 jobs → `detect_for_profile`(tasty-ssh) → `detect_port_mode`(내부 함수)다. 전환 뒤에는 jobs → tasty로 옮긴 `detect_for_profile` → 이 레포의 공개 `detect_port_mode`다 |
| `tasty:crates/tasty-ssh/src/lib.rs` | 프로필 함수를 뺀 전부 → ssh | `SshTarget::from_remote_profile`·`from_attach_inline`, `resolve_attach_target`, `run_ssh_connect`, `apply_shell_to_profile`, `detect_for_profile`, `detect_and_persist` |
| `tasty:crates/tasty-ssh/src/tunnel.rs` | 전부 → ssh | — |
| `tasty:crates/tasty-ssh/src/listener.rs` | 전부 → ssh(번역 키 대신 enum) | 번역 문구 |
| `tasty:crates/tasty-utils/src/process.rs` | `hide_console` 같은 동작 → ssh 안에 새로 씀 | 원본 |
| `tasty:crates/tasty-utils/src/poison.rs` | `recover_mutex` 같은 동작 → ssh 안에 새로 씀 | 원본 |
| `tasty:crates/tasty-utils/src/socket_wake.rs` | `shutdown_both_and_wake` 같은 동작 → transport 안에 새로 씀 | 원본 |
| `tasty:crates/tasty-reaper/src/lib.rs` | — (자식 생성 훅으로 주입받음) | 전부 |
| `tasty:crates/tasty-remote-profiles/src/secret_file.rs` | 권한 계약을 참고해 identity에 새로 씀 | 원본(passkey 전용) |
| `tasty:src/app/attach_client/bulk.rs` | 두 번째 소켓 열기 → transport `open_bulk` | bulk 파일 흐름 |
| `tasty:src/app/attach_heartbeat.rs` | — | 설정 읽기. 값은 연결마다 transport에 넘김 |
| `tasty:src/app/auto_attach.rs` | — | 재연결 정책과 끝점 해석 |
| `tasty:src/core/attach.rs` | — | 점유 레지스트리 |
| `tasty:src/remote/server.rs` | — | attach 서버 도메인, descriptor, bulk 완료 인가 |

## `StreamOpenParams` 도메인 필드

`StreamOpenParams`의 `target`·`target_workspace`·`bulk_workspace`(`tasty:crates/tasty-ipc/src/stream.rs:199`)는 attach 도메인 값이다. v2 handshake는 이 값을 해석하지 않는다. tasty가 이 값을 담은 JSON 텍스트를 `ext` 문자열에 그대로 싣는다([handshake v2](handshake.md) "도메인 확장"). 세 필드의 상호 배타 검사는 tasty의 도메인 검증 콜백이 한다. 지금 tasty는 `target`과 `target_workspace`를 함께 받으면 workspace를 고르지만 필드 문서는 거절한다고 적는다. v2로 바꿀 때 tasty는 문서대로 거절(`Domain` 사유)한다. 이 레포의 시험은 거절 경로를 소비자 콜백 흉내로 확인한다.

## tasty-ssh 역방향 의존 끊기

`tasty:crates/tasty-ssh/Cargo.toml`은 tasty 내부 크레이트 4개에 의존한다. 후보 정의(기존 계획):

- (a) 오류를 번역 키가 아닌 오류 종류(enum)로 돌려주고 tasty가 번역한다.
- (b) 도움 함수를 trait·함수 주입이나 작은 공용 크레이트로 뺀다.
- (c) 프로필 의존을 값 타입으로 끊고 프로필 해석·저장은 tasty에 남긴다.

세 후보는 서로 다른 의존을 맡으므로 의존마다 고른다.

| 의존 | 쓰는 곳 | 결정 | tasty 전환 때 tasty가 할 일 |
|---|---|---|---|
| `tasty-i18n` | `PortDiscoveryError`의 `Display`(`lib.rs:453`, 키 `ssh.port_discovery.*` :414-419), `RemoteListenerAbsent`(`listener.rs:180`), `detect_and_persist`(`lib.rs:1121`) | (a) `PortDiscoveryFailureKind`와 `RemoteListenerAbsent`를 공개 타입으로 두고 `Display`는 영어 고정 문구. `detect_and_persist`는 tasty에 남으므로 그 번역 호출도 tasty에 남는다 | enum 값으로 분기해 기존 키(`ssh.port_discovery.*`, `ssh.tunnel.listener_absent`, `cli.remote_profile.not_found`)를 고른다 |
| `tasty-utils` | `process::hide_console`(`lib.rs:823`, `tunnel.rs:74`), `poison::recover_mutex`(`lib.rs:584`) | 이 레포에서 자체 구현. `hide_console`은 Windows에서 `CREATE_NO_WINDOW`(0x0800_0000)를 거는 몇 줄 함수이고 `std::os::windows::process::CommandExt::creation_flags`로 외부 의존 없이 쓴다. `recover_mutex`는 poison 복구와 첫 1회 보고다. 공용 크레이트를 둘 만큼 크지 않다 | 없음 |
| `tasty-reaper` | `spawn_bound_to_host`(`tunnel.rs:76`) | (b) 자식 생성 훅 주입. ssh 크레이트는 `SpawnHook`(`SpawnHook::new`로 `Fn(Command) -> io::Result<Child> + Send + Sync` 함수를 감싼 값)을 `SshLauncher::with_spawn_hook`으로 받고, 주지 않으면 `Command::spawn`을 쓴다. 훅은 터널 ssh와 포트 발견 ssh 모두에 쓴다 | `spawn_bound_to_host`를 훅으로 넣는다. 아래 두 계약을 tasty가 지킨다 |
| `tasty-remote-profiles` | `SshTarget::from_remote_profile`(`lib.rs:100`)·`from_attach_inline`(:123), `resolve_attach_target`(:153), `apply_shell_to_profile`(:1070), `detect_for_profile`(:1095), `detect_and_persist`(:1115) | (c) `SshTarget`·`PortMode` 값 타입과 프로필 없이 도는 함수(`SshTarget::parse`, `discover_remote_port`, `detect_port_mode`)만 이 레포에 둔다. 위 여섯 함수는 tasty에 남긴다 | 여섯 함수를 tasty 크레이트로 옮기고 이 레포 값 타입을 쓴다. `SshTarget`이 이 레포 타입이 되므로 associated 생성자(`SshTarget::from_remote_profile`·`from_attach_inline`)는 tasty 쪽 자유 함수나 확장 trait로 바꾼다(외부 타입에 inherent 메서드를 더할 수 없다) |

결과: 이 레포의 SSH 크레이트에 남는 tasty 의존은 0개다.

### reaper 계약은 훅을 넣는 tasty가 지킨다

`tasty:crates/tasty-reaper/src/lib.rs`의 `spawn_bound_to_host` 문서(51행 부근)가 정한 계약이다. 시험 `tasty:crates/tasty-reaper/tests/bound_spawn.rs`는 파일 전체가 Linux 전용(`#![cfg(target_os = "linux")]`)이라 계약 1만 확인한다. 계약 2는 같은 `lib.rs`의 Windows 구현(`init_host_reaper`·`adopt_pid`, Windows용 `spawn`)이 근거이고, 그 단위 시험은 빈 job 생성과 미초기화 때 무동작까지만 본다. 초기화된 job에 자식을 등록하고 호스트가 끝날 때 자식이 끝나는지는 tasty 시험이 확인하지 않는다.

1. Linux: 부모가 죽으면 자식이 SIGTERM을 받도록 PDEATHSIG를 건다. PDEATHSIG는 fork한 스레드의 종료에 반응하므로 짧게 사는 워커가 아니라 프로세스 수명 동안 남는 spawner 스레드에서 실행한다.
2. Windows: `init_host_reaper`로 만든 호스트 job에 등록한다. 초기화하지 않은 CLI 등에서는 등록하지 않는다. 다른 OS는 그대로 실행한다.

이 레포는 훅을 "자식을 만드는 유일한 자리"로 부르기만 한다. 훅이 어느 스레드에서 실제로 fork하는지는 훅이 정한다. 그래서 이 레포의 ssh 크레이트가 짧게 사는 워커 스레드에서 훅을 불러도 tasty 훅(spawner 스레드에 넘김)은 계약 1을 지킨다. 이 레포의 시험은 훅이 불린 횟수만 확인한다. 계약 1은 tasty의 `bound_spawn.rs`(Linux)가 계속 확인하고, 계약 2는 tasty의 Windows 구현이 지키며 그 시험은 이 레포 범위 밖이다.

### 포트 발견의 tasty 관례

포트 발견은 원격 명령 `<원격 tasty> port`(`lib.rs:859`), 포트 파일 `~/.tasty/tasty.port`·`~/.tasty-debug/tasty.port`(:845-914), Windows `type %USERPROFILE%\...`에 기댄다. 이 레포는 이 이름들을 상수로 두지 않고 `PortDiscoveryConfig`로 받는다. 원격 실행 파일 이름(`remote_program`), 그 뒤 인자(`remote_args`, tasty는 `port`), 데이터 디렉터리 이름(`data_dir`), 포트 파일 이름(`port_file`), 명시 파일 경로(`explicit_port_file`)가 필드다. 기본값은 두지 않는다. tasty가 자기 값을 넣고, 시험과 예제는 임시 값을 넣는다.

포트 발견의 동작 규칙(지금 tasty와 같게 옮긴다):

- 명시 경로가 우선한다. `PortDiscoveryConfig.explicit_port_file`에 명시 파일 경로가 있으면 관례 경로와 모드 체인보다 먼저, 그것만 읽는다(`cat <경로>` → `type <경로>`, `lib.rs` `discover_via_explicit_file`). tasty는 프로필의 `port_file`을 여기에 넣는다(`tasty:crates/tasty-remote/src/browse.rs`).
- 명시 경로가 없으면 모드를 따른다. `Auto`는 subcommand → Unix 파일 → Windows 파일 순이다(`AUTO_FALLBACK_CHAIN`).
- 취소와 원격 인스턴스 종료 확정(`RemoteInstanceExited`)은 다음 방식으로 넘어가지 않고 그 자리에서 끝난다(`stops_auto_chain`).
- 새 연결 레코드(포트·토큰·인스턴스 ID, [기기 키·토큰](identity.md) "토큰 전달")도 같은 우선순위·체인·종료 분류로 읽는다. 레코드 하나를 읽으므로 포트와 토큰을 따로 찾는 단계가 없다.
- 레코드 해석은 한 곳에 둔다. `tasty-net-ssh`의 발견 루프는 stdout을 해석하는 디코더를 주입받고(지금 tasty의 `parse_port` 자리. `discover_remote_port`와 `detect_port_mode`의 `decode` 인자. 숫자 포트 하나만 읽는 디코더는 `decode_port`다). 셸 감지도 디코더를 받는다. 셸 감지는 디코더가 받아들인 모드를 성공으로 보므로, 레코드를 출력하는 원격에 숫자 포트 디코더를 쓰면 subcommand 모드가 실패로 보인다, `tasty-net-transport`가 `tasty-net-identity`의 레코드 파서를 넘긴다. 그래서 게시와 SSH 발견이 같은 파서를 쓰고, ssh 크레이트는 이 레포 안 의존이 없는 채로 남는다([크레이트 구성](crates.md) "의존 방향"). 숫자 포트 하나를 읽는 경우(`Discovered::LegacyPort`)도 그 파서가 가른다.
- 발견 실패의 진단 detail에는 stdout 원문을 넣지 않는다([기기 키·토큰](identity.md) "토큰 비교"의 원문 규칙).

- 환경 변수 `TASTY_SSH_VERIFY`(`lib.rs:227` `run_ssh_connect`, :1105 `detect_for_profile`, `tasty:crates/tasty-remote/src/browse.rs`)는 읽지 않는다. 호스트 키 확인 여부는 `verify: bool` 값(`SshLauncher::with_verify`)으로 받고, 환경 변수를 이 값으로 바꾸는 일은 tasty에 남는다.
- ssh 옵션 순서(`lib.rs:348-384`)는 그대로 옮긴다. `BatchMode=no`·`ServerAliveInterval`·`ServerAliveCountMax`는 언제나, `StrictHostKeyChecking=accept-new`는 `verify = true`일 때만 사용자 `extra_options`보다 먼저 넣는다(ssh는 같은 옵션의 첫 값을 쓰므로 이 값들이 이긴다). `verify = false`이면 이 레포는 `StrictHostKeyChecking`을 넣지 않고, 사용자 `extra_options`나 ssh 설정 파일·기본값이 정한다. `ConnectTimeout`은 맨 뒤에 기본값(`SSH_CONNECT_TIMEOUT` 10초)을 넣는다. ssh는 명령행 `-o`를 설정 파일보다 먼저 적용하므로, 이 기본값을 이기는 것은 그보다 앞에 들어간 사용자 `extra_options`의 `ConnectTimeout`뿐이다. ssh 설정 파일의 `ConnectTimeout`은 이 기본값에 진다.
- 한국어 `anyhow!` 문자열(예: `PortMode::parse` :289)은 [오류 규칙](crates.md#오류-규칙)대로 enum + 영어 `Display`로 바꾼다. 터널 수립 실패는 `TunnelError` enum이다.
- 리스너 판정(`is_dropped_connection`, `ListenerVerdict::remote_listener_absent`·`explain_failure`)은 `anyhow::Error` 대신 `&(dyn std::error::Error + 'static)`를 받아 `source()` 사슬을 본다(`ReadFailure::of_error`와 같은 방식). tasty의 `explain_failure`는 실패 위에 `RemoteListenerAbsent` 맥락을 붙여 돌려줬지만, 이 레포의 것은 판정 결과 `Option<RemoteListenerAbsent>`만 돌려주고 붙이는 일은 소비자가 한다. `connection_closed_error`는 `io::Error`를 돌려준다.
- `SshTunnel`의 메서드는 모두 `&self`를 받는다. transport가 `Arc`로 한 터널을 여러 채널에 나눠 주고([전송 추상화](transport.md) "터널 수명과 재연결"), 마지막 참조가 drop될 때 자식을 끝내고 회수한다.
