# P2P 전송 (quinn + rendezvous)

사용자 결정(2026-11-27): P2P는 iroh를 쓰지 않고 직접 구현한다. 원문은 "내가 원하는건 iroh의 철학과 맞지 않아보이는데. 제어된 환경과 네트워크 효율성을 직접 설정해서 쓰는게 내가 원하는 방향이잖아"이다. 구조는 셋이다.

1. rendezvous 서버는 기기에게 바깥에서 보이는 주소를 알려 주고(STUN 같은 반사), 같은 서버에 붙은 두 기기의 동시 펀칭을 조정한다. 데이터를 중계하는 기능은 없다. 서버에 중계 코드 경로 자체를 두지 않는 구조로 보장한다([rendezvous](rendezvous.md)).
2. 펀칭된 UDP 소켓 위에서 quinn QUIC으로 연결한다. 인증은 기기 키로 한다(아래 "인증").
3. 이미 정한 정책이 기본 동작이다. 아래 "정책"에 있다.

이 문서는 기기 쪽 크레이트 `tasty-net-p2p`를 다룬다. 서버 프로토콜·서버 상태·배포는 [rendezvous](rendezvous.md)에 있다. 크레이트 경계와 의존 방향의 정본은 [크레이트 구성](crates.md)이다.

## 구성 요소와 크레이트

