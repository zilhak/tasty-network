# 전송 추상화, 채널, 역할

`tasty:<경로>`는 tasty 레포 698aa49e03 기준이다. 아래 타입·함수 이름은 `tasty-net-transport`의 정본 이름이다. 시그니처는 구현 때 정하되 이름과 계약은 이 문서를 따른다.

## 용어

| 용어 | 뜻 |
|---|---|
| 전송(transport) | 두 기기 사이에 바이트 채널을 만드는 방법. SSH 정방향, SSH 역할 반전, P2P(`tasty-net-p2p`의 quinn 연결을 쓰는 어댑터), 시험용 메모리 |
| 채널(channel) | 순서를 지키는 양방향 프레임 흐름 하나. SSH에서는 터널을 지나는 TCP 연결 하나, P2P에서는 QUIC 양방향 스트림 하나 |
| 링크(`Link`) | handshake를 마친 채널 묶음. 목적(`purpose`)이 `attach`이면 순서 채널 하나와 나중에 여는 bulk 채널들을 가진다 |
| dialer / acceptor | 채널을 연 쪽 / 받은 쪽. SSH를 누가 띄웠는지, 누가 holder인지와 별개다 |

## 전송 trait

두 층으로 나눈다. 전송마다 다른 것은 아래 층(`RawChannel`)뿐이고, handshake·heartbeat·링크 수명은 위 층이 모든 전송에 같게 한다.

- `RawChannel`: 전송 구현이 제공하는 채널 하나. 읽기 반쪽(`Read`), 쓰기 반쪽(`Write`), 제어(`ChannelControl`: `set_read_timeout`, `set_write_timeout`, `shutdown_write`, `shutdown_and_wake`, P2P의 `close_reason`)로 나눌 수 있다.
- `Transport`: `connect(&self, target, &AttemptCancel) -> Result<Connection, ConnectError>`로 순서 채널을 연다. `Connection`은 그 채널(`channel`)과 이번 연결이 닿은 끝점(`endpoint: Endpoint`)이다. 같은 끝점에 두 번째 채널(bulk·요청)을 여는 `Endpoint::connect_more`가 있고, `Transport::connect_more(&Endpoint, ..)`는 그것을 부른다. 끝점은 SSH 경로의 토큰(발견한 연결 레코드의 것)이나 P2P 연결이 확인한 상대 기기 ID(`P2pConnection::remote_device_id()`)도 들고 있어 handshake가 쓴다. SSH 구현은 터널을 한 번 만들고 그 로컬 포트에 TCP를 다시 연다. P2P 구현은 같은 QUIC 연결에 양방향 스트림을 하나 더 연다.
- `Listener`: 받는 쪽. `accept() -> Result<RawChannel, AcceptError>`. SSH 경로에서는 소비자의 루프백 TCP listener가 받은 소켓을 `RawChannel::from_tcp`로 감싼다. 첫 줄 분기(`stream.open`인지 일반 JSON-RPC인지)는 소비자가 하고, 이미 읽은 바이트를 넘긴다(아래 "받는 쪽 진입").
- 위 층 함수: `dial_handshake`(dialer)와 `accept_handshake`(acceptor)가 `RawChannel`에 handshake v2를 돌려 `Link`(또는 bulk·요청 채널)를 만든다([handshake v2](handshake.md)).

`Link`가 소비자에게 주는 것:

| 항목 | 계약 |
|---|---|
| `take_ordered()` | 순서 채널의 `ChannelReader`·`ChannelWriter`를 한 번만 꺼낸다. 읽기 스레드와 쓰기 스레드가 따로 가진다 |
| `open_bulk(ext)` | 부모 링크의 bulk 자격으로 bulk 채널을 연다([handshake v2](handshake.md) "bulk 자격") |
| `peer()` | handshake가 확인한 상대 정보: 기기 ID, 인스턴스 ID, 역할, capability 목록, 전송 종류. 전송 종류는 `TransportKind{Ssh, Tcp, P2p, Memory}`이고 이 쪽에서 본 값이다. SSH 경로의 받는 쪽은 루프백 listener가 받은 소켓이라 `Tcp`이고, 역할 표(아래 "역할")는 `Ssh`·`Tcp`에 같다 |
| `closer()` | 복제 가능한 `LinkCloser`. 강제 종료와 끝 사유 확정 |
| `Drop` | 그 링크의 모든 채널(이미 꺼낸 순서 채널의 반쪽 포함)을 양쪽으로 닫고 bulk 자격을 폐기하고, 내부 워커 스레드를 join하고, SSH 터널 참조를 놓는다. 그 터널의 마지막 참조였으면 터널 자식을 끝내고 회수한다(아래 "터널 수명과 재연결") |

`ChannelReader::read_frame`은 `tasty-net-frame`의 `read_frame`을 쓰고 실패를 `ReadFailure`로 분류해 돌려준다. `ChannelWriter::write_frame`은 헤더와 payload를 한 버퍼로 모아 `write_all`을 한 번 부르고 `flush`한다(`tasty:crates/tasty-ipc/src/stream.rs` `write_frame`). `write_all`이 안에서 부분 쓰기를 다시 시도하므로 시스템 호출 하나를 보장하지는 않는다.

### 받는 쪽 진입

tasty 서버는 포트 하나에서 첫 줄을 읽어 `stream.open`이면 스트림으로, 아니면 요청·응답 루프로 보낸다(`tasty:src/adapters/production/tcp_ipc_server.rs` `handle_connection`). 그 첫 줄을 읽은 `BufReader`에는 이미 다음 바이트가 들어와 있을 수 있다. `accept_handshake`는 `(RawChannel, first_line, buffered: Vec<u8>)`를 받아 버퍼 바이트를 먼저 읽고 소켓으로 이어 읽는다. 버퍼를 버리면 첫 프레임이 사라진다.

인증 전 읽기의 경계:

