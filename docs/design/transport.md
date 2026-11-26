# 전송 추상화, 채널, 역할

`tasty:<경로>`는 tasty 레포 698aa49e03 기준이다. 아래 타입·함수 이름은 `tasty-net-transport`의 정본 이름이다. 시그니처는 구현 때 정하되 이름과 계약은 이 문서를 따른다.

## 용어

| 용어 | 뜻 |
|---|---|
| 전송(transport) | 두 기기 사이에 바이트 채널을 만드는 방법. SSH 정방향, SSH 역할 반전, P2P(iroh), 시험용 메모리 |
| 채널(channel) | 순서를 지키는 양방향 프레임 흐름 하나. SSH에서는 터널을 지나는 TCP 연결 하나, P2P에서는 QUIC 양방향 스트림 하나 |
| 링크(`Link`) | handshake를 마친 채널 묶음. 목적(`purpose`)이 `attach`이면 순서 채널 하나와 나중에 여는 bulk 채널들을 가진다 |
| dialer / acceptor | 채널을 연 쪽 / 받은 쪽. SSH를 누가 띄웠는지, 누가 holder인지와 별개다 |

## 전송 trait

두 층으로 나눈다. 전송마다 다른 것은 아래 층(`RawChannel`)뿐이고, handshake·heartbeat·링크 수명은 위 층이 모든 전송에 같게 한다.

- `RawChannel`: 전송 구현이 제공하는 채널 하나. 읽기 반쪽(`Read`), 쓰기 반쪽(`Write`), 제어(`set_read_timeout`, `shutdown_write`, `shutdown_and_wake`)로 나눌 수 있다.
- `Transport`: `connect(&self, target, &AttemptCancel) -> Result<RawChannel, ConnectError>`로 순서 채널을 연다. 같은 끝점에 두 번째 채널(bulk)을 여는 `connect_more`를 가진다. SSH 구현은 터널을 한 번 만들고 그 로컬 포트에 TCP를 다시 연다. P2P 구현은 같은 QUIC 연결에 양방향 스트림을 하나 더 연다.
- `Listener`: 받는 쪽. `accept() -> Result<RawChannel, AcceptError>`. SSH 경로에서는 소비자의 루프백 TCP listener가 받은 소켓을 `RawChannel::from_tcp`로 감싼다. 첫 줄 분기(`stream.open`인지 일반 JSON-RPC인지)는 소비자가 하고, 이미 읽은 바이트를 넘긴다(아래 "받는 쪽 진입").
- 위 층 함수: `dial_handshake`(dialer)와 `accept_handshake`(acceptor)가 `RawChannel`에 handshake v2를 돌려 `Link`(또는 bulk·요청 채널)를 만든다([handshake v2](handshake.md)).

`Link`가 소비자에게 주는 것:

| 항목 | 계약 |
|---|---|
| `take_ordered()` | 순서 채널의 `ChannelReader`·`ChannelWriter`를 한 번만 꺼낸다. 읽기 스레드와 쓰기 스레드가 따로 가진다 |
| `open_bulk(ext)` | 부모 링크의 bulk 자격으로 bulk 채널을 연다([handshake v2](handshake.md) "bulk 자격") |
| `peer()` | handshake가 확인한 상대 정보: 기기 ID, 인스턴스 ID, 역할, capability 목록, 전송 종류 |
| `closer()` | 복제 가능한 `LinkCloser`. 강제 종료와 끝 사유 확정 |
| `Drop` | 남은 채널을 닫고, 내부 워커 스레드를 join하고, SSH 터널 자식을 끝내고 회수한다 |

`ChannelReader::read_frame`은 `tasty-net-frame`의 `read_frame`을 쓰고 실패를 `ReadFailure`로 분류해 돌려준다. `ChannelWriter::write_frame`은 헤더와 payload를 한 버퍼로 한 번에 쓴다(`tasty:docs/dev-guide/attach-behavior.md` "프레임 전송 지연 (Nagle 금지)").

### 받는 쪽 진입

tasty 서버는 포트 하나에서 첫 줄을 읽어 `stream.open`이면 스트림으로, 아니면 요청·응답 루프로 보낸다(`tasty:src/adapters/production/tcp_ipc_server.rs` `handle_connection`). 그 첫 줄을 읽은 `BufReader`에는 이미 다음 바이트가 들어와 있을 수 있다. `accept_handshake`는 `(RawChannel, first_line, buffered: Vec<u8>)`를 받아 버퍼 바이트를 먼저 읽고 소켓으로 이어 읽는다. 버퍼를 버리면 첫 프레임이 사라진다.