| 구성 요소 | 자리 | 맡는 것 |
|---|---|---|
| P2P 전송 라이브러리 | `crates/tasty-net-p2p` | UDP 소켓과 패킷 나누기, NAT 판별, 펀칭, quinn 연결, 기기 키 인증, 페어링 허용 목록, 동기 API와 소유 런타임 |
| rendezvous 프로토콜과 서버 | `crates/tasty-net-rendezvous`(라이브러리 + 실행 파일 `tasty-rendezvous`) | 조정 메시지 형식, STUN 부분 집합 코덱, 펀칭 프로브 형식, 기기 키 raw public key TLS 설정, 서버 본체 |
| 전송 어댑터 | `tasty-net-transport`의 P2P 모듈 | `tasty-net-p2p`의 스트림을 `RawChannel`로 감싸고 handshake v2·링크·연결 공유를 얹는다([전송 추상화](transport.md)) |
| 배포물 | `rendezvous/`(지금의 `relay/`를 바꾼다) | 서버 컨테이너 설정 예시와 검사, 운영 안내([rendezvous](rendezvous.md#배포-형태)) |

- 격리: `tasty-net-p2p`와 `tasty-net-rendezvous`는 이 workspace의 SSH·토폴로지·전송·프레임 크레이트에 의존하지 않는다. 공용 크레이트는 `tasty-net-identity` 하나다(근거는 [크레이트 구성](crates.md#p2p-크레이트-격리)). 전송 쪽이 P2P를 어댑터로 쓰고, 반대 방향 의존은 없다.
- `tasty-net-p2p`는 `tasty-net-rendezvous`의 라이브러리(메시지 형식·코덱·TLS 설정)를 쓰고, 시험에서는 같은 크레이트의 서버를 프로세스 안에서 띄운다. 서버 크레이트는 `tasty-net-p2p`에 의존하지 않는다. 그래서 둘 사이에 순환이 없다.
- iroh·iroh-relay·iroh-base 의존은 설계에서 없앤다. 기기 키의 ed25519 구현도 `iroh-base` 대신 `ed25519-dalek`을 직접 쓴다([기기 키·토큰](identity.md)).

### 외부 의존과 고정

| 의존 | 쓰는 크레이트 | 쓰는 것 |
|---|---|---|
| `quinn` | p2p, rendezvous | QUIC endpoint·연결·스트림. `default-features = false`로 두고 `runtime-tokio`·`rustls-ring`(과 로그용 `log`)만 켠다. 기본 feature의 `platform-verifier`·`bloom`은 빠진다(신뢰는 기기 키로 정하고, 0-RTT를 쓰지 않아 재전송 방지 필터가 필요 없다) |
| `rustls` | p2p, rendezvous | TLS 1.3과 RFC 7250 raw public key. `default-features = false`, `features = ["ring", "std", "logging"]`. 기본 feature의 `aws_lc_rs`·`tls12`·`prefer-post-quantum`은 넣지 않는다. 설정을 만들 때 ring provider를 명시한다(`builder_with_provider`) |
| `tokio` | p2p, rendezvous | 런타임, UDP 소켓, 타이머 |
| `ed25519-dalek` | identity, rendezvous(간접으로 p2p) | 기기 키 서명·검증. rendezvous는 약한 키 검사(`VerifyingKey::is_weak`)만 직접 쓰고 identity와 같은 판(`=3.0.0`, `default-features = false`)을 고정한다 |
| `serde`·`serde_json` | rendezvous, p2p | 조정 메시지(rendezvous), 페어링 메시지(p2p) |
| `toml` | rendezvous | 서버 설정 파일(`default-features = false`, `std`·`parse`·`serde`) |
| `socket2` | rendezvous, p2p | IPv6 소켓을 묶기 전에 `IPV6_V6ONLY`를 끄고([rendezvous](rendezvous.md#광고-주소와-바인딩-주소) 이중 스택) 묶인 소켓의 값을 읽는다. p2p는 소켓 버퍼 크기(`socket.recv_buffer`·`socket.send_buffer`)도 이것으로 정하고 읽는다. quinn의 UDP 계층과 같은 판(0.6) |
| `getrandom` | rendezvous, p2p | OS 난수. 펀칭 토큰(rendezvous), 재연결 지연의 무작위 폭(p2p) |
| `libc`(Unix)·`windows-sys`(Windows) | rendezvous | 경로보다 큰 보내기의 오류 번호 `EMSGSIZE`·`WSAEMSGSIZE`(아래 "소켓 하나와 패킷 나누기"). `windows-sys`는 `Win32_Networking_WinSock`만 켠다 |

- 판은 정확히 고정한다(`=` 지정). 2026-11-27 확인 기준 최신은 quinn 0.11.12(`rustls ^0.23.5`를 선택 의존으로 둔다, `rust-version = "1.85"`), rustls 0.23.46(`rust-version = "1.71"`)이다. raw public key resolver가 0.23.16부터 있으므로 rustls는 그 이상이어야 한다([quinn](https://docs.rs/crate/quinn/latest), [rustls](https://docs.rs/rustls/latest/rustls/)). 구현 작업이 그때의 판을 다시 보고 고정하며, 올릴 때는 이 문서의 API 서술을 새 판으로 다시 확인하고 같은 커밋에서 고친다([버전·호환](versioning.md#의존-고정)).
- crypto provider를 `ring`으로 고르는 근거: rustls 0.23의 기본 provider는 `aws_lc_rs`라 기본 feature를 그대로 두면 두 provider가 함께 들어온다. 하나만 두려면 고르고 기본 feature를 꺼야 한다. 둘 다 C 컴파일러가 필요하다. `aws-lc-rs`는 비 FIPS 빌드에 CMake가 필요 없지만, Windows x86·x86-64에서는 NASM이 필요하다. NASM이 없을 때 쓸 수 있는 미리 빌드한 객체(`prebuilt-nasm`)는 Windows x86-64에만 있고, Windows x86에서는 NASM 없이 빌드되지 않는다([aws-lc-rs 빌드 안내](https://github.com/aws/aws-lc-rs/blob/main/aws-lc-rs/README.md#build)). quinn의 기본 TLS feature도 `rustls-ring`이다. 그래서 OS마다 조건이 갈리지 않는 ring으로 고른다. TLS 1.2는 QUIC에 쓰이지 않으므로 `tls12`를 뺀다.

## 소켓 하나와 패킷 나누기

- 기기는 P2P용 UDP 소켓을 하나만 연다. rendezvous 서버로 가는 QUIC 연결, STUN 반사 질의, 펀칭 프로브, 상대와의 QUIC 연결이 모두 이 소켓을 지난다. 서버가 보는 바깥 주소가 상대에게 줄 주소와 같은 NAT 매핑이 되게 하고, 서버 연결의 keepalive가 그 매핑을 살려 두게 하기 위해서다.
- quinn에는 이 소켓을 직접 주지 않고 감싼 소켓(quinn의 `AsyncUdpSocket` 구현)을 준다. 감싼 소켓 아래의 실제 입출력은 trait `PacketIo`(보내기·받기·로컬 주소)이고, 기본 구현이 OS UDP 소켓이다(시험은 모의 NAT를 넣는다, [CI와 시험 환경](testing.md#모의-nat)). 받은 패킷은 첫 바이트로 나눈다([RFC 7983](https://www.rfc-editor.org/rfc/rfc7983) §7, [RFC 9443](https://www.rfc-editor.org/rfc/rfc9443) §3).

  | 첫 바이트 | 보내는 곳 |
  |---|---|
  | 0–3 | STUN(서버의 반사 응답, 상대의 프로브와 그 응답). P2P 크레이트가 처리한다 |
  | 64–127 | QUIC 짧은 헤더. quinn에 넘긴다(이 소켓에는 TURN이 없어 RFC 9443의 64–79 예외가 생기지 않는다) |
  | 192–255 | QUIC 긴 헤더(Initial·Handshake 등, [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §17.2). quinn에 넘긴다 |
  | 4–63, 128–191 | 버린다(버린 수를 센다) |

- QUIC bit를 비틀어 보내는 확장([RFC 9287](https://www.rfc-editor.org/rfc/rfc9287))을 양쪽 모두 끈다(quinn `EndpointConfig::grease_quic_bit(false)`, 기본은 켬). RFC 9443 §1도 다른 프로토콜과 나눠 쓰는 포트에서는 `grease_quic_bit`를 보내지 말라고 한다. 켜 두면 고정 비트(0x40)가 0인 QUIC 패킷이 온다. 짧은 헤더는 첫 바이트가 0–63, 긴 헤더(Initial·Handshake·Retry 등에도 적용될 수 있다, RFC 9287 §3.1)는 128–191이 되어, 두 범위 모두 위 분류에서 QUIC으로 가지 않고 버려지거나(4–63, 128–191) STUN과 섞인다(0–3).
- 분할 금지: 감싼 소켓의 `may_fragment()`는 `PacketIo::may_fragment()`를 그대로 넘긴다. quinn은 이 값이 거짓일 때만 MTU 탐색(`quic.mtu_discovery.*`)을 돌린다. 기본 구현(OS UDP 소켓)은 quinn의 UDP 계층(`quinn::udp::UdpSocketState`)으로 분할 금지를 켜고 그 결과를 돌려준다. 이 계층이 켜는 받기 합치기(GRO 등)로 한 번에 온 여러 데이터그램은 하나씩 나눠 넘긴다. 모르는 구현(모의 NAT 등)은 기본값 참으로 탐색을 끈다.
- 받기 오류: 데이터그램 하나에 딸린 오류(ICMP 오류가 알려진 `ConnectionReset`·`ConnectionRefused`, Windows에서 버퍼보다 큰 데이터그램의 `WSAEMSGSIZE` 등)는 그 데이터그램을 버리고 세며 다음 데이터그램을 받는다. quinn은 `ConnectionReset` 밖의 받기 오류에 드라이버를 끝내므로 감싼 소켓이 걸러 준다. 버린 뒤에는 실행기를 한 번 돌려준다. 소켓이 망가졌다는 오류(`NotConnected`·`BrokenPipe`·`UnexpectedEof`·`Unsupported`)만 quinn에 넘긴다.
- 보내기 오류: 경로가 받지 않는 크기라 OS가 그 자리에서 거절한 보내기(`EMSGSIZE`, Windows `WSAEMSGSIZE`)는 그 데이터그램을 버리고 세며 quinn에는 성공으로 돌려준다. 분할 금지를 켜면 MTU 탐색 probe가 인터페이스 MTU보다 클 수 있고, quinn은 `WouldBlock` 밖의 보내기 오류에 연결 드라이버를 끝내기 때문이다. quinn의 UDP 계층(`UdpSocketState::send`)도 이 오류를 잃은 probe로 삼킨다. quinn은 답이 없는 probe를 잃은 것으로 보고 더 작은 크기로 탐색을 잇는다. 감싼 소켓이 걸러 주므로 주입한 `PacketIo`에도 같다.
- 분류는 단위 시험 `demux_classifies_first_byte`(경계값 0·3·4·63·64·127·128·191·192·255)가 고정한다. 서버의 주 포트도 같은 분류 코드를 쓴다.
- 소켓은 `[::]:0` 이중 스택으로 연다. OS가 이중 스택을 받지 않으면 IPv4 `0.0.0.0:0`으로 연다. 반사 주소는 서버에 닿은 주소족 하나에서만 얻는다. 전역 IPv6 주소는 사설 후보와 같이 호스트 후보로 보낸다(NAT 없이 닿을 수 있다). IPv6 경로가 실제로 쓰이는지는 펀칭 실험에서 본다.
- 호스트 후보의 IP: 소켓이 묶인 IP(와일드카드가 아닐 때), 와일드카드면 소켓이 받는 주소족마다 OS가 경로에서 고르는 로컬 IP, 그리고 `P2pConfig.host_ips`로 준 IP다. 서버의 주소족은 서버로 가는 경로(같은 주소족의 UDP 소켓을 서버 주소로 `connect`한 뒤의 `local_addr`, 패킷은 보내지 않는다), 다른 주소족은 그 주소족의 문서용 주소(IPv6 `2001:db8::1`, IPv4 `192.0.2.1`)로 가는 경로를 같은 방법으로 묻는다. 그래서 IPv4 서버를 써도 이중 스택 소켓은 전역 IPv6 호스트 후보를 찾는다. IPv4로 연 소켓은 IPv4만, 이중 스택이 아닌 IPv6 소켓은 IPv6만 묻는다. 포트는 P2P 소켓의 포트다. 모두 후보 주소 규칙([rendezvous](rendezvous.md#후보-주소-규칙))으로 거른 뒤 IPv4를 먼저 보낸다.
- 소켓 수신·송신 버퍼 크기는 설정값이다(`socket.recv_buffer`·`socket.send_buffer`, 기본은 OS 값이고 조회에 실제 값과 출처 `os_default`가 보인다).

## NAT 판별

목적은 매핑 성질(같은 내부 주소·포트가 목적지마다 같은 바깥 주소·포트를 받는지)을 알아 펀칭 후보를 정하는 것이다. 용어는 [RFC 4787](https://www.rfc-editor.org/rfc/rfc4787) §4.1의 endpoint-independent mapping(EIM)·address-dependent mapping(ADM)·address and port-dependent mapping(APDM)이다.

- 서버 조건: 서버는 STUN Binding을 주 포트 P1과 보조 포트 P2(같은 IP)에서 받는다. 두 번째 공인 IP(IP2)가 있으면 IP2:P1, IP2:P2에서도 받는다. 서버는 `welcome.stun`에 보조 주소(IP1:P2, IP2:P1, IP2:P2)를 싣는다. 주 주소는 기기가 접속한 주소를 쓴다([rendezvous](rendezvous.md#광고-주소와-바인딩-주소)).
- 기기는 같은 소켓에서 주 주소와 보조 주소들로 STUN Binding Request를 보내고 응답의 XOR-MAPPED-ADDRESS([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §14.2)를 비교한다. 대상마다 `nat.stun_rto`(기본 500 ms) 간격으로 `nat.stun_attempts`(기본 3)번까지 보낸다. 대상들은 동시에 묻는다. RFC 8489 §6.2.1은 재전송마다 RTO를 두 배로 늘리지만, 여기서는 대상 수가 적고 응답 없음을 `unknown`으로 두면 되므로 고정 간격으로 줄인 것이다. 응답은 transaction ID가 맞고 보낸 대상 주소에서 온 것만 받는다([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §6.3).
- 비교 방법:
  - IP2가 있으면 [RFC 5780](https://www.rfc-editor.org/rfc/rfc5780) §4.3의 시험 II(IP2:P1)와 시험 III(IP2:P2)에 해당하는 비교를 한다. IP1:P1과 IP2:P1이 같으면 EIM, 다르고 IP2:P1과 IP2:P2가 같으면 ADM, 그것도 다르면 APDM이다.
  - 같은 IP의 두 포트(IP1:P1과 IP1:P2) 비교는 RFC 5780의 시험이 아니라 이 설계가 더한 것이다. 다르면 포트 의존(APDM)이 확실하다. 같으면 EIM이거나 ADM이고 둘을 가를 수 없다.

  | 결과 | `NatMapping` |
  |---|---|
  | IP2가 있고, 질의한 대상 모두 응답했고 모두 같다 | `independent` |
  | IP2가 없고(서버가 광고하지 않음) IP1:P1과 IP1:P2가 같다 | `independent_port_only`(잠정. ADM일 수 있다) |
  | 응답 중 하나라도 다르다 | `dependent` |
  | 광고된 대상 중 응답이 빠진 것이 있고, 받은 응답끼리는 같다 | `unknown`(빠진 대상을 함께 보고한다) |
  | 응답이 하나뿐이거나 없다 | `unknown` |

- 필터링 성질(RFC 4787 §5)은 판별하지 않는다. RFC 5780의 CHANGE-REQUEST가 필요하고, 판별 결과가 시도 여부를 바꾸지 않기 때문이다. 시도해 보고 결과로 판정한다.
- 판별은 서버에 등록한 직후와 `reflexive_changed`를 받을 때 다시 한다. 결과와 대상별 응답(`stun_results`)을 `nat_report`로 서버에 알린다. 서버는 다른 반사 주소를 `reported_reflexive` 후보로, `mapping`을 상대에게 줄 `peer_nat`으로 저장한다.
- 시도 규칙(서버가 펀칭 요청을 받을 때 정한다):
  - 두 기기가 모두 `dependent`면 반사 주소 후보로는 시도하지 않는다. 주소 의존·주소와 포트 의존 매핑은 같은 내부 주소·포트에서 같은 목적지로 보내는 패킷에만 매핑을 다시 쓴다([RFC 4787](https://www.rfc-editor.org/rfc/rfc4787) §4.1). 그래서 서버에서 관측한 매핑이 다른 목적지인 상대에게 보낼 때 그대로 쓰인다는 보장이 없고, 양쪽이 모두 그러면 교환한 반사 주소로는 펀칭이 서지 않는다([RFC 5128](https://www.rfc-editor.org/rfc/rfc5128) §3.3. 포트 예측(§3.5)은 하지 않는다). 대신 호스트 후보(사설 IPv4, 전역 IPv6)로만 시도한다(`mode = host_only`). 같은 망 안의 두 기기나 NAT 없는 IPv6 경로는 이것으로 닿는다. 어느 한쪽에도 호스트 후보가 없으면 시도하지 않고 바로 `NatUnsupported`로 끝낸다. `host_only` 시도가 실패해도 `NatUnsupported`다.
  - 그 밖(한쪽만 `dependent`, `independent_port_only`, `unknown`)은 모든 후보로 시도한다(`mode = all`). `dependent` 쪽이 상대의 반사 주소로 보낸 프로브가 상대 NAT의 필터를 통과하면, 상대는 처음 보는 주소(peer reflexive, [RFC 8445](https://www.rfc-editor.org/rfc/rfc8445) §7.3.1.3)에서 온 프로브로 경로를 얻는다.
  - 어느 경우든 중계로 넘어가지 않는다. 시도가 실패하면 연결 실패다.

## 펀칭 절차

A가 연결을 거는 쪽(dialer), B가 받는 쪽이다. 두 기기 모두 같은 rendezvous 서버에 등록돼 있어야 한다.

```
A                         서버                          B
|-- punch_request(B) ---->|                             |
|                         | B 등록·상한·NAT 규칙 확인     |
|                         |-- punch_offer(A) ---------->|
|                         |                             | 페어링 목록에 A가 있나
|                         |<---------- punch_answer(수락)|
|<-- punch_go ------------|------------- punch_go ----->|
|   (B 후보, 토큰, 지연,   |  (A 후보, 토큰, 지연,        |
|    창, handshake 기한)   |   창, handshake 기한)        |
| start_after_ms 대기      |                 start_after_ms 대기
|== STUN 프로브 ==========================================>| (양쪽이 동시에 후보마다)
|<========================================== STUN 프로브 ==|
| 첫 성공 응답 → 그 주소로 QUIC connect                     |
|== QUIC handshake (raw public key, ALPN tasty-net/2) ====>|
|-- punch_result(ok) ---->|<-------- punch_result(ok) ---|
```

1. 요청: A가 서버에 `punch_request{request_id, peer: B의 기기 ID}`를 보낸다. 서버는 B가 등록돼 있는지, 상한(동시 펀칭 수), NAT 규칙을 본다. 통과하지 못하면 `punch_refused{reason}`(`not_found`·`nat_unsupported`·`busy`·`self`)로 끝난다. A가 `punch.request_timeout` 안에 `punch_go`나 `punch_refused`를 받지 못하면 `RequestTimeout`으로 끝난다.
2. 받는 쪽 확인: 서버가 B에게 `punch_offer{punch_id, from: A}`를 보낸다. B는 A가 자기 페어링 허용 목록에 있는지 보고 `punch_answer{punch_id, accept}`로 답한다. B가 거절하면 A는 `declined`를 받는다. 수락 전에는 서버가 양쪽 주소를 서로에게 주지 않는다. 그래서 페어링하지 않은 기기는 B의 주소를 얻지 못한다. B가 서버 설정 `offer_timeout`(기본 2 s) 안에 답하지 않으면 서버는 `not_found`로 끝낸다. B의 등록 때 `welcome.limits` 검사가 어긋나 있었으면 B는 페어링과 관계없이 `punch_answer{accept: false, reason: limits_invalid}`로 답하고 A는 `PeerLimitsInvalid`를 받는다([rendezvous](rendezvous.md#메시지-표) "한계값 검사"). 수락한 B는 `punch.go_timeout` 안에 `punch_go`나 `punch_cancel`을 기다리고, 오지 않으면 그 시도를 끝낸다.
3. 통지와 시각 맞춤: 수락되면 서버가 두 기기에게 동시에 `punch_go`를 보낸다. 실린 것은 [rendezvous](rendezvous.md#메시지-표)의 표에 있다: 상대 기기 ID, 후보와 종류, 상대 `NatMapping`, 이쪽이 보낼 토큰과 상대가 보낼 토큰(서버가 만든 16바이트 난수 두 개), 역할(A는 `controlling`, B는 `controlled`), `mode`, `start_after_ms`, `window_ms`, `handshake_ms`. 두 기기의 시계는 맞추지 않는다. 서버는 자기 쪽 QUIC 연결의 RTT 추정(quinn `Connection::rtt`)으로 `start_after_ms = max(rtt_A, rtt_B)/2 + start_delay_margin − rtt_자기/2`(서버 설정, 상한 `start_delay_max`)를 계산해, 두 기기가 서버 기준으로 거의 같은 때 보내기 시작하게 한다. 두 기기 모두 받은 `punch_go`의 값을 `welcome.limits`와 자기 상한으로 다시 검사하고, 어긋나면 프로브를 보내지 않고 `punch_result{ok: false, reason: limits_invalid}`를 보낸다. `punch_go`의 상대가 이 시도의 상대(A는 요청한 기기, B는 `punch_offer.from`)와 다르거나 역할이 A의 `controlling`·B의 `controlled`와 다르면 서버의 규칙 위반으로 보고 같은 방법으로 끝낸다(A의 오류 키 `punch_go.peer`·`punch_go.role`).
4. 후보 주소: 서버가 본 반사 주소(`server_reflexive`), 상대가 판별에서 보고한 다른 반사 주소(`reported_reflexive`), 상대가 알린 호스트 주소(`host`: 사설 IPv4, 전역 IPv6, 소켓 포트)다. `host_only`면 호스트 주소만 온다. 같은 공유기 안의 두 기기는 사설 주소로 닿는다. 허용 주소·포트, 종류별 우선순위, 상한(기본 1 + 3 + 4 = 8 ≤ `punch.max_candidates` 8)은 서버와 기기가 같은 규칙을 쓴다([rendezvous](rendezvous.md#후보-주소-규칙)). 기기는 받은 후보의 주소도 그 규칙으로 다시 거른다. 후보 수가 기기 상한을 넘은 `punch_go`는 서버의 규칙 위반이므로 잘라 쓰지 않고 `limits_invalid`로 끝낸다(흐름 3).
5. 동시 송신: 시작 시각부터 양쪽이 상대 후보마다 STUN Binding Request를 보낸다. `punch.probe_interval`(기본 50 ms)마다 후보 하나씩 돌아가며 보낸다. 후보가 상한인 8개면 한 후보에 400 ms마다 한 번이다. 요청에는 이쪽 토큰을 속성 `TASTY-PUNCH`로 싣는다([rendezvous](rendezvous.md#펀칭-프로브)). 프로브를 받은 쪽은 토큰이 상대 토큰(`token_peer`)과 같으면 XOR-MAPPED-ADDRESS와 자기 토큰을 실은 Binding Success Response를 그 프로브가 온 주소로 돌려준다.
6. peer reflexive: `mode = all`일 때 후보에 없던 주소에서 맞는 토큰의 프로브가 오면 그 주소를 `peer_reflexive` 후보로 더하고 그 주소에도 프로브를 보낸다. `mode = host_only`에서는 더하지 않는다. 호스트 후보가 아닌 주소에서 온 프로브에는 응답하지 않고 버린 수를 센다(양쪽이 `dependent`면 서버에서 관측한 매핑이 상대 쪽 목적지에 재사용된다는 보장이 없어, 반사 계열 주소로는 양방향 경로가 선다고 기대할 수 없다. RFC 4787 §4.1, 위 "시도 규칙"). 더한 뒤에도 후보 수는 `punch.max_candidates`를 넘지 않는다. 다 찼으면 그 주소는 응답만 하고 후보에 넣지 않으며 버린 수를 센다.
7. 성공 판정: 이쪽이 보낸 요청에 대한 응답이 (a) 그 요청의 transaction ID를 담고, (b) 그 요청을 보낸 후보 주소에서 왔고, (c) 상대 토큰을 담았을 때만 그 주소 쌍이 양방향으로 통한다고 본다([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §6.3, [RFC 8445](https://www.rfc-editor.org/rfc/rfc8445) §7.2.5.3과 같은 뜻). 셋 중 하나라도 어긋난 STUN 패킷은 버린다.
8. QUIC으로 넘어가는 때: A(`controlling`)는 처음 성공한 주소로 바로 quinn `connect`를 건다. 다른 후보의 프로브는 멈추고, 그 주소로 온 프로브에는 계속 응답한다. QUIC 연결은 언제나 A가 연다([전송 추상화](transport.md#p2p의-닫기-범위)의 "스트림은 연결을 연 쪽만 연다"와 맞는다). B(`controlled`)는 성공을 먼저 보거나 A의 응답을 못 받았더라도 QUIC을 걸지 않는다. B의 성공 여부는 B가 QUIC을 받을지에 쓰지 않는다. A의 응답이 잃어 A가 성공을 못 보면 A는 다른 후보나 같은 후보로 프로브를 계속 보내고, 창이 끝나면 실패다.
9. B가 QUIC을 받는 조건(펀칭 세션에 묶기): 두 단계로 고른다. TLS 전에는 Initial의 원격 주소만 안다. 그 주소가 활성 펀칭 세션 하나 이상에서 맞는 토큰의 프로브를 받은 주소(`host_only`면 호스트 후보 주소만)이면 Initial을 받는다. 맞는 세션이 하나도 없으면 TLS 전에 거절한다(quinn `Incoming::refuse`. 상대는 QUIC 전송 오류 `CONNECTION_REFUSED`를 받는다, [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §20.1). 서버의 한 쌍 한 세션 규칙은 기기 ID 쌍에 대한 것이라, 다른 상대들과의 세션 여럿이 같은 원격 주소에 맞을 수 있다. handshake가 끝나면 그 주소에 맞는 세션 가운데 상대(`punch_go.peer`)가 TLS가 확인한 기기 ID와 같은 세션을 고른다. 그런 세션이 없으면 종료 코드 `session_mismatch`로 닫는다([handshake v2](handshake.md#quic-종료-코드)). 고른 세션은 끝내고 연결을 그 세션의 서버·후보 정보와 묶어 [연결 정보](#연결-정보)에 남긴다.
10. 기한: 두 기기 모두 프로브 송신은 시작 시각부터 `window_ms`까지다. A는 창 안에 성공을 보고 QUIC을 걸면 그때부터 `handshake_ms` 안에 handshake가 끝나야 한다. B는 새 QUIC Initial을 시작 시각 + `window_ms` + `handshake_ms`까지 받는다(A가 창 끝 직전에 걸어도 받을 수 있게). B는 이미 받기 시작한 handshake에는 받은 때부터 `handshake_ms`를 준다. B는 연결이 서거나 받기 기한이 끝날 때까지 프로브 응답을 계속한다(B 쪽 NAT 매핑을 열어 두기 위해서다).
11. 끝: 각 기기는 QUIC handshake가 끝나거나 자기 기한이 지나면 `punch_result{punch_id, ok, reason}`를 서버에 보낸다. 서버는 두 기기의 결과를 모두 받거나 세션 TTL이 지나면 그 펀칭 상태를 지운다. TTL은 B의 기한 전체와 한쪽 통지 지연을 덮도록 서버가 계산한다([rendezvous](rendezvous.md#서버-상태)).

- 창과 handshake 기한의 정본은 서버다(`punch_go.window_ms`·`handshake_ms`). 기기 설정 `punch.window_max`·`quic.handshake_timeout_max`는 서버 값의 상한 검사에만 쓴다(넘으면 `ServerLimitsInvalid`, [rendezvous](rendezvous.md#메시지-표)의 `welcome.limits`).
- 진행 중 `punch_cancel`을 받으면: 사유가 `peer_limits_invalid`면 `Scheduled`·`Punching`·`Handshaking` 어느 단계든 재시도 없이 끝낸다(A는 `PeerLimitsInvalid`). 상대가 프로브를 보내지 않으므로 그 시도는 설 수 없다. 그 밖의 사유(`peer_gone`·`expired`·`superseded`)는 프로브 단계면 그 시도를 끝내고(A는 `PunchCancelled{reason}`, 재시도 가능), 이미 QUIC handshake 중이거나 연결이 섰으면 무시한다. 서버가 없어도 기기 사이 경로는 성립하기 때문이다. B가 받는 `superseded`는 같은 A가 재시도로 새 세션을 연 것이므로 B는 옛 시도를 `Closed`로 끝내고 새 `punch_offer`를 따로 받는다.
- 조정 연결을 잃으면: 아직 `punch_go`를 받지 못한 단계(A의 `Requesting`, B의 `Offered`)는 `RendezvousLost`로 끝난다. `punch_go`를 받은 뒤의 단계는 서버 없이 계속하고, `punch_result`는 보내지 않는다.

## 흐름

어떤 패킷이 언제 어느 주소·포트로 나가는지의 설명이다. 사용자 원칙(2026-11-27): "네트워크 장치간 어떻게 연결되어있는지 명확해야한단거야". 연결을 맺고 유지하는 자동 동작(재연결, keepalive, 펀칭 재시도)은 라이브러리가 한다. 대신 그 동작이 지금 어느 단계인지와 결과를 [상태 기계](#상태-기계)와 [연결 정보](#연결-정보)로 드러낸다.

주소 표기: 기기 소켓 `L`(P2P 소켓 하나), 서버 주 주소 `S`(기기가 접속한 주소), 보조 STUN 주소 `S2..S4`(`welcome.stun`), 상대 후보 `C1..Cn`. 모두 UDP다.

| # | 단계 | 송신 → 수신 | 주소 | 내용 | 주기·횟수 | 타임아웃(설정) |
|---|---|---|---|---|---|---|
| 1 | 서버 접속 | 기기 → 서버 | `L` → `S` | QUIC Initial(ALPN `tasty-rendezvous/1`, 서로 raw public key) | 접속 시도마다 한 번(QUIC 재전송은 quinn, 손실 복구 값은 [설정값](#설정값)) | `rendezvous.connect_timeout` |
| 2 | 등록 | 기기 ↔ 서버 | 1의 연결, 조정 스트림 | `hello` → `welcome` | 접속마다 한 번 | `rendezvous.hello_timeout` |
| 3 | NAT 판별 | 기기 → 서버 | `L` → `S`, `S2..S4` | STUN Binding Request / Success Response | 대상마다 `nat.stun_rto` 간격 `nat.stun_attempts`번, 대상끼리 동시 | `nat.stun_rto × nat.stun_attempts` |
| 4 | 판별 보고 | 기기 → 서버 | 조정 스트림 | `nat_report` | 3이 끝날 때마다 | 없음 |
| 5 | 서버 연결 유지 | 양쪽 | `L` ↔ `S` | QUIC PING | `rendezvous.keepalive_interval` | `rendezvous.idle_timeout` |
| 5a | 반사 주소 바뀜 | 서버 → 기기 | 조정 스트림 | `reflexive_changed` → 기기는 3·4를 다시 | 서버 관측 때 | 없음 |
| 6 | 펀칭 요청 | A → 서버 → B → 서버 | 조정 스트림 | `punch_request` → `punch_offer` → `punch_answer` | 요청마다 한 번 | A: `punch.request_timeout`, 서버: `offer_timeout` |
| 7 | 통지 | 서버 → A, B | 조정 스트림 | `punch_go`(후보, 토큰, 지연, 창, handshake 기한) | 수락마다 한 번 | B: `punch.go_timeout` |
| 8 | 프로브 | A ↔ B | `L_A` ↔ `C1..Cn` | STUN Binding Request(`TASTY-PUNCH`) | 후보를 돌며 `punch.probe_interval`마다 하나 | `punch_go.window_ms` |
| 9 | 프로브 응답 | 받은 쪽 → 보낸 쪽 | 프로브가 온 주소로 | Binding Success Response(XOR-MAPPED-ADDRESS, `TASTY-PUNCH`) | 받은 프로브마다 | 없음 |
| 10 | 기기 사이 QUIC | A → B | `L_A` → 성공한 주소 | QUIC Initial(ALPN `tasty-net/2`) | 시도마다 한 번 | `punch_go.handshake_ms` |
| 11 | 결과 | A, B → 서버 | 조정 스트림 | `punch_result` | 시도마다 한 번 | 없음 |
| 11a | 취소 | 서버 → A, B | 조정 스트림 | `punch_cancel` | 상대가 사라지거나 세션 TTL이 지날 때 | 없음 |
| 12 | 경로 유지 | 양쪽 | `L_A` ↔ `L_B`의 바깥 주소 | QUIC PING | `quic.keepalive_interval` | `quic.idle_timeout` |
| 13 | 닫기 | 닫는 쪽 → 상대 | 같은 경로 | QUIC CONNECTION_CLOSE(종료 코드) | 한 번 | 없음 |
| 14 | 서버 재연결 | 기기 → 서버 | 1과 같음 | 1~4 반복 | `rendezvous.reconnect.*` backoff | 1과 같음 |
| 15 | 펀칭 재시도 | A → 서버 | 6과 같음 | 3(반사 주소 재확인), 6~11 반복 | `punch.attempts`, `punch.retry_delay` | 6~10과 같음 |

- 서버는 8~10과 12~13의 패킷을 보지도 나르지도 않는다. 서버가 기기에게 보내는 것은 2·3의 응답, 5의 PING, 5a·6·7·11a의 통지와 `punch_refused`뿐이다. 11은 기기가 서버에게 보내는 보고다.
- 13 뒤에 남은 연결이 없어도 서버 연결(5)은 P2P가 켜져 있는 동안 유지된다. P2P를 끄면 서버 연결을 코드 0으로 닫고 소켓을 닫는다.

## 정책

사용자 결정(2026-11-27)이 기본 동작이다. 앞선 iroh 설계 단계의 결정(Q3·Q4와 그 대체 결정)도 이 구조에서 아래처럼 성립한다.

- P2P는 기기 설정이고 기본은 꺼짐이다. `P2pConfig.enabled` 기본값은 false다. 꺼져 있으면 `P2pEndpoint::bind`는 소켓을 열지 않는 꺼진 endpoint를 돌려준다. 서버에 연결하지 않고 연결도 받지 않는다. 꺼진 endpoint의 연결 시도는 네트워크에 아무것도 보내지 않고 `P2pConnectError::Disabled`로, 받기는 `P2pError::Disabled`로 끝난다.
- 켰을 때만 서버 연결을 유지한다. 연결을 거는 쪽도 켜져 있어야 한다. 거는 쪽의 펀칭 요청도 자기 서버 연결로 보내기 때문이다.
- 같은 서버에 붙은 기기끼리만 연결된다. 같은 서버인지 비교하는 로직은 두지 않는다. 기기는 설정한 서버 하나에만 등록하고 그 서버에만 요청한다. 다른 서버에 등록한 상대는 이 서버에 없으므로 `not_found`로 끝난다(`P2pConnectError::PeerNotFound`). 서버끼리 연동하는 기능도 없다.
- 전역 discovery는 없다. 상대를 찾는 길은 설정한 rendezvous 서버 하나뿐이다. DNS 기반 주소 게시나 공용 디렉터리를 쓰지 않는다.
- 펀칭 실패는 연결 실패다. 양쪽 모두 `dependent`인 NAT 조합을 포함한다(호스트 후보로만 시도하고, 닿지 않으면 `NatUnsupported`). 중계로 넘어가지 않는다. 넘어갈 중계 경로가 이 구조에 없다.
- 앞서 정한 Q3(직접 경로 확인 전 attach 데이터 보류, 2026-11-27)는 이 구조에서 저절로 충족된다. QUIC 연결은 펀칭으로 확인한 직접 주소 위에서만 서고, 서버는 기기 사이 패킷을 나르지 않는다. 그래서 handshake v2도 attach 데이터도 서버를 지나지 않는다. iroh 설계의 `hold_until_direct` 설정, `DirectRequired`·`RelayAllowed` 정책, `direct_wait`, `unknown_relay_policy`는 지킬 대상이 없어져 없앤다.
- 서버 주소는 소비자(tasty) 설정이다. 라이브러리에 기본 서버 상수를 두지 않는다. tasty의 기본은 hub가 운영하는 rendezvous 서버(hub rendezvous)이고, 사용자가 자기 서버를 지정하면 그 서버 하나만 쓴다([rendezvous](rendezvous.md#클라이언트-설정)).

## 연결 유지

| 대상 | keepalive | idle timeout |
|---|---|---|
| 서버 QUIC 연결 | `rendezvous.keepalive_interval` 15 s(quinn `TransportConfig::keep_alive_interval`, quinn 기본은 꺼짐) | `rendezvous.idle_timeout` 45 s(`max_idle_timeout`, quinn 기본 30 s) |
| 펀칭된 경로의 QUIC 연결 | `quic.keepalive_interval` 15 s | `quic.idle_timeout` 45 s |

- 주기 15 s의 근거: NAT의 UDP 매핑 만료 시간에 대해 [RFC 4787](https://www.rfc-editor.org/rfc/rfc4787) §4.3 REQ-5는 2분 미만으로 만료하지 말라고(권장 5분 이상) 하고, NAT마다 쓰는 값의 차이가 크다고 적는다. [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §10.1.2는 "experience shows that sending packets every 30 seconds is necessary to prevent the majority of middleboxes from losing state for UDP flows"라고 한다. ICE는 keepalive 기본을 15 s로 둔다([RFC 8445](https://www.rfc-editor.org/rfc/rfc8445) §11, 15 s 미만 금지). 30 s보다 짧은 ICE 값을 그대로 빌린다.
- idle timeout 45 s는 keepalive의 세 배다. keepalive PING은 확인 응답을 요구하는 패킷이라 잃으면 손실 복구(PTO)로 다시 보낸다. 그래서 보통은 PING 두 번을 잃어도 연결이 남지만, 실제 여유는 손실 양상과 PTO에 달렸고 보장은 아니다.
- `quic.idle_timeout`은 설정값이고, 연결이 실제로 끝나는 시각은 [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §10.1의 규칙으로 정해진다. 협상값은 양쪽 값 중 작은 쪽이고, 한쪽이 0(끔)이면 다른 쪽 값이다. 타이머 길이는 협상값과 현재 PTO의 세 배 중 큰 쪽이다. 타이머는 상대 패킷을 받을 때와, 마지막 수신 뒤 처음 보내는 ack-eliciting 패킷 때 다시 시작한다(quinn-proto `reset_idle_timeout`). 그래서 두 끝의 종료 시각은 서로 다를 수 있고, 설정값 45 s가 양쪽의 종료 시각인 것은 아니다. 시험은 종료 원인이 `idle_timeout`인지와 상한 시간 안에 끝나는지만 본다. 상한은 마지막 타이머 재시작 시각(마지막 유효 수신, 또는 그 뒤 첫 ack-eliciting 송신 중 늦은 쪽) + 유효 idle timeout(그때의 max(협상값, 3 × PTO)) + 여유다.
- EIM NAT에서는 서버 연결의 keepalive가 같은 매핑을 살리지만, 상대 주소에 대한 필터 상태는 상대와 주고받는 패킷만 살린다. 그래서 펀칭된 경로에도 따로 keepalive를 둔다.
- 링크의 heartbeat와의 관계: attach 링크는 프레임 heartbeat(`HEARTBEAT_INTERVAL` 5 s, `read_timeout` 기본 30 s, [전송 추상화](transport.md#heartbeat와-읽기-시간-제한))를 그대로 쓴다. 링크가 살아 있는 동안은 heartbeat가 먼저 끊김을 본다(기본값에서 30 s < 45 s. 설정을 바꿔 이 순서가 뒤집혀도 동작은 맞고, 끊김을 먼저 보는 쪽만 바뀐다). QUIC keepalive는 링크가 없거나 조용한 연결(요청 채널만 있는 연결, 다음 채널을 기다리는 연결)의 매핑을 살린다. QUIC idle timeout으로 끝난 연결의 읽기는 `TimedOut`으로 분류된다([전송 추상화](transport.md#읽기-실패-분류)).

## 재시도와 포기

- 서버 재연결: 서버 연결이 끊기면 지수 backoff로 다시 붙는다. n번째 대기는 `min(max_delay, initial × multiplier^(n−1))`에 `1 + u`(u는 `[−jitter, +jitter]`의 균등 난수)를 곱한 뒤 `[0, max_delay]`로 자른 값이다. 그래서 대기는 `max_delay`(기본 60 s)를 넘지 않는다. 대기 뒤의 접속·등록 시간(`rendezvous.connect_timeout`, `hello_timeout`)은 이 대기와 따로 걸린다. 연결이 `reset_after`(60 s) 넘게 유지되면 n을 1로 되돌린다. 횟수 상한 `max_attempts` 기본은 0(켜져 있는 동안 계속)이다. 상한을 주면 그 횟수 뒤 `Stopped{cause: reconnect_exhausted}`로 멈춘다. P2P를 끄면 재연결을 멈춘다. `superseded`, `access_denied`, `unsupported_proto`, ALPN 불일치, 서버 키 불일치면 다시 붙지 않고 `Stopped`로 알린다(소비자가 사용자에게 보인다, [rendezvous](rendezvous.md#메시지-표)).
- 펀칭 재시도: 연결 시도 하나 안에서 펀칭을 최대 `punch.attempts`(기본 2)번 한다. 한 시도가 창이나 handshake 기한으로 실패하면 반사 주소 재확인(흐름 3)을 바로 시작하고, `punch.retry_delay`(기본 1 s)와 재확인 중 늦게 끝나는 쪽을 기다린 뒤 새 `punch_request`로 다시 한다. 이때 B는 옛 세션의 받기 기한 안에 있을 수 있지만, 같은 A의 새 요청이므로 서버가 옛 세션을 끝내고(B에게 `punch_cancel{superseded}`) 새 세션을 만든다([rendezvous](rendezvous.md#메시지-표) "한 쌍에 세션 하나"). 그래서 재시도는 `busy`로 막히지 않는다. 모두 실패하면 마지막 시도의 `mode`로 오류를 정한다: `all`이면 `P2pConnectError::PunchFailed`, `host_only`면 `NatUnsupported`(두 기기 모두 `dependent`라 호스트 경로 밖에는 길이 없다는 뜻).
- 연결 시도 하나의 전체 상한: 시도마다 `punch.request_timeout` + 서버 `start_delay_max` + `window_ms` + `handshake_ms`, 시도 사이마다 `max(punch.retry_delay, nat.stun_rto × nat.stun_attempts)`. 기본값이면 2 × (3 + 1 + 5 + 5) s + 1.5 s = 29.5 s다. 재시도가 옛 세션을 바꾸므로 B 쪽 옛 세션의 남은 기한은 이 합에 들어가지 않는다. 서버 값은 `welcome.limits`로 알고 계산해 연결 정보에 보인다. `punch_go`의 창과 handshake 기한이 `welcome.limits`보다 길면 그 시도를 `limits_invalid`로 끝내고(흐름 3), 이 상한이 지나면 시도는 그때의 단계에서 끝난다.
- 전체 상한의 범위: 상한은 시각(지금 + 상한)이 되어야 하므로, 다른 시간 설정처럼 2^32 − 1 s 이하이고 현재 시각에 더해 시계 범위 안이어야 한다. 키 하나하나가 범위 안이어도 합이 넘을 수 있다(예: `nat.stun_rto` 2 s, `nat.stun_attempts`·`punch.attempts` 2^32 − 1). `bind`는 서버가 알릴 수 있는 가장 긴 창과 handshake(`punch.window_max`·`quic.handshake_timeout_max`)와 시작 지연 0으로 상한을 계산해, 넘으면 설정 관계 오류(`SettingsError::Relation`, 관계에 드는 키 `punch.request_timeout`·`punch.window_max`·`quic.handshake_timeout_max`·`punch.attempts`·`punch.retry_delay`·`nat.stun_rto`·`nat.stun_attempts`와 값)로 거절한다. 서버 값이 끼는 상한은 등록 때 `welcome.limits`(`start_delay_max_ms`·`punch_window_ms`·`quic_handshake_ms`)로 다시 계산하고, 넘으면 키 `attempt_bound`의 `server_limits_mismatch`이고 그 서버로의 연결 시도는 `ServerLimitsInvalid{key: "attempt_bound"}`로 끝난다. 시도의 기한은 검사한 덧셈으로 만든다(넘으면 같은 오류로 끝나고 패닉하지 않는다).
- 바로 끝내는 경우(재시도 없음): `nat_unsupported`, `declined`, `self`, P2P 꺼짐, 상대 키 불일치, ALPN 불일치, 서버 한계값 불일치(이쪽이든 상대든).
- 서버에 아직 붙지 못한 상태의 연결 시도는 `RendezvousUnavailable`로 바로 끝난다. 다음 시도를 언제 할지는 연결 목록의 재연결 도우미나 소비자가 정한다([전송 추상화](transport.md#연결-목록)).

| `P2pConnectError` | 뜻 | `is_retryable` |
|---|---|---|
| `Disabled` | 이 기기의 P2P가 꺼져 있다. 네트워크에 아무것도 보내지 않았다 | 아니오 |
| `RendezvousUnavailable` | 서버에 붙어 있지 않다(endpoint가 `Connecting`·`Backoff`·`Stopped`. `Stopped`면 원인을 함께 싣는다) | `Stopped`면 아니오, 그 밖은 예 |
| `RendezvousLost` | `punch_go`를 받기 전에 조정 연결을 잃었다 | 예 |
| `RequestTimeout` | `punch.request_timeout` 안에 서버 답이 없다 | 예 |
| `PeerNotFound` | 서버에 상대가 없다(꺼져 있음, 다른 서버, 응답 없음) | 예 |
| `Declined` | 상대가 이 기기를 페어링 목록에 두지 않았다 | 아니오 |
| `NatUnsupported` | 두 기기 모두 `dependent`이고 호스트 후보로도 닿지 않았다(또는 호스트 후보가 없다) | 아니오 |
| `PunchCancelled{reason}` | 서버가 진행 중 펀칭을 취소했다(`peer_gone`·`expired`·`superseded`. `peer_limits_invalid`는 아래 `PeerLimitsInvalid`) | 예 |
| `PeerLimitsInvalid` | 상대 기기가 서버 한계값을 자기 설정과 맞지 않는다고 거절했다(`punch_refused{peer_limits_invalid}`·`punch_cancel{peer_limits_invalid}`) | 아니오 |
| `PunchFailed` | 펀칭 시도가 모두 시간 안에 서지 않았다 | 아니오 |
| `Busy` | 서버 상한(동시 펀칭 수 등) | 예 |
| `ServerLimitsInvalid{key}` | 서버가 알린 값이 이 기기 설정과 맞지 않는다(`welcome.limits` 또는 `punch_go`) | 아니오 |
| `PeerIdentityMismatch` | QUIC 상대 키가 요청한 기기 ID와 다르다 | 아니오 |
| `SelfAttach` | 자기 기기 ID로 연결하려 했다 | 아니오 |
| `AlpnMismatch` | 상대와 ALPN(`proto`)이 다르다([버전·호환](versioning.md)) | 아니오 |
| `Cancelled` | 호출자가 시도를 취소했다 | 아니오 |

`PunchFailed`의 `is_retryable`이 false인 것은 같은 망 조건에서 곧바로 다시 걸어도 같은 결과일 가능성이 높아서다. 소비자가 자동으로 되풀이하지 않고 사용자에게 알린다. 망이 바뀐 뒤 사용자가 다시 거는 것은 막지 않는다.

## 상태 기계

endpoint(서버 등록)와 연결 시도·연결이 각각 상태 기계다. 전이마다 이벤트 `{from, to, cause, at}`를 하나 낸다. `cause`는 아래 표의 원인 값이고, 오류가 원인이면 그 오류 값(`P2pConnectError`, QUIC 종료 코드, io 분류)을 함께 싣는다. 호출자는 `status()`로 지금 상태를, `events()`로 전이 이력을 본다. 이벤트 큐 상한은 `events.queue_limit`(기본 256)이고, 넘치면 가장 오래된 것을 버리고 `Lagged{dropped}` 이벤트 하나를 남긴다. 블로킹 대기 `next_event`는 endpoint의 끝도 받는다: 끝난 뒤에도 큐에 남은 이벤트(`Off` 포함)를 먼저 돌려주고, 큐가 비었으면 기다리지 않고 `P2pError::Closed`를 돌려준다. 기다리던 호출자는 끝에서 모두 깨어난다.

### endpoint

| 상태 | 뜻 | 상태가 싣는 값 |
|---|---|---|
| `Off` | P2P 꺼짐. 소켓 없음 | 없음 |
| `Connecting` | 서버 접속·등록 중(흐름 1~2) | 서버 주소, 시도 번호 |
| `Registered` | 등록됨. 연결을 걸고 받을 수 있다 | 서버 주소, 로컬 소켓 주소, 반사 주소, `NatMapping`, 서버 한계값(`welcome.limits`), 등록 시각 |
| `Backoff` | 다음 재연결 대기 | 시도 번호, 다음 시도 시각, 직전 실패 원인 |
| `Stopped` | 재연결하지 않고 멈춤 | 원인 |

| 전이 | 원인(`cause`) |
|---|---|
| `Off` → `Connecting` | `enabled` |
| `Connecting` → `Registered` | `registered` |
| `Connecting` → `Backoff` | `connect_timeout`, `hello_timeout`, `io`(분류 포함), `server_closed`(코드 0·4·5·6) |
| `Registered` → `Registered` | `nat_changed`(반사 주소나 `NatMapping`이 바뀜. 상태는 같고 값만 바뀐다), `server_limits_mismatch`(서버 한계값이 이 기기 설정과 어긋남. 펀칭은 `ServerLimitsInvalid`로 끝난다) |
| `Registered` → `Backoff` | `idle_timeout`, `server_closed`, `io` |
| `Backoff` → `Connecting` | `backoff_elapsed` |
| `Connecting`·`Registered`·`Backoff` → `Stopped` | `superseded`, `access_denied`, `unsupported_proto`(서버 종료 코드 1·2·3), `alpn_mismatch`, `server_key_mismatch`(TLS 단계, [rendezvous](rendezvous.md#메시지-표)), `reconnect_exhausted` |
| 어느 상태든 → `Off` | `disabled` |

### 연결 시도와 연결

| 상태 | 쪽 | 뜻 |
|---|---|---|
| `Requesting` | A | `punch_request`를 보내고 `punch_go`·`punch_refused`를 기다림 |
| `Offered` | B | `punch_offer`를 받아 페어링 목록을 보고 답함. 수락했으면 `punch_go`를 기다림 |
| `Scheduled` | 양쪽 | `punch_go`를 받고 `start_after_ms`를 기다림 |
| `Punching` | 양쪽 | 프로브 송수신(흐름 8~9). 시도 번호, 후보 목록, 창 끝 시각을 싣는다 |
| `Handshaking` | 양쪽 | QUIC handshake(흐름 10). A는 성공한 주소로 건 연결, B는 세션에 묶인 주소에서 받은 연결. 주소 쌍과 기한을 싣는다 |
| `RetryWait` | A | 다음 펀칭 시도 대기. 시도 번호, 다음 시도 시각, 직전 실패 원인 |
| `Connected` | 양쪽 | QUIC 연결이 섰다. 주소 쌍, 상대 기기 ID, 서버 |
| `Closed` | 양쪽 | 끝. 원인 |

| 전이 | 원인 |
|---|---|
| (시작) → `Requesting` | `connect_called` |
| `Requesting` → `Scheduled` | `punch_go` |
| `Requesting` → `Closed` | `not_found`, `declined`, `peer_limits_invalid`, `nat_unsupported`, `busy`, `self`(서버 거절), `request_timeout`, `rendezvous_lost`, `server_limits_invalid` |
| (시작) → `Offered` | `punch_offer` |
| `Offered` → `Scheduled` | `punch_go`(수락 뒤) |
| `Offered` → `Closed` | `not_paired`·`limits_invalid`(B가 거절), `go_timeout`, `punch_cancelled(reason)`, `rendezvous_lost` |
| `Scheduled` → `Closed` | `server_limits_invalid`(받은 `punch_go`가 상한을 벗어남. A·B 모두 재시도 없음) |
| `Scheduled`·`Punching`·`Handshaking` → `Closed` | `punch_cancelled(peer_limits_invalid)`(재시도 없음. A의 오류는 `PeerLimitsInvalid`) |
| `Scheduled` → `Punching` | `start_elapsed` |
| `Scheduled`·`Punching` → `RetryWait` / `Closed` | `punch_cancelled(reason)`, 사유는 `peer_gone`·`expired`·`superseded`(A는 남은 시도가 있으면 `RetryWait`, B는 `Closed`. B의 `superseded`는 `Handshaking`에서도 `Closed`) |
| `Punching` → `Handshaking` | A: `probe_succeeded`(주소 포함). B: `quic_incoming`(세션에 묶인 주소에서 온 Initial) |
| `Punching` → `RetryWait` / `Closed` | A: `punch_window_elapsed`(남은 시도가 있으면 `RetryWait`, 없으면 `Closed`. 마지막 시도가 `host_only`면 오류는 `NatUnsupported`, 아니면 `PunchFailed`). B: `accept_deadline_elapsed`(`Closed`) |
| `Handshaking` → `Connected` | `quic_established` |
| `Handshaking` → `RetryWait` / `Closed` | `handshake_timeout`(A는 남은 시도가 있으면 `RetryWait`. 오류는 위 `Punching`과 같은 규칙), `peer_identity_mismatch`(바로 `Closed`), `session_mismatch`(B, 바로 `Closed`), `alpn_mismatch`(바로 `Closed`). B는 받은 Initial의 주소가 맞은 세션이 하나뿐일 때만 그 handshake의 실패를 그 세션의 원인으로 보고, 여럿이면 어느 세션의 실패인지 모르므로 받기 기한까지 기다린다 |
| `RetryWait` → `Requesting` | `retry_elapsed` |
| `Connected` → `Connected` | `remote_address_observed_change`(B에서만, 아래) |
| `Connected` → `Closed` | `closed_by_local(code)`, `closed_by_peer(code)`, `idle_timeout`, `endpoint_off` |
| 어느 상태든 → `Closed` | `cancelled`, `disabled` |

- `Connected`에서 끊긴 연결을 다시 맺는 일은 연결 목록을 가진 transport 쪽이 한다([전송 추상화](transport.md#연결-목록)). P2P 크레이트는 새 연결 시도(새 펀칭)로 받는다. 중계로 넘어가지 않는다.
- transport 연결 목록의 네 상태와의 대응: `Requesting`·`Offered`·`Scheduled`·`Punching`·`Handshaking`·`RetryWait`는 `connecting`(재연결로 만든 항목이면 `reconnecting`), `Connected`는 `connected`, `Closed`는 `disconnected`다. 세부 상태는 항목의 `phase`에 그대로 보인다.

### 주소 바뀜 (QUIC 경로 이동)

- QUIC 연결은 언제나 A가 연다. 그래서 A가 QUIC 클라이언트, B가 서버다. QUIC의 연결 이동은 클라이언트 주소에만 허용된다. 클라이언트는 자기가 시작하지 않은 서버 주소 변화의 패킷을 버린다([RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §9, §9.6).
- A 쪽 NAT 재매핑: B가 새 주소에서 오는 A의 패킷을 받고, quinn이 경로 검증과 함께 그 주소로 옮긴다(B의 quinn `ServerConfig::migration`, 설정 `quic.allow_migration`, 기본 켬. 이 설정은 서버 쪽에서 클라이언트의 이동을 받을지만 정한다). B는 `Connection::remote_address()`의 변화를 `remote_address_observed_change{from, to}`로 기록한다.
- B 쪽 NAT 재매핑: A는 새 주소에서 온 B의 패킷을 버린다. 연결은 idle 타이머가 끝나면 `idle_timeout`으로 끊기고(양쪽 모두. 끝나는 시각은 위 "연결 유지"의 규칙으로 끝마다 다르다), transport가 새 펀칭으로 다시 맺는다. 이 경우를 따로 알아내지는 않는다. 끊긴 원인은 `idle_timeout`이다.
- 이 기록은 "관측된 원격 주소 변화"다. 경로 검증이 끝났다는 뜻이 아니다. quinn은 이동을 시작할 때 현재 경로를 바꾸고, 검증이 실패하면 옛 경로로 되돌린다. 되돌림도 같은 사건으로 기록된다.
- quinn에는 이 변화를 알리는 콜백이 없어 `path.poll_interval`(기본 1 s)마다 `remote_address()`를 읽어 비교한다. 한 간격 안에 바뀌었다 돌아온 변화와, 한 간격 안의 두 번째 이후 변화는 놓친다. 이력은 폴링 시점의 값만 담는다.

## 설정값

모든 타이머·재시도 횟수·간격·타임아웃·keepalive 주기·상한은 설정값이다(사용자 결정 2026-11-27, 원문 "타이머, 재시도횟수 이런것은 전부 설정가능 및 보기 가능값이어야하지").

- 바꾸기: `P2pConfig.settings`(`P2pSettings`)로 준다. 주지 않은 항목은 기본값이다. 값의 범위 검사(예: `idle_timeout > keepalive_interval`, 연결 시도 전체 상한의 범위([재시도와 포기](#재시도와-포기)))는 `bind`가 하고 어긋나면 오류로 돌려준다.
- 보기: `P2pEndpoint::settings()`가 항목마다 `{key, value, source}`를 돌려준다. `source`는 `default`(이 표의 기본), `configured`(설정으로 줌), `os_default`(OS가 정한 값을 읽어 보임), `fixed`(아래 "고정값"), `server`(rendezvous 서버가 정해 알린 값. 연결 정보의 `applied`에만 나온다) 중 하나다. 서버가 정한 값이 서버 안에서 기본인지 운영자가 준 값인지는 서버의 `settings()`와 시작 로그에서 본다(기기는 그 구분을 받지 않는다). 연결마다 그 연결에 적용된 값과 진행 값(지금 몇 번째 시도인지, 다음 시도까지 남은 시간)은 [연결 정보](#연결-정보)에 있다.
- 네 가지를 구분한다.
  1. 설정 가능한 동작값: 아래 두 표(이 레포가 정하는 값, 의존성이 노출하는 값). 모두 설정·조회된다.
  2. 고정값: 의존성 setter가 있지만 와이어 형식이나 인증 전제를 깨므로 바꿀 수 없게 한 값. 조회에는 보인다.
  3. 의존성 내부 값: 의존성이 노출하지 않아 설정할 수 없는 값. 이름과 출처만 아래 표에 적는다.
  4. 와이어 형식 상수: 프로토콜 형식의 정의라 동작값이 아니다(아래 표). 형식 모듈에 모은다.
- 기본값의 원천은 코드의 기본값 표 하나다(`settings::DEFAULTS`, 키·값·출처 종류를 담는다). `Default` 구현과 `settings()`가 이 표에서 나오고, 시험이 설계 문서의 표를 읽어 이 원천과 값까지 대조한다([CI와 시험 환경](testing.md#설정값-시험)).
- 코드 안 상수로만 있는 동작값은 두지 않는다. 정적 검사와 기본값 대조 시험이 지킨다([CI와 시험 환경](testing.md#설정값-시험)).
- 적용 범위: `tasty-net-p2p`와 `tasty-net-rendezvous`는 처음부터 이 규칙을 따른다. develop에 이미 있는 transport의 상수(heartbeat 간격 등)는 이 규칙에 맞추는 후속 작업으로 둔다(이번 문서 작업 범위 밖). identity는 규칙에 맞췄다([기기 키·토큰](identity.md#설정값)). ssh도 맞췄다([전송 추상화](transport.md#ssh-크레이트-설정값)). transport에 이번에 새로 정한 연결 목록 값은 처음부터 따른다([전송 추상화](transport.md#연결-목록)).

### 이 레포가 정하는 값

| 키 | 기본 | 근거 |
|---|---|---|
| `rendezvous.connect_timeout` | 5 s | 서버까지 QUIC handshake는 RTT 두세 번이다. 먼 서버(RTT 300 ms)와 손실 몇 번을 덮는다 |
| `rendezvous.hello_timeout` | 5 s | `hello` → `welcome` 한 번 왕복. 위와 같은 여유 |
| `rendezvous.max_message_bytes` | 64 KiB | 서버에서 받는 프레임 상한. `welcome` 전에도 쓴다. 서버 기본 16 KiB의 네 배 여유. 1 KiB–1 MiB만 받는다([rendezvous](rendezvous.md#프레임)) |
| `rendezvous.frame_timeout` | 5 s | 서버 프레임 하나가 끝나기까지의 기한. 서버 `frame_timeout`과 같은 근거 |
| `rendezvous.keepalive_interval` | 15 s | 위 "연결 유지" |
| `rendezvous.idle_timeout` | 45 s | 위 "연결 유지" |
| `rendezvous.reconnect.initial` | 1 s | 일시 끊김은 바로 복구 |
| `rendezvous.reconnect.multiplier` | 2 | 서버 장애 때 접속 폭주를 줄이는 보통의 지수 backoff |
| `rendezvous.reconnect.max_delay` | 60 s | 서버가 돌아온 뒤 1분 안에는 다시 등록. 무작위 폭을 더한 뒤에도 이 값으로 자른다 |
| `rendezvous.reconnect.jitter` | 0.2 | 같은 때 끊긴 기기들이 같은 때 다시 붙지 않게 |
| `rendezvous.reconnect.reset_after` | 60 s | 1분 유지되면 안정된 연결로 본다 |
| `rendezvous.reconnect.max_attempts` | 0(무제한) | 켜 둔 동안은 계속 붙으려 한다. 끄는 것은 사용자가 P2P를 끄는 것이다 |
| `nat.stun_rto` | 500 ms | [RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §6.2.1 RTO 권장 초깃값 500 ms(두 배로 늘리지 않는 고정 간격으로 줄였다, 위 "NAT 판별") |
| `nat.stun_attempts` | 3 | RFC 8489의 Rc 기본 7보다 적다. 응답 없음은 `unknown`으로 두면 되므로 1.5 s 안에 끝낸다 |
| `punch.request_timeout` | 3 s | 서버 `offer_timeout` 2 s + 서버 왕복 여유. 서버 값보다 커야 한다(`welcome.limits`로 검사) |
| `punch.go_timeout` | 3 s | B가 수락한 뒤 `punch_go`를 기다리는 시간. 서버 왕복 한 번에 여유 |
| `punch.probe_interval` | 50 ms | [RFC 8445](https://www.rfc-editor.org/rfc/rfc8445) §14.2 Ta 기본 50 ms |
| `punch.window_max` | 30 s | 서버가 주는 창(`punch_go.window_ms`, 서버 기본 5 s)의 상한 검사. 잘못된 서버 설정이 연결 시도를 몇 분씩 붙잡지 않게 |
| `quic.handshake_timeout_max` | 30 s | 서버가 주는 handshake 기한(`handshake_ms`, 서버 기본 5 s)의 상한 검사 |
| `punch.max_candidates` | 8 | 서버 반사 1 + 보고 반사 3(서버 기본 `max_reported_reflexive`) + 호스트 4(서버 기본 `max_host_candidates`). 서버 값과의 관계는 `welcome.limits`로 검사한다([rendezvous](rendezvous.md#후보-주소-규칙)). 프로브 한 바퀴 400 ms. peer reflexive를 더해도 넘지 않는다. `bind`는 1 이상 64 이하만 받는다 |
| `punch.attempts` | 2 | 일시 손실로 한 번 실패한 경우만 덮는다. 펀칭 실패는 연결 실패라는 결정에 맞춰 짧게 둔다. 이 값으로 곱한 연결 시도 전체 상한은 2^32 − 1 s 이하여야 한다([재시도와 포기](#재시도와-포기)) |
| `punch.retry_delay` | 1 s | 반사 주소를 다시 확인할 시간 |
| `quic.keepalive_interval` | 15 s | 위 "연결 유지" |
| `quic.idle_timeout` | 45 s | 위 "연결 유지" |
| `quic.max_bidi_streams` | 64 | attach 링크 몇 개와 bulk·요청 채널을 넉넉히 덮는다(quinn 기본 100) |
| `quic.max_uni_streams` | 0 | 단방향 스트림을 쓰지 않는다(quinn 기본 100) |
| `quic.allow_migration` | 켬 | B가 A의 NAT 재매핑에도 연결을 유지한다(위 "주소 바뀜") |
| `path.poll_interval` | 1 s | 주소 바뀜을 1초 안에 연결 정보에 반영 |
| `history.limit` | 32 | 연결마다 전이 이력 상한. 재연결 몇 번과 주소 바뀜을 담고, 넘치면 오래된 것부터 버리고 버린 수를 센다 |
| `events.queue_limit` | 256 | 이벤트를 읽지 않는 호출자 때문에 메모리가 늘지 않게 |
| `runtime.worker_threads` | 2 | 소유 런타임(아래 "endpoint와 런타임")의 작업 스레드 수. 소켓 하나와 연결 몇 개의 입출력·타이머만 돈다. `bind`는 1 이상 1024 이하만 받는다 |
| `runtime.shutdown_timeout` | 5 s | endpoint가 끝날 때 소유 런타임이 남은 작업을 기다리는 상한. 넘으면 남은 작업을 버리고 스레드를 끝낸다 |
| `socket.recv_buffer`, `socket.send_buffer` | OS 값 | 따로 주지 않으면 OS 값을 쓰고 조회에 `os_default`로 보인다 |

- 펀칭 창과 handshake 기한, 시작 지연, `offer_timeout`은 서버 설정이다([rendezvous](rendezvous.md#서버-설정값)). 기기는 `welcome.limits`와 `punch_go`로 받은 값을 쓰고 상한만 검사한다.

### 의존성이 노출하는 값

- 규칙: 고정한 판(quinn 0.11.12, 그 의존 quinn-proto 0.11.19(`Cargo.lock`으로 고정), rustls 0.23.46)에서 공개 setter나 공개 필드가 있는 값 가운데 시간·횟수·크기·상한을 받는 것은 모두 설정·조회 대상이다. 아래 "고정값"으로 뺀 것만 예외다. 표에 하나씩 적지만, 표가 빠뜨린 setter를 시험이 찾는다(`dependency_setters_are_covered`, [CI와 시험 환경](testing.md#설정값-시험)). 그 시험은 고정판의 공개 API 목록(rustdoc JSON에서 뽑아 시험 자료로 커밋한 목록)과 이 표의 "의존성 설정" 열을 대조한다. 판을 올리면 목록을 다시 뽑고 이 표를 같이 고친다.
- 값이 켜고 끄는 기능에 딸린 경우(예: ACK frequency, datagram) 기능이 꺼져 있으면 딸린 값은 "적용 안 됨"으로 조회에 보인다. 값은 그대로 설정할 수 있고 기능을 켤 때 쓰인다.
- 기본값은 quinn-proto 0.11.19와 rustls 0.23.46의 `Default`·생성 함수 소스에서 읽은 값이다. 이 설계가 바꾼 값은 근거에 적었다.

| 키 | 의존성 설정 | 기본 | 근거 |
|---|---|---|---|
| `quic.initial_rtt` | `TransportConfig::initial_rtt` | 333 ms | [RFC 9002](https://www.rfc-editor.org/rfc/rfc9002) §6.2.2 kInitialRtt. 서버 접속의 Initial 재전송 간격이 이 값에서 시작한다 |
| `quic.packet_threshold` | `TransportConfig::packet_threshold` | 3 | RFC 9002 §6.1.1 kPacketThreshold |
| `quic.time_threshold` | `TransportConfig::time_threshold` | 9/8 | RFC 9002 §6.1.2 kTimeThreshold |
| `quic.persistent_congestion_threshold` | `TransportConfig::persistent_congestion_threshold` | 3 | RFC 9002 §7.6.1 kPersistentCongestionThreshold |
| `quic.stream_receive_window` | `TransportConfig::stream_receive_window` | 1 250 000 B | quinn 기본(12.5 MB/s × 100 ms). bulk 실측(D4 `bulk_transfer_matches_bytes`)으로 다시 본다 |
| `quic.receive_window` | `TransportConfig::receive_window` | 2^62 − 1 B(VarInt 최대, 사실상 연결 단위 상한 없음) | quinn 기본. 스트림 수 상한 64와 스트림 창이 메모리를 묶는다 |
| `quic.send_window` | `TransportConfig::send_window` | 10 000 000 B | quinn 기본(스트림 창의 8배) |
| `quic.initial_mtu`, `quic.min_mtu` | `TransportConfig::initial_mtu`·`min_mtu` | 1200 B | [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §14 최소 데이터그램 크기 |
| `quic.mtu_discovery.interval` | `MtuDiscoveryConfig::interval` | 600 s | quinn 기본 |
| `quic.mtu_discovery.upper_bound` | `MtuDiscoveryConfig::upper_bound` | 1452 B | quinn 기본 |
| `quic.mtu_discovery.black_hole_cooldown` | `MtuDiscoveryConfig::black_hole_cooldown` | 60 s | quinn 기본 |
| `quic.mtu_discovery.minimum_change` | `MtuDiscoveryConfig::minimum_change` | 20 B | quinn 기본 |
| `quic.crypto_buffer_size` | `TransportConfig::crypto_buffer_size` | 16 KiB | quinn 기본. handshake 메시지 버퍼 상한 |
| `quic.ack_frequency` | `TransportConfig::ack_frequency_config` | 끔(`None`, quinn 기본) | 켜면 아래 세 값이 쓰인다 |
| `quic.ack_frequency.ack_eliciting_threshold`, `quic.ack_frequency.max_ack_delay`, `quic.ack_frequency.reordering_threshold` | `AckFrequencyConfig`의 같은 이름 | 1, 없음(상대 값), 2 | quinn 기본. 위가 꺼져 있어 적용 안 됨 |
| `quic.datagram_receive_buffer_size` | `TransportConfig::datagram_receive_buffer_size` | 끔(`None`, quinn 기본은 1 250 000 B) | QUIC datagram을 쓰지 않는다 |
| `quic.datagram_send_buffer_size` | `TransportConfig::datagram_send_buffer_size` | 1 MiB | quinn 기본. datagram이 꺼져 있어 적용 안 됨 |
| `quic.cubic.initial_window` | `CubicConfig::initial_window`(`congestion_controller_factory`로 넣는다) | 12 000 B | quinn 기본(14 720을 1200 B의 2~10배로 자름, [RFC 9002](https://www.rfc-editor.org/rfc/rfc9002) §7.2). 혼잡 제어 알고리즘은 CUBIC으로 고정한다(아래 "고정값") |
| `quic.cid_lifetime` | `HashedConnectionIdGenerator::set_lifetime`(`EndpointConfig::cid_generator`로 넣는다) | 없음(`None`, quinn 기본. 연결 ID를 수명으로 바꾸지 않는다) | quinn 기본 |
| `quic.max_udp_payload_size` | `EndpointConfig::max_udp_payload_size` | 1472 B | quinn 기본(이더넷 MTU − IP·UDP 머리) |
| `quic.min_reset_interval` | `EndpointConfig::min_reset_interval` | 20 ms | quinn 기본. stateless reset 송신 간격 하한 |
| `quic.retry_token_lifetime` | `ServerConfig::retry_token_lifetime` | 15 s | quinn 기본 |
| `quic.validation_token.lifetime` | `ValidationTokenConfig::lifetime` | 14일 | quinn 기본 |
| `quic.validation_token.sent` | `ValidationTokenConfig::sent` | 0 | quinn 기본(`bloom` feature를 끄면 0). 이 설계는 `bloom`을 켜지 않는다 |
| `quic.max_incoming` | `ServerConfig::max_incoming` | 65 536 | quinn 기본. 받는 쪽(B, 서버)의 handshake 대기 상한 |
| `quic.incoming_buffer_size` | `ServerConfig::incoming_buffer_size` | 10 MiB | quinn 기본 |
| `quic.incoming_buffer_size_total` | `ServerConfig::incoming_buffer_size_total` | 100 MiB | quinn 기본 |
| `tls.max_fragment_size` | rustls `ClientConfig`·`ServerConfig::max_fragment_size` | 없음(rustls 기본) | QUIC은 TLS 레코드를 쓰지 않아 적용 안 됨([RFC 9001](https://www.rfc-editor.org/rfc/rfc9001) §4) |

`quic.max_bidi_streams`, `quic.max_uni_streams`, `quic.keepalive_interval`, `quic.idle_timeout`, `quic.allow_migration`도 의존성 setter(`max_concurrent_bidi_streams`·`max_concurrent_uni_streams`·`keep_alive_interval`·`max_idle_timeout`·`ServerConfig::migration`)로 들어가는 값이고 위 "이 레포가 정하는 값" 표에 있다.

### 고정값 (설정 불가, 조회에는 보임)

와이어 형식이나 인증 전제를 깨는 의존성 값이다. 설정 구조체와 설정 파일에 키가 없어 바꿀 수 없고, `settings()`에는 `source = fixed`와 이유로 보인다.

| 키 | 고정값 | 이유 |
|---|---|---|
| `quic.grease_quic_bit` | 끔(`EndpointConfig::grease_quic_bit(false)`, quinn 기본 켬) | 켜면 상대가 QUIC 고정 비트를 0으로 보낼 수 있어 짧은 헤더는 첫 바이트가 0–63, 긴 헤더는 128–191이 된다. 첫 바이트 분류가 두 범위 모두 QUIC으로 보내지 않는다([RFC 9287](https://www.rfc-editor.org/rfc/rfc9287) §3.1, [RFC 9443](https://www.rfc-editor.org/rfc/rfc9443) §1·§3, 위 "소켓 하나와 패킷 나누기") |
| `quic.supported_versions` | QUIC v1 하나 | 첫 바이트 분류와 와이어 형식이 v1 기준이다. 클라이언트가 Initial에 쓰는 판(`ClientConfig::version`)도 quinn 기본 v1 그대로 둔다 |
| `quic.preferred_address` | 없음 | 서버가 다른 주소로 옮기라고 하면 펀칭한 경로를 벗어난다 |
| `quic.congestion_controller` | CUBIC | 알고리즘 선택은 동작값이 아니라 구현 선택이다. 바꾸면 위 `quic.cubic.*`의 뜻이 바뀐다 |
| `tls.versions` | TLS 1.3만 | QUIC은 TLS 1.3만 쓴다([RFC 9001](https://www.rfc-editor.org/rfc/rfc9001)) |
| `tls.provider` | ring | 위 "외부 의존과 고정" |
| `tls.resumption` | 끔(`Resumption::disabled()`, rustls 기본은 켬) | 재개된 handshake는 CertificateVerify를 거치지 않는다([RFC 8446](https://www.rfc-editor.org/rfc/rfc8446) §2.2). 매 연결이 키 소유를 증명한다는 인증 전제(아래 "인증")가 깨진다 |
| `tls.tickets` | 0(rustls 서버 `send_tls13_tickets`, rustls 기본 2) | 재개를 끄므로 티켓을 보내지 않는다. 클라이언트의 티켓 요청([RFC 9149](https://www.rfc-editor.org/rfc/rfc9149))에 따른 발급 상한 `max_tls13_tickets`도 rustls 기본 0(요청을 보지 않는다) 그대로 둔다 |
| `tls.early_data` | 끔(클라이언트 `enable_early_data` false, 서버 `max_early_data_size` 0) | 0-RTT는 재개 위에서만 서고 재전송 공격에 열린다. quinn의 기본 생성 함수는 이것을 켜므로 이 설계는 rustls 설정을 직접 만든다 |
| `tls.alpn` | `tasty-net/2`, `tasty-rendezvous/1` | 와이어 형식([버전·호환](versioning.md)) |

잘못된 설정의 거절(`bind`와 서버 시작이 한다. 어긋나면 키와 값을 담은 오류로 돌려주고 소켓을 열지 않는다):

1. 고정값 키를 주면 모르는 키로 거절한다(설정 파일 파서는 모르는 키를 받지 않는다).
2. 의존성이 받지 않는 범위를 거절한다. 예: `quic.initial_mtu < 1200`, `quic.min_mtu > quic.initial_mtu`, `quic.mtu_discovery.upper_bound < quic.initial_mtu`, VarInt 범위(2^62)를 넘는 창·스트림 수, `quic.idle_timeout`이 VarInt 밀리초를 넘음.
3. 관계를 거절한다. 예: `quic.idle_timeout ≤ quic.keepalive_interval`, `rendezvous.idle_timeout ≤ rendezvous.keepalive_interval`, `punch.max_candidates`가 1–64 밖, `history.limit = 0`.
4. 단방향 스트림 상한처럼 와이어·인증 전제를 깨지 않는 동작값은 범위 안이면 받는다(예: `quic.max_uni_streams`를 0보다 크게 주면 상대가 연 단방향 스트림을 quinn이 받되 이 레포는 읽지 않고 닫는다).

### 의존성 내부 값 (설정 불가)

quinn-proto와 rustls가 노출하지 않는 값이다. 바꿀 수 없으므로 이름과 출처만 적는다. 판을 올렸을 때 이 중 설정 함수가 생긴 것이 있으면 `dependency_setters_are_covered`가 찾고 위 표로 옮긴다.

| 값 | 출처 |
|---|---|
| 손실 탐지 시간 단위 kGranularity(1 ms) | RFC 9002 §6.1.2, quinn-proto 내부 |
| PTO 지수 backoff(두 배씩) | RFC 9002 §6.2.1, quinn-proto 내부 |
| closing·draining 기간(PTO의 세 배) | RFC 9000 §10.2, quinn-proto 내부 |
| 경로 검증 기한(PTO의 세 배 이상) | RFC 9000 §8.2.4, quinn-proto 내부 |
| idle timeout 하한(PTO의 세 배) | RFC 9000 §10.1, quinn-proto 내부(아래 "연결 유지") |
| CUBIC 계수 β(0.7)와 C(0.4) | quinn-proto `congestion/cubic.rs`의 상수. setter가 있는 값은 `initial_window` 하나다 |

### 와이어 형식 상수 (설정 아님)

| 상수 | 값 | 정의 |
|---|---|---|
| 첫 바이트 분류 | STUN 0–3, QUIC 64–127·192–255 | 위 "소켓 하나와 패킷 나누기" |
| STUN magic cookie, 헤더 길이 | 0x2112A442, 20 B | RFC 8489 §5 |
| `TASTY-PUNCH` 속성 번호, 토큰 길이 | 0xC0A1, 16 B | [rendezvous](rendezvous.md#펀칭-프로브) |
| 조정 프레임 길이 접두 | 4 B big-endian | [rendezvous](rendezvous.md#프레임) |
| ALPN | `tasty-net/2`, `tasty-rendezvous/1` | 아래 "인증", [버전·호환](versioning.md) |
| QUIC 종료 코드 | 기기 사이 0·1·4, 조정 연결 0~6 | [handshake v2](handshake.md#quic-종료-코드), [rendezvous](rendezvous.md#메시지-표) |
| ed25519 SPKI 앞머리 | 12 B | RFC 8410, 아래 "인증" |
| ed25519 PKCS#8 v1 앞머리, 키 길이 | 16 B, 32 B | RFC 8410 §7, RFC 8032 |
| STUN 메시지 종류 | Binding 요청 0x0001, 성공 응답 0x0101, 종류 위 두 비트 0 | RFC 8489 §5·§6 |
| STUN 속성 형식 | `XOR-MAPPED-ADDRESS` 0x0020, 주소족 IPv4 0x01·IPv6 0x02, 속성 머리 4 B·4 B 정렬, 0x8000 미만은 이해 필수, 트랜잭션 ID 12 B | RFC 8489 §5·§14·§14.2 |
| 조정 프로토콜 판 `proto` | 1 | [rendezvous](rendezvous.md#메시지-표) `hello` |
| QUIC 판 | v1 (0x00000001) | RFC 9000 §15, 위 고정값 `quic.supported_versions` |
| 펀칭 토큰 글자 | 소문자 hex 32자(16바이트, 바이트마다 두 자) | [rendezvous](rendezvous.md#메시지-표) `punch_go` |
| 호스트 후보 IPv6 범위 | `2000::/3` | [rendezvous](rendezvous.md#후보-주소-규칙) |

## 연결 정보

호출자가 "지금 이 기기가 어떤 기기와, 어떤 주소 쌍으로, 어느 서버를 통해 소개받아 연결돼 있는지"를 볼 수 있게 한다. 세 경로(P2P, SSH 정방향, SSH 역할 반전)를 한 목록에 모으는 것은 transport의 연결 목록이다([전송 추상화](transport.md#연결-목록)). P2P 크레이트는 자기 연결의 정보만 내보낸다.

- `P2pEndpoint::status()`: endpoint 상태(위 표), 서버 주소와 서버 공개키, 로컬 소켓 주소, 반사 주소, `NatMapping`, 서버 한계값, 재연결 시도 번호와 다음 시도까지 남은 시간.
- `P2pEndpoint::connections()`: 연결과 진행 중 시도마다 `P2pConnectionInfo`.

| 필드 | 뜻 |
|---|---|
| `id` | 이 endpoint 안에서 고유하고 바뀌지 않는 연결 번호. 시도 하나(그 안의 펀칭 재시도 포함)와 그 결과 연결 하나가 같은 번호다 |
| `peer` | 상대 기기 ID |
| `side` | `dialer`(A) · `acceptor`(B) |
| `state` | 위 상태 기계의 상태와 그 상태의 값 |
| `local_bind` | 소켓이 바인딩한 주소(예: `[::]:54321`). wildcard일 수 있다 |
| `local_ip` | 이 연결에 실제로 쓰인 로컬 IP. B(QUIC 서버)는 quinn `Connection::local_ip()`가 주는 경우에 채운다(`PacketIo`가 받은 패킷의 목적지 IP를 quinn에 넘길 때). A(QUIC 클라이언트)는 quinn 0.11.12에서 `local_ip()`가 언제나 `None`이므로 늘 `unknown`이다. A의 로컬 쪽은 `local_bind`(포트 포함)와 `local_reflexive`로 보인다 |
| `local_reflexive` | 펀칭 때 서버가 본 이 기기의 반사 주소(서버가 `welcome.reflexive`·`reflexive_changed`로 알린 값). A·B 모두 채워진다 |
| `remote_addr` | quinn이 지금 쓰는 상대 주소(`Connection::remote_address()`). 연결 전에는 비어 있다 |
| `remote_candidate` | 성공한 후보 주소와 종류(`server_reflexive` · `reported_reflexive` · `host` · `peer_reflexive`). 위 주소 바뀜이 있으면 `remote_addr`와 달라질 수 있다 |
| `rendezvous` | 소개해 준 서버 주소와 서버 공개키 |
| `nat` | 양쪽 `NatMapping`(이쪽은 판별 결과, 상대는 `punch_go.peer_nat`)과 `mode`(`all`·`host_only`) |
| `attempt` | 지금 몇 번째 펀칭 시도인지, `RetryWait`면 다음 시도까지 남은 시간, 시도 전체 상한까지 남은 시간, `Punching`이면 지금의 후보 목록과 창 끝까지 남은 시간, `Handshaking`이면 주소 쌍(바인딩 주소와 상대 주소)과 handshake 기한까지 남은 시간 |
| `rtt` | quinn RTT 추정 |
| `last_cause` | 마지막 전이의 원인 |
| `history` | 전이와 관측된 원격 주소 변화의 이력(최대 `history.limit`개, 버린 수 함께) |
| `applied` | 이 연결에 적용된 설정값. 항목마다 값과 출처(`default`·`configured`·`os_default`·`fixed`, 서버에서 받은 창·기한·시작 지연·한계값은 `server`) |

## 인증

고른 방식: RFC 7250 raw public key. 기기 키(ed25519)의 SubjectPublicKeyInfo를 TLS 인증서 대신 그대로 보낸다. 서버 연결과 기기 사이 연결이 같은 방식이다.

- 근거:
  1. rustls는 0.23.16부터 raw public key를 보내는 resolver(`client::AlwaysResolvesClientRawPublicKeys`, `server::AlwaysResolvesServerRawPublicKeys`)를 두고, 받는 쪽 검증기가 raw public key를 요구하는 길(`requires_raw_public_keys`)과 서명 검증 함수(`crypto::verify_tls13_signature_with_raw_key`)를 준다. 이 설계는 0.23.46(2026-11-27 최신)으로 고정한다. TLS 1.3에서만 되며 QUIC은 TLS 1.3만 쓴다([RFC 9001](https://www.rfc-editor.org/rfc/rfc9001)).
  2. 자체 인증서 방식은 X.509 생성(`rcgen` 등)과 파싱이 더 들어온다. 확인할 것은 "상대가 이 ed25519 키를 가졌는가" 하나뿐이라 X.509의 이름·기간·확장은 쓸 데가 없다. 덜 들어오고 공격 면이 작은 쪽을 고른다.
  3. TLS 1.3의 CertificateVerify가 개인키 소유를 증명한다([RFC 8446](https://www.rfc-editor.org/rfc/rfc8446) §4.4.3). 그래서 그 위에서 서명 증명을 다시 하지 않는다.
- 확인 범위와 대체: 위 API가 있다는 것과 구현 경로는 배포 소스로 확인했다. quinn이 그 rustls 설정을 그대로 받아 handshake가 서는지, ring provider로 ed25519 CertificateVerify가 검증되는지는 구현 첫 단계 시험으로 확인한다. 안 되면 기기 키로 만든 자체 서명 인증서와 SPKI만 보는 검증기로 바꾸고 이 절을 고친다. 바깥 계약(상대 기기 ID 확인)은 같다.
- 키 변환: 기기 키 seed 32바이트로 PKCS#8 v1 DER(ed25519, [RFC 8410](https://www.rfc-editor.org/rfc/rfc8410))을 만들어 rustls 서명 키로 넣는다. 보내는 "인증서"는 SPKI DER 하나이고, SPKI는 고정 앞머리 12바이트 + 공개키 32바이트다. 받은 SPKI는 이 길이와 앞머리(ed25519 OID)가 정확히 맞을 때만 받고, 그 32바이트를 기기 ID로 쓴다.
- 보내는 쪽(키 고르기): 클라이언트는 `AlwaysResolvesClientRawPublicKeys`, 서버는 `AlwaysResolvesServerRawPublicKeys`에 기기 키(서버는 서버 키)의 `CertifiedKey`를 넣는다. 키 고르기는 소유 증명이 아니다. 증명은 받는 쪽 검증기의 CertificateVerify 검증이다.
- 받는 쪽(검증):
  - 클라이언트 쪽 검증기(`rustls::client::danger::ServerCertVerifier` 구현): `requires_raw_public_keys`가 참, `verify_server_cert`가 SPKI 형식을 보고 그 연결의 기대 키와 비교, `verify_tls13_signature`가 `verify_tls13_signature_with_raw_key`로 CertificateVerify를 검증, `supported_verify_schemes`는 ED25519 하나, TLS 1.2 서명 검증은 항상 실패(TLS 1.2를 켜지 않는다).
  - 서버 쪽 검증기(`rustls::server::danger::ClientCertVerifier` 구현): `requires_raw_public_keys`와 `client_auth_mandatory`가 참, SPKI 형식 검사, 같은 서명 검증. 받을 기기를 여기서 고르지 않는다(아래).
  - 기대 키는 연결마다 다르다. dialer는 연결마다 그 기대 키(P2P는 상대 기기 ID, 서버 연결은 고정한 서버 공개키)를 담은 rustls 설정을 만들어 quinn `Endpoint::connect_with`로 건다.
  - 세션 재개와 0-RTT는 끈다(위 "고정값"). 재개된 handshake는 CertificateVerify를 거치지 않기 때문이다.
  - handshake 뒤 quinn `Connection::peer_identity()`는 rustls의 인증서 목록(`Vec<CertificateDer>`)을 주고, raw public key일 때 그 하나가 SPKI다. 이것으로 `remote_device_id()`를 만든다.
- dialer: 검증기가 상대 키가 요청한 기기 ID와 다르면 handshake를 실패시키고 `PeerIdentityMismatch`다. 서버 이름(SNI)은 고정값 `tasty-p2p`이고 검증에 쓰지 않는다.
- acceptor: 검증기는 형식이 맞고 서명이 맞는 ed25519 raw public key면 받는다. 연결이 선 직후 그 기기 ID가 펀칭 세션의 상대인지([펀칭 절차](#펀칭-절차) 9, 다르면 `session_mismatch`)와 페어링 허용 목록에 있는지(없으면 `not_paired`)를 본다([handshake v2](handshake.md#quic-종료-코드)). `punch_offer` 확인이 이미 걸렀으므로 보통은 생기지 않는 경로다.
- handshake v2와의 정리: 지금 [handshake v2](handshake.md#흐름-p2p-경로)의 P2P 흐름(토큰·nonce·서명 없음, 메시지 1의 `device_id`가 연결의 상대 기기 ID와 같은지만 봄, hello → ACK)을 그대로 쓴다. 기기 확인은 TLS가 한 번만 하고, handshake v2는 그 결과와 메시지 필드가 맞는지만 본다. 인스턴스 ID 비교, 역할, `caps`, bulk 자격은 handshake v2 몫으로 남는다.
- 서버 인증: 기기는 서버의 raw public key를 설정에서 받은 값과 비교한다(키 고정). 서버는 기기의 raw public key로 기기 ID를 정한다. 자세한 것은 [rendezvous](rendezvous.md#기기-인증).
- ALPN: 기기 사이 연결은 `tasty-net/2`, 서버 연결은 `tasty-rendezvous/1`이다. 기기 사이 ALPN 숫자는 handshake `proto`와 같이 움직인다([버전·호환](versioning.md)).

## endpoint와 런타임

- `P2pEndpoint::bind(config)`가 tokio 런타임을 만들고 소켓을 연다. config: `enabled`, 기기 키, rendezvous 서버(주소와 서버 공개키), 페어링 허용 목록 저장소, `settings`([설정값](#설정값)). 켜져 있는데 서버를 주지 않으면 `bind`가 오류다.
- 소유 런타임의 작업 스레드 수와 종료 대기 상한은 설정값이다(`runtime.worker_threads`·`runtime.shutdown_timeout`). 비동기 작업 안에서 블로킹 API를 부르면 `P2pError::InAsyncContext`다(`spawn_blocking` 안은 된다). `tokio::task::coop::unconstrained`로 감싼 작업은 가려내지 못해 거절되지 않고 그 워커를 막는다(한계는 [전송 추상화](transport.md#동기비동기-경계)).
- endpoint는 마지막 clone과 그 endpoint의 마지막 `P2pConnection`·스트림이 모두 drop될 때, 또는 `close()`에서 끝난다(연결만 쥐고 endpoint를 놓아도 연결은 계속 쓸 수 있다). 끝날 때는 새 블로킹 호출을 받지 않고, 런타임 안에서 막힌 호출(`accept` 등)을 `P2pError::Closed`로 깨우고, QUIC을 `closed`로 닫고, 받지 않은 연결과 소켓을 놓은 뒤 `Off`가 된다(`Off`는 소켓 없음). `close()` 뒤의 블로킹 호출은 런타임에 들어가지 않고 `Closed`(스트림은 `ConnectionAborted`)다.
- 동기 API와 소유 런타임의 관계(블로킹 호출, 소유 스레드에서의 종료·회수, 비동기 작업 안에서 부르면 패닉 대신 오류)는 [전송 추상화](transport.md#동기비동기-경계)에 적힌 규칙을 이 크레이트가 자기 API에 지킨다. 어댑터는 그 위에 아무것도 더하지 않는다.
- 같은 기기 키로 P2P endpoint는 하나만 연다. 서버는 같은 기기 ID의 등록을 하나만 두므로(새 등록이 옛 등록을 밀어낸다), 같은 키로 둘을 열면 서로를 밀어낸다. `bind`는 키 디렉터리의 잠금 파일을 배타로 잡고, 이미 잡혀 있으면 `P2pError::EndpointInUse`를 돌려준다. 한 프로세스 안에서는 키 디렉터리를 주지 않아도 같은 기기 ID의 켜진 endpoint를 하나만 둔다(프로세스 전역 목록, 두 번째 `bind`는 `EndpointInUse`). 잠금과 목록 자리는 `Off`에서 놓는다. 독립 P2P endpoint가 필요한 인스턴스는 다른 키 디렉터리를 쓴다. SSH 경로는 이 제약 없이 같은 키를 함께 쓴다.
- 자기 기기 ID로 거는 연결은 네트워크에 보내기 전에 `SelfAttach`로 끝낸다. 서버도 같은 요청을 `self`로 거절한다.

## 스트림과 연결 (어댑터와의 경계)

`tasty-net-p2p`는 전송 계약을 모른다. 바이트 스트림과 연결 수명만 준다. 링크·handshake·연결 공유 규칙은 transport 어댑터가 얹는다.

| `tasty-net-p2p`가 주는 것 | transport 어댑터가 하는 것 |
|---|---|
| `P2pEndpoint::connect(device_id)`·`accept()` → `P2pConnection` | `Transport`·`Listener` 구현, `AttemptCancel` 등록, 연결 공유 목록 |
| `P2pConnection::open_bi()`·`accept_bi()` → `P2pStream`(동기 읽기·쓰기, 읽기·쓰기 시간 제한, `finish`, `reset`·`stop`, 막힌 호출 깨우기) | `RawChannel`로 감싸기, 프레임 읽기·쓰기, handshake v2 |
| `P2pConnection::close(P2pCloseReason)`, `close_reason()`, `remote_device_id()`, `rtt()` | 링크·채널 닫기 범위, 마지막 채널 뒤 연결 닫기, 토폴로지 엣지의 `rtt_ms` |
| QUIC 오류 → `std::io::ErrorKind` 분류 | `ReadFailure` 분류(전송과 무관한 같은 표) |
| `P2pCloseReason`과 QUIC 종료 코드 표의 정의 | handshake 오류와 링크에 그 값을 싣는다 |

- 채널 대응(순서 채널 = `purpose = attach`로 연 양방향 스트림, bulk = 같은 연결의 추가 양방향 스트림, 요청 채널 = `purpose = request` 스트림)과 P2P의 닫기 범위는 QUIC 일반의 성질이라 iroh 설계의 내용을 그대로 쓴다([전송 추상화](transport.md#순서-계약과-채널), [전송 추상화](transport.md#p2p의-닫기-범위)).
- `P2pCloseReason`은 이 크레이트가 정의한다(QUIC 종료 코드가 이 크레이트의 와이어 값이기 때문이다). transport는 그것을 다시 내보내 `HandshakeError::Closed`와 `Link::close_reason()`에 쓴다([handshake v2](handshake.md#quic-종료-코드)).
- 스트림 수 상한: 연결마다 양방향 스트림 동시 `quic.max_bidi_streams`(기본 64), 단방향 동시 `quic.max_uni_streams`(기본 0, 설정 가능)([설정값](#설정값)).

## 페어링

- 페어링은 양쪽 사람이 승인한다. 기기를 찾는 키는 기기 ID 하나이고, 주소는 저장하지 않는다(서버가 연결할 때마다 알려 준다). 그래서 페어링 메시지는 `PairingOffer{device_id, display_name}`, `PairingAccept{device_id, display_name}`이다. 이 레포는 형식과 허용 목록 검사를 준다. 메시지를 나르는 경로(이미 있는 attach 링크, 사용자가 복사하는 문자열 등)와 승인 화면은 tasty가 정한다.
- 허용 목록은 두 곳에서 본다. `punch_offer`를 받을 때(없으면 거절, 주소를 내주지 않는다)와 QUIC 연결을 받은 직후(없으면 `not_paired`로 닫는다).

## 펀칭 실험

목적: 서버 설정 `punch_window`·`quic_handshake_timeout`과 기기 설정 `punch.probe_interval`의 기본값을 실측으로 정하고, 망 조합별 성공률을 기록한다.

검증 범위(사용자 결정 2026-11-27, Q1): 펀칭 실험용 기기·망·공인 서버는 hub rendezvous 배포 때 함께 준비한다. 그 전에는 같은 망 안 기기와 로컬에서 띄운 서버로만 확인한다. CI는 프로세스 안 서버와 모의 NAT로 확인한다([CI와 시험 환경](testing.md#p2p-시험)).

### 필요한 조건 (hub rendezvous 배포 때)

| 항목 | 조건 |
|---|---|
| 기기 | 최소 2대. 각 기기에서 `punch_probe` 예제를 실행할 수 있어야 한다(Linux·macOS·Windows 중 아무거나). UDP 차단 조합을 하려면 방화벽 규칙을 바꿀 수 있는 기기 1대 |
| 네트워크 A | 가정용 공유기 뒤. 보통 EIM NAT |
| 네트워크 B | A와 다른 망. 모바일 테더링(이동통신사 CGNAT, APDM일 수 있음)을 1순위로 한다 |
| 추가 조합 | (1) 두 기기가 같은 공유기 안, (2) 서버에는 닿지만 기기 사이 UDP가 막힌 상태(방화벽으로 상대 주소만 막음), (3) 가능하면 양쪽 모두 `dependent`(호스트 후보만 시도하고 `NatUnsupported`가 나는지 확인) |
| 서버 위치 | 공인 IPv4 주소가 있는 호스트 1대. 두 네트워크 어디에서도 닿아야 한다. 사설망 안의 머신은 안 된다. 주소 의존 판별까지 보려면 공인 IP 2개 |
| 서버 포트 | UDP P1·P2 외부 개방([rendezvous](rendezvous.md#포트와-전송)) |

측정 항목: 요청부터 성공 판정까지의 시간, 성공 판정부터 QUIC handshake 완료까지의 시간, 성공한 후보 종류(반사·호스트·peer reflexive), 두 기기의 `NatMapping`, 실패 사유. 반복 횟수는 실험 작업 지시로 정한다.

공개 문서에 실험 기록을 적을 때 기기 이름, 공인 IP, 사설 경로를 쓰지 않는다. 네트워크 종류만 적는다.

### 결과 (실험 뒤 채운다)

| 조합 | `NatMapping`(A/B) | 성공/시도 | 성공까지 시간 중앙값·최댓값 | 성공 후보 종류 |
|---|---|---|---|---|
| (미측정) | | | | |

확정 값: 서버 `punch_window` = (미정), 서버 `quic_handshake_timeout` = (미정). 확정 근거는 이 표의 행을 가리킨다.

### 동작 수준 기준과의 관계

펀칭 실험은 "제대로 동작하는 수준"([동작 수준 기준](done-criteria.md)) 기준에 넣지 않는다. 대신 "P2P를 사용자에게 내놓기 전 기준"(같은 문서 P1)에 넣는다. 근거:

1. "제대로 동작하는 수준"은 tasty 본체에서 네트워크 코드를 걷어내기 시작해도 되는지 판단하는 기준이다. 걷어내기의 첫 대상은 지금 tasty에 있는 SSH attach다. P2P는 지금 tasty에 없는 새 기능이라, 펀칭 값이 정해지지 않아도 걷어내기를 시작할 수 있다.
2. 펀칭 절차와 정책이 맞게 동작하는지(성공, `NatUnsupported`, 막힌 UDP에서 시간 안의 실패, 꺼짐, 다른 서버)는 CI의 모의 NAT 시험이 확인한다. 실험이 정하는 것은 코드가 아니라 시간 값과 실제 망의 성공률이다.
3. 실험은 사람이 서로 다른 두 네트워크의 기기와 공인 서버를 준비해야 한다. 일정은 hub rendezvous 배포에 묶여 있다(Q1).
4. 값이 정해지지 않은 채 공개하면 문제가 되는 곳은 hub rendezvous 배포와 P2P 사용자 공개다. 그래서 그 두 일의 선행 조건으로 둔다.