- SSH 경로에서는 첫 줄을 소비자가 읽는다. 그래서 첫 줄의 크기 상한과 기한은 소비자 책임이다. tasty는 지금처럼 요청 줄 크기 상한과 첫 줄 전체 기한으로 읽고, 넘으면 거절하고 닫는다(`tasty:src/adapters/production/tcp_ipc_server.rs` `read_first_line`·`read_line_capped`). 이 레포는 같은 일을 하는 도우미 `read_first_line(reader, max_len, deadline)`을 주고, `accept_handshake`는 받은 첫 줄 길이가 `MAX_HANDSHAKE_LINE`(구현이 정하는 상수, tasty 요청 줄 상한 이하)을 넘으면 `malformed`로 거절한다.
- `LinkTimeouts.handshake_timeout`은 첫 줄을 받은 뒤부터 ACK까지의 전체 기한이다. 첫 줄 기한과 합쳐 인증 전 연결 하나가 머무는 시간의 상한이 된다. 구현은 프레임을 읽고 쓰기 전마다 남은 시간을 소켓 timeout으로 다시 걸고, 프레임 하나가 끝난 직후와 도메인 콜백 직전에 전체 기한을 확인해 지났으면 `TimedOut`으로 끝낸다. 다만 프레임 하나 안에서는 부분 읽기·쓰기마다 시스템 호출 timeout이 다시 적용되므로, 상대가 바이트를 조금씩 보내면 그 프레임 하나가 끝나기까지는 기한을 넘을 수 있다. 최악은 프레임 크기(바이트 수)에 남은 시간을 곱한 만큼이고, 그 프레임은 끝난 직후의 확인에서 거절된다. 역할 반전 승인 대기만 이 기한 밖이고 `role_proposal_timeout`이 따로 잰다([handshake v2](handshake.md) "역할 반전 승인의 기한"). 승인 대기는 검사 순서 1~6(전송별 인증: SSH는 토큰·서명, P2P는 페어링 목록의 `remote_id()`와 메시지 1 검사)을 통과한 연결에만 생긴다.
- P2P 경로에서는 스트림을 이 레포가 받으므로 첫 메시지의 크기 상한과 기한도 이 레포가 건다(같은 상수와 `handshake_timeout`).

## 채널 대응

| 채널 | SSH(정방향·역할 반전) | P2P | 메모리(시험) |
|---|---|---|---|
| 순서 채널 | 터널 로컬 포트로 연 TCP 연결 하나. `set_nodelay(true)` | `purpose = attach`로 연 QUIC 양방향 스트림 | 프로세스 안 파이프 한 쌍 |
| bulk 채널 | 같은 로컬 포트로 연 두 번째 TCP 연결(`tasty:src/app/attach_client/bulk.rs:123`과 같음) | 같은 QUIC 연결의 추가 양방향 스트림 | 파이프 한 쌍 더 |
| 요청 채널 | 같은 로컬 포트로 연 TCP 연결. attach 링크 없이 혼자 열 수 있다 | `purpose = request`로 연 양방향 스트림. QUIC 연결의 첫 스트림이어도 되고 attach 링크가 없어도 된다 | 파이프 한 쌍 더 |

채널마다 handshake를 한 번 한다. 첫 메시지의 `purpose`가 `attach`·`bulk`·`request` 중 무엇인지로 받는 쪽이 구분한다.

bulk 채널은 전송 구조상 언제나 부모 링크의 dialer가 열고 acceptor가 받는다. SSH 역할 반전에서 B(holder, acceptor)는 A의 `-L` 로컬 포트에 새 연결을 열 수 없기 때문이다. 데이터를 보내는 방향은 이와 별개로 도메인이 정한다(정방향은 holder인 dialer가 보내고, 역할 반전은 holder인 acceptor B가 A가 연 bulk 채널로 보낸다). 그래서 역할 반전에서 holder B가 파일을 보내려면 A에게 bulk 채널을 열어 달라고 요청해야 하고, 그 요청 메시지는 tasty 도메인 Control이다. 자격 발급·검증 주체는 [handshake v2](handshake.md) "bulk 자격".

## heartbeat와 읽기 시간 제한

- 읽기 시간 제한은 링크마다 정한다. `LinkTimeouts { read_timeout, write_timeout, handshake_timeout, role_proposal_timeout, drain_cap }`를 `dial_handshake`·`accept_handshake`에 넘긴다. 기본 `read_timeout`은 `HEARTBEAT_TIMEOUT`(30초)이다. tasty는 새 연결마다 설정값을 읽어 넘긴다(`tasty:src/app/attach_heartbeat.rs`). 시험은 짧은 값을 넘긴다.
- Ping 송신 정책은 연결 쪽마다 고른다(`tasty:docs/dev-guide/attach-behavior.md` "연결 생존 확인 (read timeout + heartbeat)").
  - `HeartbeatPolicy::WhenIdle`: 송신이 `HEARTBEAT_INTERVAL` 동안 조용할 때만 Ping을 보낸다. tasty 서버 write 스레드의 동작이다.
  - `HeartbeatPolicy::Periodic`: 트래픽과 관계없이 `HEARTBEAT_INTERVAL`마다 보낸다. tasty GUI·CLI 클라이언트의 동작이다.
  - 실제 간격은 `HEARTBEAT_INTERVAL`(5초)과 `read_timeout`의 6분의 1 중 짧은 쪽이다. 시험처럼 `read_timeout`을 짧게 준 링크에서도 상대의 시간 제한보다 Ping이 먼저 간다.