## 채널 대응

| 채널 | SSH(정방향·역할 반전) | P2P | 메모리(시험) |
|---|---|---|---|
| 순서 채널 | 터널 로컬 포트로 연 TCP 연결 하나. `set_nodelay(true)` | dialer가 연 첫 QUIC 양방향 스트림 | 프로세스 안 파이프 한 쌍 |
| bulk 채널 | 같은 로컬 포트로 연 두 번째 TCP 연결(`tasty:src/app/attach_client/bulk.rs:123`과 같음) | 같은 QUIC 연결의 추가 양방향 스트림 | 파이프 한 쌍 더 |
| 요청 채널 | 같은 로컬 포트로 연 TCP 연결 | 같은 QUIC 연결의 추가 양방향 스트림 | 파이프 한 쌍 더 |

채널마다 handshake를 한 번 한다. 첫 메시지의 `purpose`가 `attach`·`bulk`·`request` 중 무엇인지로 받는 쪽이 구분한다.

## heartbeat와 읽기 시간 제한

- 읽기 시간 제한은 링크마다 정한다. `LinkTimeouts { read_timeout, handshake_timeout }`를 `dial_handshake`·`accept_handshake`에 넘긴다. 기본 `read_timeout`은 `HEARTBEAT_TIMEOUT`(30초)이다. tasty는 새 연결마다 설정값을 읽어 넘긴다(`tasty:src/app/attach_heartbeat.rs`). 시험은 짧은 값을 넘긴다.
- Ping 송신 정책은 연결 쪽마다 고른다(`tasty:docs/dev-guide/attach-behavior.md` "연결 생존 확인 (read timeout + heartbeat)").
  - `HeartbeatPolicy::WhenIdle`: 송신이 `HEARTBEAT_INTERVAL` 동안 조용할 때만 Ping을 보낸다. tasty 서버 write 스레드의 동작이다.
  - `HeartbeatPolicy::Periodic`: 트래픽과 관계없이 `HEARTBEAT_INTERVAL`마다 보낸다. tasty GUI·CLI 클라이언트의 동작이다.
- 채널마다 따로 돈다. bulk 채널도 자기 Ping과 시간 제한을 가진다.
- 시간 제한은 소켓 읽기 시간 제한이라 프레임 일부를 읽은 뒤에도 만료될 수 있다(`tasty:crates/tasty-ipc/src/stream/read_failure.rs` 주석). 한 번 `HeartbeatTimeout`이 난 `ChannelReader`는 이후 읽기도 같은 실패를 돌려준다. 같은 채널에서 다시 읽어 프레임 경계를 잃는 일을 막는다.

## 읽기 실패 분류

`ReadFailure{HeartbeatTimeout, PeerClosed, Protocol, Other}`는 `std::io::ErrorKind`로 판정한다. 대응은 tasty와 같다(`tasty:crates/tasty-ipc/src/stream/read_failure.rs` `of_io`).

| `io::ErrorKind` | 분류 |
|---|---|
| `WouldBlock`(macOS·Linux 시간 초과), `TimedOut`(Windows 시간 초과) | `HeartbeatTimeout` |
| `UnexpectedEof`, `ConnectionReset`, `ConnectionAborted` | `PeerClosed` |
| `InvalidData`(모르는 tag, 상한 초과) | `Protocol` |
| 그 밖 | `Other` |

P2P 구현은 QUIC 오류를 이 `ErrorKind`로 바꿔 돌려준다. 상대가 연결이나 스트림을 닫음 → `ConnectionReset` 또는 `UnexpectedEof`, QUIC idle timeout → `TimedOut`, 이 쪽이 닫음 → `ConnectionAborted`. 그래서 소비자는 전송 종류를 모른 채 같은 분류를 쓴다. 정확한 대응은 `tasty-net-p2p` 시험이 정본이다.

## 시도 취소, 닫기, 회수