- 채널마다 따로 돈다. bulk 채널도 자기 Ping과 시간 제한을 가진다.
- Ping 송신과 읽기 시간 제한은 성공 ACK 직후부터 양쪽 모두 시작한다. 첫 도메인 프레임(descriptor, 첫 요청 등)이 heartbeat 주기보다 늦어도 연결이 끊기지 않게 하기 위해서다. `ChannelReader`는 Ping을 받아 시간 제한만 갱신하고 소비자에게 넘기지 않는다(지금 tasty client reader도 Ping을 건너뛴다, `tasty:crates/tasty-remote/src/transport.rs`). 그래서 Ping은 O2(첫 Control은 descriptor)나 상태 표의 "첫 도메인 프레임" 규칙에 들어가지 않는다.
- 시간 제한은 소켓 읽기 시간 제한이라 프레임 일부를 읽은 뒤에도 만료될 수 있다(`tasty:crates/tasty-ipc/src/stream/read_failure.rs` 주석). 시간 초과뿐 아니라 어떤 읽기 실패든 헤더나 payload 일부를 이미 소비했을 수 있다(`tasty:crates/tasty-ipc/src/stream.rs` `read_frame` 주석). 그래서 한 번 읽기 실패가 난 `ChannelReader`는 종류와 관계없이 이후 읽기도 같은 실패를 돌려준다. 같은 채널에서 다시 읽어 프레임 경계를 잃는 일을 막는다.

## 읽기 실패 분류

`ReadFailure{HeartbeatTimeout, PeerClosed, Protocol, Other}`는 `std::io::ErrorKind`로 판정한다. 대응은 tasty와 같다(`tasty:crates/tasty-ipc/src/stream/read_failure.rs` `of_io`).

| `io::ErrorKind` | 분류 |
|---|---|
| `WouldBlock`(macOS·Linux 시간 초과), `TimedOut`(Windows 시간 초과) | `HeartbeatTimeout` |
| `UnexpectedEof`, `ConnectionReset`, `ConnectionAborted` | `PeerClosed` |
| `InvalidData`(모르는 tag, 상한 초과) | `Protocol` |
| 그 밖 | `Other` |

`tasty-net-p2p`는 QUIC 오류를 이 `ErrorKind`로 바꿔 돌려주고 어댑터는 그대로 쓴다. 상대가 연결이나 스트림을 닫음 → `ConnectionReset` 또는 `UnexpectedEof`, QUIC idle timeout → `TimedOut`, 이 쪽이 닫음 → `ConnectionAborted`. 그래서 소비자는 전송 종류를 모른 채 같은 분류를 쓴다. 연결 전체가 종료 코드와 함께 닫힌 경우의 사유는 분류가 아니라 `Link::close_reason()`으로 받는다([handshake v2](handshake.md) "QUIC 종료 코드"). 정확한 대응은 `tasty-net-p2p` 시험이 정본이다. 경로가 펀칭된 직접 경로 하나뿐이므로 iroh 설계에 있던 "relay로 되돌아감" 같은 경로 사유는 없다.

## 시도 취소, 닫기, 회수

| 동작 | 계약 | 참고한 tasty 동작 |
|---|---|---|
| 시도 취소 `AttemptCancel` | 연결 시도마다 하나. 시도 중 만든 소켓·스트림과, 아직 다른 채널에 공개하지 않은 SSH 터널·QUIC 연결을 등록한다. `cancel()` 뒤 진행 중인 `connect`·handshake는 정해진 시간 안에 `Cancelled`로 돌아오고 등록된 자원은 닫히고 회수된다. 이미 공개된 터널·연결을 쓰는 시도는 자기 소켓·스트림만 소유한다(아래 "P2P의 닫기 범위"의 "연결 공유의 시작", "터널 수명과 재연결") | `tasty:crates/tasty-remote/src/outbound.rs` `register_socket` |
| 쓰기 반쪽 닫기 `ChannelWriter::half_close` | 상대가 남은 프레임을 끝까지 읽은 뒤 EOF를 본다. TCP는 `shutdown(Write)`, QUIC는 송신 스트림 `finish` | `tasty:crates/tasty-remote/src/transport.rs` `Shutdown::Write` |
| 강제 종료 `LinkCloser::force_close(reason)` | 그 링크의 모든 채널을 닫고 막혀 있는 읽기를 깨운다. TCP는 `shutdown(Both)` 뒤 Windows에서 진행 중 I/O 취소까지 한다. P2P는 그 링크의 스트림만 닫는다(아래 "P2P의 닫기 범위") | `tasty:crates/tasty-utils/src/socket_wake.rs` `shutdown_both_and_wake` |
| 끝 사유 | 사유는 올라가기만 하고 한 번만 확정된다. 먼저 확정한 쪽의 사유가 최종이다. 깨우기는 두 가지로 나눈다. 소켓 I/O 깨우기(`shutdown_and_wake`)는 확정 결과와 관계없이 한다. 소비자에게 끝을 알리는 콜백은 확정에 이긴 쪽만 부른다 | `tasty:crates/tasty-remote/src/transport_end.rs` |
| 회수 | `Link`를 drop하면 내부 워커를 join하고 SSH 터널 참조를 놓는다. 마지막 참조를 놓은 쪽(링크든 요청 채널이든)이 터널 자식을 끝내고 회수한다. 시험이 끝난 뒤 자식 프로세스와 스레드가 남지 않는다 | `tasty:crates/tasty-remote/src/transport.rs` 워커 묶음의 깨우기·join·터널 회수 |

### 종료 순서

tasty attach writer의 종료 순서(`tasty:crates/tasty-remote/src/transport.rs` `spawn_attach_write_thread`·`drain_reader_then_settle`·`settle_by_writer`)를 링크 계약으로 옮긴다.

1. writer가 Detach를 썼거나, 쓰기에 실패했거나, 끝 사유가 이미 올라 있으면 쓰기 반쪽을 닫는다.
2. reader가 끝나기를 기다린다. 기다리는 것은 EOF 자체가 아니라 reader의 종료 표지다. reader는 EOF, 끝내는 프레임(`force_detached` 통지·Detach처럼 소비자가 끝이라고 판정한 프레임), 읽기 실패 중 먼저 오는 것에서 끝난다. 끝내는 프레임을 읽으면 남은 바이트를 더 읽지 않는다. 그 사이 reader가 읽은 통지는 끝 사유를 올린다. reader가 panic해도 종료 표지는 선다.
3. reader가 먼저 끝나면 reader가 확정한다. 안전 상한(`LinkTimeouts.drain_cap`)까지 끝나지 않으면 writer가 사유를 `Lost`로 올리고 → 확정하고 → 양쪽 닫기와 읽기 깨우기를 한다.
4. 소켓 I/O 깨우기는 확정 결과와 관계없이 한다. 소비자에게 끝을 알리는 콜백은 확정에 이긴 쪽만 부른다(위 표 "끝 사유").