| 동작 | 계약 | 참고한 tasty 동작 |
|---|---|---|
| 시도 취소 `AttemptCancel` | 연결 시도마다 하나. 시도 중 만든 소켓·터널 자식·QUIC 연결을 등록한다. `cancel()` 뒤 진행 중인 `connect`·handshake는 정해진 시간 안에 `Cancelled`로 돌아오고 등록된 자원은 닫히고 회수된다 | `tasty:crates/tasty-remote/src/outbound.rs` `register_socket` |
| 쓰기 반쪽 닫기 `ChannelWriter::half_close` | 상대가 남은 프레임을 끝까지 읽은 뒤 EOF를 본다. TCP는 `shutdown(Write)`, QUIC는 송신 스트림 `finish` | `tasty:crates/tasty-remote/src/transport.rs` `Shutdown::Write` |
| 강제 종료 `LinkCloser::force_close(reason)` | 모든 채널을 닫고 막혀 있는 읽기를 깨운다. TCP는 `shutdown(Both)` 뒤 Windows에서 진행 중 I/O 취소까지 한다. QUIC는 연결을 닫는다 | `tasty:crates/tasty-utils/src/socket_wake.rs` `shutdown_both_and_wake` |
| 끝 사유 | 사유는 올라가기만 하고 한 번만 확정된다. 먼저 확정한 쪽의 사유가 최종이다. 확정하지 못한 쪽은 깨우기를 하지 않는다 | `tasty:crates/tasty-remote/src/transport_end.rs` |
| 회수 | `Link`를 drop하면 내부 워커를 join하고 SSH 터널 자식을 끝내고 회수한다. 시험이 끝난 뒤 자식 프로세스와 스레드가 남지 않는다 | `tasty:crates/tasty-remote/src/transport.rs` 워커 묶음의 깨우기·join·터널 회수 |

### 터널 수명과 재연결

- SSH 전송의 `Link`가 그 터널의 수명을 가진다. 링크를 닫으면 터널도 끝난다. bulk·요청 채널은 같은 터널을 쓴다.
- 언제 다시 거는지(backoff 간격, 포기 조건, 거절 사유별 재시도 여부)는 소비자가 정한다. 이 레포는 `tasty-net-ssh`의 `Backoff`와 거절 사유 enum만 준다. 인증 거절(`TokenInvalid` 등)은 다시 걸어도 같은 결과이므로 소비자가 재시도하지 않게 거절 사유에 `is_retryable()`을 둔다.
- 다시 걸 때는 포트 발견과 터널 열기부터 새로 한다. 원격 인스턴스가 다시 시작하면 포트가 바뀌기 때문이다(`tasty:src/app/auto_attach.rs` `resolve_endpoint`).

## 동기·비동기 경계

- `Transport`, `RawChannel`, `Link`, `ChannelReader`·`ChannelWriter`는 모두 동기 API다. 블로킹 호출은 호출한 스레드를 막는다.
- `tasty-net-p2p`는 `P2pEndpoint`를 만들 때 자기 tokio 런타임(멀티스레드, 작은 워커 수)을 만들어 소유한다. 동기 읽기·쓰기는 그 런타임 핸들의 `block_on`으로 QUIC 스트림을 읽고 쓴다. 시간 제한은 `tokio::time::timeout`으로 건다. 강제 종료는 QUIC 연결을 닫아 막힌 `block_on`을 깨운다.
- 동기 API를 tokio 런타임 안(비동기 작업 안)에서 부르면 안 된다. P2P 크레이트는 그 경우 오류를 돌려준다(패닉하지 않는다). 지금 소비자(tasty)는 tokio를 쓰지 않는다.
- 비동기 소비자가 생기면 `tasty-net-p2p`에 async API를 더한다. 동기 trait는 그대로 둔다.

## 순서 계약과 채널

이 레포가 보장하는 순서는 하나다: **한 채널 안에서 한 writer가 쓴 프레임은 쓴 순서대로 상대 reader에 도착한다.** 채널 사이에는 순서가 없다. 그래서 tasty의 순서·시점 계약은 모두 그 계약에 관련된 프레임이 한 채널에 있어야 지켜진다. 아래 표는 tasty 계약 문서(`tasty:docs/dev-guide/attach-behavior.md`, `tasty:docs/features/remote-attach/index.md`)의 계약을 모은 것이다.

| # | 계약 | tasty 출처(절) | 채널 |
|---|---|---|---|
| O1 | attach 직후 snapshot은 구독 등록과 같은 lock cut에서 한 번 나가고, 그 뒤 변화는 Data delta로 온다 | attach-behavior "초기 스냅샷 + delta", remote-attach "화면 동기화" | 순서 |
| O2 | 연결의 첫 Control은 `attached_workspace`·`attached`·`attach_error` 중 하나다. 서버는 descriptor 전에 다른 Control을 보내지 않는다 | attach-behavior "workspace mux" | 순서 |
| O3 | descriptor의 `cols`/`rows`는 새 snapshot과 같은 cut의 크기다. 재attach는 그 크기로 mirror를 맞춘 뒤 새 연결의 프레임을 적용한다 | attach-behavior "리사이즈 전파 (mirror geometry)", remote-attach "화면 동기화" | 순서 |
| O4 | busy·attention·cwd의 최초 값은 descriptor 다음 Busy tick에 나간다 | attach-behavior "workspace mux" | 순서 |
| O5 | `Resize` 확정 echo는 출력과 같은 순서로 온다. client는 Data와 Resize를 도착 순서대로 적용한다 | attach-behavior "리사이즈 전파"·"크기 요청의 응답과 재시도" | 순서 |
| O6 | client→server의 Data·MeshContext·MeshInput·입력 확인 지점(`InputCheckpoint`)은 받은 순서대로 처리한다 | attach-behavior "입력 처리 확인과 View의 선행 배치" | 순서 |
| O7 | 구조 변경은 result → delta → 새 surface snapshot/tap 순서다. ID 대응보다 raw 출력이 먼저 오지 않는다 | attach-behavior "mirror 구조 변경 forward" | 순서 |
| O8 | `Loss`는 마지막으로 보존된 데이터 뒤에 온다(알려진 경합은 tasty 문서에 적힌 대로 남는다) | attach-behavior "client 에게 공백을 알린다" | 순서 |
| O9 | 재attach는 Detach를 보내고 이전 연결의 EOF를 확인한 뒤 새로 연결한다. 서버는 Disconnected를 inbound에 넣은 뒤 소켓을 닫는다 | attach-behavior "통지를 받은 client 가 하는 일" | 순서(쓰기 반쪽 닫기 뒤 EOF) + 링크 경계 |
| O10 | `MeshData` 청크는 한 프레임을 이루는 청크끼리 재조립된다. 손실 뒤에는 full texture부터 다시 구독한다 | attach-behavior "mesh mirror 채널" | 순서 |
| O11 | `MeshFullResendRequest`·`MeshError`는 같은 surface의 mesh 흐름과 함께 처리된다 | attach-behavior "mesh mirror 채널" | 순서 |
| O12 | bulk 파일 청크는 `(transfer_id, seq)` 순서로 쓰이고, 손실이면 결과 불명으로 끝내고 자동 재시도하지 않는다 | attach-behavior "통지를 받은 client 가 하는 일" 표, remote-attach "화면 동기화" | bulk |
| O13 | 프레임 하나는 한 번의 쓰기로 보내고 TCP는 Nagle을 끈다 | attach-behavior "프레임 전송 지연 (Nagle 금지)" | 모든 채널 |
| O14 | heartbeat Ping과 읽기 시간 제한 | attach-behavior "연결 생존 확인 (read timeout + heartbeat)" | 모든 채널(채널마다 따로) |

- mesh는 지금처럼 순서 채널에 둔다. O6이 MeshInput을 Data·확인 지점과 한 순서로 묶기 때문이다. 나중에 대역폭 때문에 나눈다면 서버→client `MeshData`만 옮길 수 있고, MeshContext·MeshInput은 순서 채널에 남아야 하며, 손실 통지가 채널마다 따로 필요하다. 그때 이 표를 먼저 고친다.
- bulk는 지금도 별도 소켓이다(O12). bulk 채널과 순서 채널 사이에 순서 계약은 없다. bulk 완료 인가는 tasty가 commit 시점에 확인한다(`tasty:src/remote/server.rs`).
- 이 레포의 시험은 "한 채널 안 순서"를 tag 종류를 섞은 프레임으로 확인한다. 정본 시험은 `tasty-net-transport`의 `ordered_channel_preserves_mixed_tag_order`(메모리·루프백 TCP·P2P 각 전송)다. O1~O11의 도메인 의미는 tasty 시험이 계속 확인한다.

## 역할