받는 쪽(서버)의 지금 순서는 다르다. tasty 서버 writer는 끝내는 프레임을 다 썼으면 쓰기 반쪽만, 아니면 양쪽을 닫고 끝난다. 연결 정리는 그 writer를 join한 뒤 `Disconnected`를 메인 루프에 넣어 점유를 해제한다(`tasty:src/adapters/production/tcp_ipc_server/stream_writer.rs` `close_after_writer`, `tasty:src/adapters/production/tcp_ipc_server.rs` `finish_stream_connection`). 그래서 상대는 점유 해제보다 EOF를 먼저 볼 수 있다. 아래 O9가 이 차이를 다룬다.

블로킹 쓰기에는 쓰기 시간 제한을 건다. 워커 join도 깨우기 뒤에 하므로 막힌 읽기·쓰기 때문에 회수가 끝없이 기다리지 않는다.

### P2P의 닫기 범위

P2P에서 두 endpoint 사이의 QUIC 연결 하나를 그 위의 모든 링크와 채널이 함께 쓴다. 아래 규칙은 transport의 P2P 어댑터가 지킨다. `tasty-net-p2p`는 스트림·연결·종료 코드의 수단만 준다([P2P](p2p.md#스트림과-연결-어댑터와의-경계)). 한 연결 위에 attach 링크 여럿과 요청 채널이 있을 수 있다.

| 일어난 일 | 닫히는 것 | 닫는 방식 | 남는 것 |
|---|---|---|---|
| 한 스트림의 handshake 거절 ACK | 그 스트림 | 정상 종료 | 같은 연결의 다른 링크·채널 |
| 요청 채널 닫기 | 그 스트림 | 정상 종료 | 나머지 |
| 부모 링크의 정상 종료(양쪽이 `half_close`를 마치고 reader가 끝남, 위 "종료 순서") | 그 링크의 순서 스트림과 그 링크를 부모로 연 bulk 스트림 | 순서 스트림은 정상 종료, 진행 중 bulk 스트림은 강제 종료(아래) | 같은 연결의 다른 링크·요청 채널 |
| `LinkCloser::force_close`, `Link` drop, 시도 취소 | 그 링크(또는 시도)의 스트림 | 강제 종료 | 같은 연결의 다른 링크·요청 채널 |
| 페어링 거절(`not_paired`) | QUIC 연결 전체와 그 위 모든 링크·채널. 종료 코드는 [handshake v2](handshake.md) "QUIC 종료 코드" | `Connection::close` | 없음 |
| 연결 위 마지막 링크·채널이 닫힘, endpoint 종료 | QUIC 연결(코드 0 `closed`) | 아래 "연결을 닫는 때" | 없음 |

- 정상 종료는 마지막 프레임을 상대가 읽게 하는 종료다. 송신 스트림을 `finish`한다. 거절 ACK와 요청 채널의 마지막 응답은 이 방식으로 보낸다. 송신 스트림의 완료 확인(`SendStream::stopped`)은 상대 QUIC 스택이 받았다는 뜻일 뿐 상대 앱이 읽었다는 뜻이 아니므로, 그것을 근거로 연결을 닫지 않는다(아래).
- 강제 종료는 남은 바이트를 버린다. 송신 쪽 `reset`과 수신 쪽 `stop`을 쓴다. `force_close`, 시도 취소, 진행 중 bulk를 부모와 함께 끊을 때만 쓴다.
- 연결을 닫는 때: 코드 0 종료는 마지막 데이터를 받는 쪽이 한다. `Connection::close`는 진행 중 작업을 모두 실패시키고, 상대 QUIC 스택에 도착했어도 상대 앱이 아직 읽지 않은 데이터를 버릴 수 있기 때문이다(quinn `Connection::close` 설명, [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §10.2).
  - QUIC 연결 위 스트림은 그 연결의 dialer만 연다. 연결을 받은 쪽이 채널을 열려면 자기 QUIC 연결을 새로 연다. 연결을 받은 쪽이 연 스트림은 받지 않는다(강제 종료). 그래서 그 연결 위 모든 채널에서 채널 dialer = 연결 dialer다.
  - 정상 종료에서 acceptor는 자기 reader가 상대 EOF를 읽은 뒤에만 자기 송신을 `finish`한다(`drain_cap`까지 오지 않으면 그때 `finish`). acceptor가 먼저 끝내고 싶으면 끝내는 프레임(예: `force_detached` 통지)을 보내고 상대의 `half_close`를 기다린다. 거절 ACK만 예외다. 상대가 더 보낼 것이 없으므로 ACK 뒤 바로 `finish`한다. 그래서 dialer가 acceptor의 EOF를 읽었으면 양방향 데이터가 모두 상대 앱에 소비된 뒤다.
  - 그 QUIC 연결을 연 dialer가 닫는다. dialer는 연결 위 마지막 채널의 reader가 acceptor의 EOF까지 읽은 뒤 코드 0으로 닫는다. 거절 ACK와 요청 채널의 마지막 응답도 dialer가 받는 쪽이므로, dialer가 그것을 읽은 뒤에 연결이 닫힌다.
  - acceptor는 마지막 채널이 끝나도 코드 0으로 닫지 않고 상대의 종료를 기다린다. `drain_cap` 안에 오지 않으면 그때 코드 0으로 닫는다. 그 경우 dialer가 아직 읽지 않은 프레임은 잃을 수 있고, dialer는 `ReadFailure::PeerClosed`로 본다.
  - 첫 스트림이 거절돼 공유 링크가 하나도 없는 연결도 같다. 시험은 `tasty-net-p2p`의 `rejected_first_stream_ack_is_read`(거절 ACK의 `reason`을 dialer가 읽음)와 `bidirectional_last_frames_are_read`(양쪽이 마지막 프레임을 보낸 정상 종료에서 두 앱이 모두 상대의 마지막 프레임을 읽음)다.
- 연결 공유의 시작: 새 QUIC 연결은 그 연결을 만든 시도의 첫 채널 handshake가 성공할 때까지 그 시도의 `AttemptCancel`이 소유한다. 그동안 다른 채널은 그 연결을 쓰지 않는다(공개를 기다리거나 시도가 끝나면 새 연결을 연다). handshake가 성공하면 연결을 공유 목록에 공개하고 `AttemptCancel`에서 뺀다. 그 뒤의 연결 수명은 공유 목록이 갖고, 시도 취소는 그 시도의 스트림만 닫는다.

### 터널 수명과 재연결

- SSH 터널은 공유 핸들 `SshTunnel`이 소유한다. `SshTransport`는 공유 키 (대상, 발견한 원격 포트, 원격 `instance_id`)마다 공개된 터널을 하나 두고, `connect`·`connect_more`로 연 채널(attach 링크, bulk, 요청 채널)이 각각 그 핸들의 참조를 가진다. 마지막 참조가 사라지면 터널 자식을 끝내고 회수한다. 그래서 attach 없이 연 요청 채널은 자기가 만든 터널을 혼자 소유하고, attach와 터널을 함께 쓰던 요청 채널은 attach가 먼저 닫혀도 계속 쓸 수 있다. 터널 자체가 죽으면 그 위 모든 채널이 `PeerClosed`로 끝난다.
- 터널 공개는 P2P의 "연결 공유의 시작"과 같은 규칙이다. 새 터널은 그 터널을 만든 시도의 첫 채널 handshake가 성공할 때까지 그 시도의 `AttemptCancel`이 소유하고, 그동안 다른 채널은 쓰지 않는다. 성공하면 공유 목록에 공개하고 `AttemptCancel`에서 뺀다. 그 뒤 시도 취소는 그 시도의 소켓만 닫는다.
- 새 터널 준비 중 취소는 준비 기한(5초, 옛 OpenSSH는 두 번) 뒤에 `Cancelled`로 돌아온다. `tasty-net-ssh`의 준비 대기가 취소를 받지 않기 때문이다. 준비가 성공했든 실패했든 그 사이 취소가 왔으면 결과는 `Cancelled`이고, 띄운 터널은 회수한다.
- 공유 키에 발견 결과가 들어가므로, 재발견한 원격 포트나 `instance_id`가 바뀌면 옛 터널은 남은 참조가 있어도 새 연결에 쓰지 않는다. 옛 터널은 남은 참조가 놓일 때 회수된다.
- 공유 터널에서 채널이 응답 없이 끊겼을 때 "원격 리스너 없음"인지는 채널마다 가린다. 채널을 열기 직전에 `SshTunnel::listener_verdict()`로 받은 판정 핸들로 그 채널의 실패를 판정한다. 그래서 같은 터널에서 이미 세어진 앞선 거절은 뒤에 연 채널의 판정에 섞이지 않는다. 실패 줄은 stderr 중계가 센 시점 기준이라, 중계가 늦으면 앞 채널의 거절이 뒤 채널 판정 핸들을 받은 뒤에 세어져 섞일 수 있다([레포 경계](repo-boundary.md) "포트 발견의 tasty 관례").
- 부모 링크가 끝나면(정상 종료든 강제 종료든) 그 링크의 진행 중 bulk 채널은 이 레포가 닫는다. 터널은 다른 채널이 쓰고 있으면 남는다. P2P도 같게 맞춘다(위 표). bulk가 부모보다 오래 살지 않는다는 규칙은 두 전송에 같다([handshake v2](handshake.md) "bulk 자격").
- 언제 다시 거는지(backoff 간격, 포기 조건, 거절 사유별 재시도 여부)는 소비자가 정한다. 이 레포는 `tasty-net-ssh`의 `Backoff`와 거절 사유 enum만 준다. 인증 거절(`TokenInvalid` 등)은 다시 걸어도 같은 결과이므로 소비자가 재시도하지 않게 거절 사유에 `is_retryable()`을 둔다.
- 다시 걸 때는 포트 발견부터 새로 하고, 발견 결과로 공유 키를 다시 만든다. 원격 인스턴스가 다시 시작하면 포트가 바뀌기 때문이다(`tasty:src/app/auto_attach.rs` `resolve_endpoint`).

## 동기·비동기 경계

- `Transport`, `RawChannel`, `Link`, `ChannelReader`·`ChannelWriter`는 모두 동기 API다. 블로킹 호출은 호출한 스레드를 막는다.
- `tasty-net-p2p`는 `P2pEndpoint`를 만들 때 자기 tokio 런타임(멀티스레드, 작은 워커 수)을 만들어 소유한다. 동기 읽기·쓰기는 그 런타임 핸들의 `block_on`으로 QUIC 스트림을 읽고 쓴다. 시간 제한은 `tokio::time::timeout`으로 건다. 강제 종료는 링크마다 둔 취소 신호로 막힌 `block_on`을 깨우고 그 링크의 스트림을 닫는다.
- 런타임과 endpoint의 종료·회수는 라이브러리가 만든 블로킹 소유 스레드 하나가 한다. 마지막 `P2pEndpoint` 참조가 어디서 사라지든(비동기 작업 안 포함) drop은 그 스레드에 종료를 알리기만 하고, endpoint 닫기와 런타임 회수는 그 스레드에서 일어난다. 런타임 `Drop`이 비동기 문맥에서 기다리는 일을 막는다.
- 동기 API를 비동기 작업 안에서 부르면 안 된다(`block_on`이 패닉한다). P2P 크레이트는 그 경우 오류를 돌려준다(패닉하지 않는다). 판정은 "현재 런타임 핸들이 있는가"가 아니라 "지금 비동기 작업을 실행하는 중인가"다. `spawn_blocking` 스레드에도 런타임 핸들은 있지만 그곳에서 부르는 것은 허용한다. tasty는 다른 곳(HTML HTTP 처리 등)에서 이미 tokio를 쓴다(`tasty:Cargo.toml`, `tasty:crates/tasty-remote/Cargo.toml`). 동기 API를 고른 근거는 attach 호출 경로가 동기 스레드 코드라는 데 있다. tokio 쪽 코드가 이 API를 부를 때는 `spawn_blocking` 같은 블로킹 경계를 거친다.
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
| O6 | client→server의 Data·MeshContext·MeshInput·입력 확인 지점(`InputCheckpoint`)은 받은 순서대로 처리한다. 서버는 앞선 입력을 다 전달한 뒤 같은 sequence의 `InputProcessed`를 server→client로 보낸다. client reader는 구조 Delta의 ID 예약으로 다른 이벤트 적용이 보류돼도 유효한 확인을 바로 소비한다(소비자가 적용 큐를 나눌 때 지켜야 할 경계). 끝난 연결의 입력은 새 연결에 재생하지 않는다 | attach-behavior "입력 처리 확인과 View의 선행 배치" | 순서(양방향 각각) |
| O7 | 구조 변경은 result → delta → 새 surface snapshot/tap 순서다. ID 대응보다 raw 출력이 먼저 오지 않는다 | attach-behavior "mirror 구조 변경 forward" | 순서 |
| O8 | `Loss`는 마지막으로 보존된 데이터 뒤에 온다(알려진 경합은 tasty 문서에 적힌 대로 남는다) | attach-behavior "client 에게 공백을 알린다" | 그 손실이 난 채널. 순서 채널의 손실은 순서 채널로, bulk 전송의 손실은 그 bulk 채널로(O12) |
| O9 | 재attach는 Detach를 보내고 이전 연결의 reader 종료(위 "종료 순서" 2)를 확인한 뒤 새로 연결한다. 지금 tasty 서버는 소켓을 닫은 뒤 `Disconnected`를 메인 루프에 넣으므로, 이전 점유의 해제가 새 연결의 점유 요청보다 먼저라는 것은 전송이 보장하지 않는다. 그 선후가 필요하면 받는 쪽 소비자가 점유 처리(tasty 메인 루프)에서 같은 holder의 이전 연결 정리를 먼저 처리한다 | attach-behavior "통지를 받은 client 가 하는 일" | 순서(쓰기 반쪽 닫기 뒤 reader 종료) + 링크 경계. 점유 해제 선후는 소비자 |
| O10 | `MeshData` 청크는 한 프레임을 이루는 청크끼리 재조립된다. 손실 뒤에는 full texture부터 다시 구독한다 | attach-behavior "mesh mirror 채널" | 순서 |
| O11 | `MeshFullResendRequest`·`MeshError`는 같은 surface의 mesh 흐름과 함께 처리된다 | attach-behavior "mesh mirror 채널" | 순서 |
| O12 | bulk 전송은 한 bulk 채널 안에서 양방향으로 끝난다. 보내는 쪽은 손실 통지 선언(`ClientLossNotify`) → `BulkBegin` → Data 청크(`(transfer_id, seq)`) → `BulkCommit` 순서로 쓰고, 받는 쪽은 그 전송의 `BulkResult` 또는 `Loss`를 같은 bulk 채널로 돌려준다. 보내는 쪽은 같은 채널에서 그 응답을 읽어 결과를 정한다(`tasty:src/app/attach_client/bulk.rs`). 손실이면 결과 불명으로 끝내고 자동 재시도하지 않는다 | attach-behavior "통지를 받은 client 가 하는 일" 표, remote-attach "화면 동기화" | bulk |
| O13 | 프레임 하나는 한 버퍼로 모아 `write_all` 한 번 + `flush`로 보내고 TCP는 `TCP_NODELAY`를 켠다 | attach-behavior "프레임 전송 지연 (Nagle 금지)" | 모든 채널 |
| O14 | heartbeat Ping과 읽기 시간 제한 | attach-behavior "연결 생존 확인 (read timeout + heartbeat)" | 모든 채널(채널마다 따로) |
| O15 | HTML entry 전송은 같은 attach registration에 묶인 요청 → metadata → read credit → chunk → complete 순서다. 끊긴 registration의 응답은 새 registration에 넘기지 않고, 손실이 난 본문을 완료로 처리하지 않는다 | attach-behavior "HTML entry 전송", `tasty:docs/dev-guide/html-transfer.md` | 순서. bulk 채널이나 요청 채널로 옮기지 않는다. metadata·credit·완료의 도메인 규칙은 tasty |

- mesh는 지금처럼 순서 채널에 둔다. O6이 MeshInput을 Data·확인 지점과 한 순서로 묶기 때문이다. 나중에 대역폭 때문에 나눈다면 서버→client `MeshData`만 옮길 수 있고, MeshContext·MeshInput은 순서 채널에 남아야 하며, 손실 통지가 채널마다 따로 필요하다. 그때 이 표를 먼저 고친다.
- bulk는 지금도 별도 소켓이다(O12). bulk 채널과 순서 채널 사이에 순서 계약은 없다. bulk의 시작 선언·데이터·완료와 그 결과·손실 통지는 모두 그 bulk 채널 안에서 오간다. 지금 tasty에서는 `StreamHub`가 연결과 `bulk_workspace` 결속을 기록하고(`register_bulk`), 시작(`BulkBegin`)과 완료(`BulkCommit`) 인가는 도메인이 한다(`tasty:src/remote/server.rs`). 이 레포의 bulk 자격은 그 앞에 새로 더하는 검사이고, 도메인의 시작·완료 인가는 그대로 남는다.
- 이 레포의 시험은 "한 채널 안 순서"를 tag 종류를 섞은 프레임으로 확인한다. 정본 시험은 전송마다 그 전송을 가진 크레이트에 둔다. 메모리·루프백 TCP는 `tasty-net-transport`의 `ordered_channel_preserves_mixed_tag_order`(handshake를 마친 루프백 링크는 `tests/loopback.rs`의 `a_handshaken_link_keeps_the_order_of_mixed_tag_frames`), P2P는 `tasty-net-transport`의 `tests/`에 있는 `p2p_ordered_stream_preserves_mixed_tag_order`(어댑터 위, 실제 P2P endpoint 두 개와 프로세스 안 rendezvous 서버)다. 프레임 순서는 어댑터 위의 계약이므로 transport에 둔다. `tasty-net-p2p`는 프레임을 모르고, 자기 시험에서는 바이트 순서만 본다. O1~O11과 O15의 도메인 의미는 tasty 시험이 계속 확인한다.

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
- 대가: 지금 tasty에서는 연결을 받은 서버가 workspace 제공 쪽이고(점유·snapshot을 dispatch, `tasty:src/adapters/production/tcp_ipc_server.rs` `dispatch_stream_attach`), 연결을 건 client가 holder다. (b)에서는 받은 쪽 B가 holder(mirror를 띄우는 쪽)가 되고 연 쪽 A가 workspace를 제공한다. 이 구조를 tasty에 넣는 일은 tasty 전환 단계에서 tasty가 한다.

handshake의 역할 필드는 `holder: "dialer" | "acceptor"` 하나다. 나머지 역할은 이 값과 전송 종류에서 나온다. 받는 쪽은 `holder = "acceptor"`인 요청(들어온 역방향 제안)을 소비자 콜백에 넘기고, 소비자가 수락·거절을 돌려준다. 사용자가 승인하는 동안 handshake가 기다릴 수 있도록 제안 대기 시간 `LinkTimeouts.role_proposal_timeout`을 소비자가 준다. `handshake_timeout`과의 관계와 늦은 승인의 처리는 [handshake v2](handshake.md) "역할 반전 승인의 기한"이다. 거절이면 `RoleRefused` 사유로 닫고 dialer는 다시 걸지 않는다. 포커스 정책과 수락 UI는 tasty 몫이다.

ACK 뒤 순서 채널의 첫 Control(descriptor, O2)은 언제나 workspace 제공 쪽이 보낸다. 정방향은 acceptor, 역할 반전은 dialer A다. 역할 반전에서 A는 ACK(B→A)를 받은 다음 descriptor를 보내고, B는 ACK를 보낸 뒤 descriptor를 기다린다.

자기 자신과의 연결은 역방향에서도 거절한다(`tasty:docs/dev-guide/attach-behavior.md` "Self-attach 거절 검증"). handshake가 양쪽 인스턴스 ID를 주고받고 같으면 `SelfAttach`로 거절한다. P2P에서 자기 기기 ID로 걸면 `tasty-net-p2p`가 네트워크에 보내기 전에 `P2pConnectError::SelfAttach`로 끝내고(서버도 `self`로 거절한다), 어댑터는 그것도 `SelfAttach`로 분류한다([P2P](p2p.md#endpoint와-런타임)).

## 요청 채널 (포트 기반 조회·생성·상태 확인)

지금 tasty는 포워딩 포트로 일반 JSON-RPC를 보낸다. 원격 목록은 두 층이다: 공개 메서드 `remote.workspaces`(로컬 tasty)가 포워딩 포트로 `workspace.list`를 부르고 `attach.list`의 점유 정보를 합친다(`tasty:crates/tasty-remote/src/browse.rs` `browse_via_port_bound`). workspace 생성은 `workspace.create`(`tasty:crates/tasty-remote/src/create.rs:36`), 상태 확인과 자기 인스턴스 판정은 `system.info`(`tasty:crates/tasty-remote/src/self_instance.rs:39-42`)다. 이 경로는 인증이 없고, 같은 포트가 모든 IPC 메서드를 연다([위협 모델](threat-model.md) T3).

- 이 레포는 `purpose = "request"` 채널을 준다. 받는 쪽은 검사 1~6을 통과한 요청 채널마다 도메인 콜백 `DomainPolicy::accept_request(peer)`를 부른다(기본은 수락, 거절은 `domain` 사유). handshake v2(토큰·기기 증명)를 마친 뒤 Control 프레임으로 `{id, method, params}` 요청과 `{id, result}` 또는 `{id, error}` 응답을 주고받는다. dialer 쪽 `RequestClient::call(method, params, timeout)`, 받는 쪽 `RequestHandler` trait.
- 이 레포는 메서드를 정의하지 않는다. 받는 쪽 소비자가 허용 목록을 둔다. 요청 채널로 원격에 보내는 최소 범위는 `workspace.list`, `attach.list`(점유 표시용, 또는 같은 정보를 주는 응답), `workspace.create`, `system.info`다. `remote.workspaces`는 로컬 쪽 메서드로 남아 이 요청들을 부른다. 그 밖의 메서드는 tasty가 필요할 때 목록에 더한다.
- P2P에서도 같은 요청 채널을 지원한다. 목록 조회·생성·상태 확인이 SSH와 P2P에서 같다.
- 자기 인스턴스 판정은 handshake의 인스턴스 ID 비교가 대신한다. `system.info`로 다시 물을 필요가 없다.
- 요청 채널이 생기면 tasty는 포워딩 포트의 일반 JSON-RPC를 막을 수 있다. 막는 일은 tasty 몫이다.

## 연결 목록

사용자 결정(2026-11-27, 원문 "연결 자체를 위한 자동 동작은 만들어야하는데, 네트워크 장치간 어떻게 연결되어있는지 명확해야한단거야"): 라이브러리는 "지금 이 기기가 어떤 기기와, 어떤 경로로, 어떤 주소·포트 쌍으로, 어느 rendezvous 서버를 통해 소개받아 연결돼 있는지"를 조회할 수 있는 목록으로 준다.

- 자리: `tasty-net-transport`의 `ConnectionTable`이다. 세 경로(P2P, SSH 정방향, SSH 역할 반전)를 모두 담아야 하고, 세 경로를 다 아는 크레이트가 transport뿐이기 때문이다. `tasty-net-p2p`는 자기 연결 정보(`P2pConnectionInfo`, [P2P](p2p.md#연결-정보))만 내보내고, transport가 그것을 읽어 항목을 채운다. P2P 크레이트는 transport에 의존하지 않는다([크레이트 구성](crates.md#p2p-크레이트-격리)).
- 조회: `ConnectionTable::connections() -> Vec<ConnectionInfo>`(스냅샷), `ConnectionTable::events()`(전이 이벤트, 큐 상한은 설정값). `Transport`·`Listener`·handshake는 연결을 열고 받을 때 이 표에 항목을 만들고 상태를 바꾼다. 소비자가 항목을 직접 만들거나 고치는 API는 없다.
- 항목 하나는 "한 상대에 닿은 한 물리 연결"이다. SSH는 터널 하나(공유 키 하나, 위 "터널 수명과 재연결"), P2P는 QUIC 연결 하나다. 그 위의 링크·채널은 항목 안의 개수와 목록으로 담는다.

| 필드 | 뜻 | SSH 정방향·역할 반전(SSH를 띄운 쪽) | SSH 받는 쪽(루프백 listener) | P2P |
|---|---|---|---|---|
| `peer` | 상대 기기 ID, 인스턴스 ID(handshake 뒤), 소비자가 준 표시 이름 | handshake가 확인한 값 | handshake가 확인한 값 | QUIC가 확인한 기기 ID + handshake의 인스턴스 ID |
| `path` | `PathKind` | `ssh_forward` · `ssh_role_reversed` | `ssh_inbound`(루프백으로 받은 연결. 이 쪽은 SSH인지 알 수 없고 루프백 포트가 SSH 포워딩 전용이라는 tasty의 전제를 따른다, [위협 모델](threat-model.md) T2·T3) | `p2p_udp` |
| `local_addr`, `remote_addr` | 주소·포트 쌍 | 로컬 포워딩 포트(`127.0.0.1:<포트>`) ↔ SSH 대상(`<호스트>:<포트>`, 프로필 값 그대로)과 원격 리스너 포트 | 루프백 listener 주소 ↔ 루프백 상대 주소 | P2P 소켓 주소 ↔ 펀칭으로 얻은 상대 주소(경로 이동 뒤에는 새 주소) |
| `via` | 소개·경유 | `ssh{target, remote_port}` | `loopback` | `rendezvous{server_addr, server_key}`와 성공 후보 종류, 양쪽 `NatMapping` |
| `state` | `connecting` · `connected` · `reconnecting` · `disconnected` | 터널 준비·handshake 중 / 링크 성립 / 재연결 대기·시도 중 / 끝 | handshake 중 / 성립 / (받는 쪽은 재연결하지 않는다) / 끝 | P2P 상태 기계의 `Requesting`~`Handshaking` / `Connected` / 재연결 / `Closed` |
| `phase` | 세부 단계 | 터널 준비, 포트 발견, handshake | handshake | P2P 상태 기계의 상태 이름과 값 |
| `last_cause` | 마지막 전이 원인 | 터널 종료, 원격 리스너 없음, 거절 사유, heartbeat 만료 등 | 거절 사유, 상대 닫음 등 | P2P 전이 원인(종료 코드 포함) |
| `history` | 전이와 경로 바뀜 이력 | 상태 전이, 재연결로 바뀐 원격 포트·`instance_id` | 상태 전이 | 상태 전이, `remote_address_changed` |
| `links` | 그 위의 링크·채널 | attach 링크(holder 포함), bulk·요청 채널 수 | 같음 | 같음 |
| `rtt` | 지연 | 없음(SSH는 재지 않는다) | 없음 | quinn RTT |
| `applied` | 이 연결에 적용된 설정값과 진행 값 | 재연결 시도 번호, 다음 시도까지 남은 시간, 적용된 시간 값 | 적용된 시간 값 | P2P 연결의 `applied`·`attempt`를 그대로 |

- 이력 상한: 항목마다 `connections.history_limit`(기본 32)개다. 넘치면 오래된 것부터 버리고 버린 수를 센다. `disconnected` 항목은 `connections.keep_closed`(기본 16)개까지 최근 것만 남기고 지운다. 끊긴 이유를 사용자가 볼 수 있게 하되 목록이 끝없이 자라지 않게 하기 위해서다.
- 재연결: `reconnecting`은 transport의 재연결 도우미 `Redialer`가 그 항목을 다시 거는 동안의 상태다. `Redialer`는 설정값(처음 간격, 배수, 상한, 무작위 폭, 횟수 상한)으로 backoff를 하고, 거절 사유의 `is_retryable()`이 false면 멈춘다. 소비자가 `Redialer`를 쓰지 않고 직접 다시 걸면 옛 항목은 `disconnected`로 끝나고 새 항목이 `connecting`으로 생긴다. 어느 쪽이든 목록이 지금 일어나는 일을 보인다. 지금 "언제 다시 거는지는 소비자가 정한다"(위 "터널 수명과 재연결")는 규칙은 그대로이고, `Redialer`는 그 결정을 설정값으로 맡길 수 있는 선택지다.
- 토폴로지와의 관계: topology의 자기 엣지 보고(`LocalLink`)는 이 목록에서 만든다. `path`가 엣지의 `transport` 값이 되고 `rtt`가 `rtt_ms`가 된다([토폴로지](topology.md#엣지-목록-입력)). 주소·서버 필드는 원격 응답에 싣지 않는다.
- 설정값: `connections.history_limit`, `connections.keep_closed`, `connections.event_queue_limit`, `Redialer`의 값은 [크레이트 구성](crates.md#설정값-규칙)의 규칙을 따른다. transport의 나머지 기존 상수는 그 규칙의 후속 작업 대상이다.