역할 네 가지를 나눈다: SSH 프로세스를 띄운 쪽, 앱 채널을 연 쪽(dialer), workspace를 제공하는 쪽, holder(workspace를 점유해 mirror로 보는 쪽). holder와 workspace 제공 쪽은 항상 서로 반대편이다.

| 경우 | SSH를 띄운 쪽 | dialer | workspace 제공 | holder | 다시 거는 쪽 |
|---|---|---|---|---|---|
| 정방향 SSH | A | A | B | A | A |
| 역방향 SSH (a) `-R` 포트 | A | B | A | B | A가 터널을, B가 앱 채널을 |
| 역방향 SSH (b) `-L` 위 역할 반전 | A | A | A | B | A |
| P2P | 없음 | 연 쪽 X | X 또는 상대 Y | handshake가 정한 쪽 | 연 쪽 X |

**역방향은 (b) 역할 반전으로 한다.** (a)는 만들지 않는다.

- (b)는 B 쪽에 새 리스너를 열지 않는다. (a)는 B의 루프백에 포트를 열어 B의 다른 OS 사용자가 접근할 수 있고, 그 포트와 토큰을 B에 따로 건네야 한다.
- (b)는 sshd의 `PermitListen`·`GatewayPorts` 설정에 영향을 받지 않는다. (a)는 서버 설정에 따라 막힌다.
- (b)는 정방향과 같은 터널 코드(`-L`)를 쓴다. SSH 크레이트에 역방향 터널을 더하지 않는다.
- (b)는 P2P와 같은 모델이다. 어느 경우든 holder를 handshake가 정하고 dialer와 묶지 않는다.
- 대가: B의 서버가 받은 채널에서 holder(mirror를 띄우는 쪽)가 된다. tasty의 "받은 소켓 = holder 후보" 구조가 뒤집히는 일은 tasty 전환 단계에서 tasty가 한다.

handshake의 역할 필드는 `holder: "dialer" | "acceptor"` 하나다. 나머지 역할은 이 값과 전송 종류에서 나온다. 받는 쪽은 `holder = "acceptor"`인 요청(들어온 역방향 제안)을 소비자 콜백에 넘기고, 소비자가 수락·거절을 돌려준다. 사용자가 승인하는 동안 handshake가 기다릴 수 있도록 제안 대기 시간은 `LinkTimeouts`와 별도로 소비자가 준다. 거절이면 `RoleRefused` 사유로 닫고 dialer는 다시 걸지 않는다. 포커스 정책과 수락 UI는 tasty 몫이다.

자기 자신과의 연결은 역방향에서도 거절한다(`tasty:docs/dev-guide/attach-behavior.md` "Self-attach 거절 검증"). handshake가 양쪽 인스턴스 ID를 주고받고 같으면 `SelfAttach`로 거절한다.

## 요청 채널 (포트 기반 조회·생성·상태 확인)

지금 tasty는 포워딩 포트로 일반 JSON-RPC를 보낸다. 원격 목록(`remote.workspaces`), workspace 생성(`tasty:crates/tasty-remote/src/create.rs:36` `workspace.create`), 상태 확인과 자기 인스턴스 판정(`tasty:crates/tasty-remote/src/self_instance.rs:39-42` `system.info`). 이 경로는 인증이 없고, 같은 포트가 모든 IPC 메서드를 연다([위협 모델](threat-model.md) T3).

- 이 레포는 `purpose = "request"` 채널을 준다. handshake v2(토큰·기기 증명)를 마친 뒤 Control 프레임으로 `{id, method, params}` 요청과 `{id, result}` 또는 `{id, error}` 응답을 주고받는다. dialer 쪽 `RequestClient::call(method, params, timeout)`, 받는 쪽 `RequestHandler` trait.
- 이 레포는 메서드를 정의하지 않는다. 받는 쪽 소비자가 허용 목록을 둔다. tasty가 올릴 최소 범위는 `system.info`, `remote.workspaces`, `workspace.create`다. 그 밖의 메서드는 tasty가 필요할 때 목록에 더한다.
- P2P에서도 같은 요청 채널을 지원한다. 목록 조회·생성·상태 확인이 SSH와 P2P에서 같다.
- 자기 인스턴스 판정은 handshake의 인스턴스 ID 비교가 대신한다. `system.info`로 다시 물을 필요가 없다.
- 요청 채널이 생기면 tasty는 포워딩 포트의 일반 JSON-RPC를 막을 수 있다. 막는 일은 tasty 몫이다.
