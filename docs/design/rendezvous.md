# rendezvous 서버

크레이트는 `tasty-net-rendezvous`(라이브러리 + 실행 파일 `tasty-rendezvous`)다. 이 서버는 두 가지만 한다(사용자 결정 2026-11-27).

1. 반사: 기기에게 바깥에서 보이는 주소를 알려 준다(STUN Binding).
2. 조정: 같은 서버에 붙은 두 기기의 동시 펀칭을 조정한다.

기기 쪽 절차(NAT 판별, 펀칭, 정책, 상태 기계)는 [P2P](p2p.md)에 있다. 이 문서는 서버 프로토콜, 서버 상태, 배포를 다룬다. hub가 운영하는 rendezvous 서버를 이 문서에서 hub rendezvous라고 부른다.

## 중계 없음

서버는 데이터를 중계하지 않는다. 이것을 설정이나 상한이 아니라 구조로 보장한다.

- 서버에는 한 기기에서 받은 바이트를 다른 기기에게 보내는 코드 경로가 없다. 서버가 기기에게 보내는 것은 서버가 자기 상태로 만든 정해진 형식의 메시지(아래 표)와 STUN 응답뿐이다. 임의 바이트나 임의 JSON을 담는 메시지 종류와 필드가 없다.
- 서버가 상대에게 넘기는 제어 정보는 아래 다섯 가지로 닫혀 있다. 이것은 펀칭을 위한 제어 메타데이터이고 데이터 경로가 아니다.

  | 넘기는 값 | 출처와 정규화 |
  |---|---|
  | 상대 기기 ID | TLS가 확인한 기기 키(기기가 적은 값이 아니다) |
  | 서버가 본 반사 주소 | 조정 연결의 원격 주소(서버 관측) |
  | 기기가 보고한 보조 반사 주소 | `nat_report.stun_results`의 주소를 IP·포트로 파싱한 값. 서버가 `welcome.stun`으로 알린 주소에 대한 결과만, 아래 "후보 주소 규칙"을 통과한 것만, 개수 상한 안에서 |
  | 기기가 보고한 호스트 주소 | `hello`·`nat_report`의 `host_candidates`를 IP·포트로 파싱한 값. 아래 "후보 주소 규칙"을 통과한 것만, 개수 상한 안에서 |
  | 상대의 `NatMapping`, 펀칭 토큰, 시간 값 | 서버가 정한 열거값과 서버가 만든 난수·설정값 |

### 후보 주소 규칙

서버가 `punch_go.candidates`에 싣는 주소와 기기가 받은 후보에서 프로브 대상으로 쓰는 주소는 같은 규칙으로 거른다. 규칙의 정본은 이 크레이트의 와이어 형식 모듈이고 서버와 `tasty-net-p2p`가 같은 함수를 쓴다.

- 허용 규칙(한 문장): 호스트 후보는 사설 IPv4([RFC 1918](https://www.rfc-editor.org/rfc/rfc1918))와 전역 유니캐스트 IPv6(`2000::/3`)만, 반사 후보(`server_reflexive`·`reported_reflexive`)는 미지정·멀티캐스트·브로드캐스트·링크 로컬이 아닌 주소만 받고, 모든 후보의 포트는 1–65535이며, 그 밖은 버리고 버린 수를 센다.
- 반사 후보에 루프백을 막지 않는 것은 같은 머신의 루프백 서버로 도는 예제와 시험 때문이다. 서버 반사 주소는 서버가 관측한 값이다. 보고 반사 주소는 기기의 주장이므로 종류를 `reported_reflexive`로 따로 표시하고, 서버 반사 주소와 주소족이 같은 것만 받는다.
- 중복 제거: 같은 주소·포트는 하나만 남기고, 앞선 종류를 남긴다.
- 우선순위와 상한: 서버는 `server_reflexive`(1개) → `reported_reflexive`(최대 `max_reported_reflexive`) → `host`(기기가 보낸 순서로 최대 `max_host_candidates`) 순으로 싣는다. `mode = host_only`면 `host`만 싣는다. 그래서 `punch_go.candidates`는 `1 + max_reported_reflexive + max_host_candidates`개를 넘지 않는다.
- 상한의 관계: `1 + max_reported_reflexive + max_host_candidates ≤ punch.max_candidates`(기기 설정)여야 한다. 기기는 `welcome.limits`의 두 값으로 이것을 검사하고, 어긋나면 `server_limits_mismatch`를 낸다(아래). 우선순위에 따른 절단은 서버의 정상 동작이다. 기기 상한을 넘는 후보가 실린 `punch_go`가 기기에 도착하면 서버가 이 규칙을 어긴 것이므로, 기기는 그 후보를 잘라 쓰지 않고 `limits_invalid`로 끝낸다(아래 "한계값 검사" 2).
- 기기는 `host_candidates`를 자기 우선순위(사설 IPv4, 전역 IPv6 고정 주소, 전역 IPv6 임시 주소)로 정렬해 보낸다.

- 기기가 보낸 메시지의 모르는 필드는 파싱할 때 버린다. 저장하지도 넘기지도 않는다. 조정 스트림은 연결마다 하나만 받고, 둘째 스트림은 연결을 `malformed`로 닫는다.
- STUN 응답은 요청이 온 주소로만 보낸다. 다른 주소로 보내라는 요청(RFC 5780의 RESPONSE-PORT·CHANGE-REQUEST)은 받지 않는다.
- 기기 사이 프로브, 기기 사이 QUIC 연결, 그 연결 유지와 닫기는 서버를 지나지 않는다([P2P](p2p.md#흐름)의 8~10, 12~13).
- 이 성질은 시험(`server_never_forwards_device_bytes`, `unknown_fields_are_not_forwarded`, `second_stream_is_refused`, [CI와 시험 환경](testing.md#p2p-시험))과 리뷰 규칙(서버 송신 함수는 메시지 enum과 STUN 응답 두 가지만 받는다)으로 지킨다.

iroh 설계 단계의 relay 모드(`punch-only`·`relay-fallback`), 수신 상한, `enable_relay`, 접속 허용 콜백(`access.http.url`)은 이 구조에 해당하는 것이 없어 없앤다.

## 포트와 전송

모두 UDP다. TCP는 쓰지 않는다. P2P 자체가 UDP를 요구하므로 UDP가 막힌 망에서 서버만 TCP로 닿아도 쓸모가 없다.

| 주소 | 기본 포트 | 받는 것 |
|---|---|---|
| IP1:P1(주) | 3478 | QUIC(조정 연결) + STUN Binding. 첫 바이트로 나눈다(0–3 STUN, 64–127·192–255 QUIC, 그 밖 버림. [RFC 7983](https://www.rfc-editor.org/rfc/rfc7983) §7, [RFC 9443](https://www.rfc-editor.org/rfc/rfc9443) §3. 이 서버에는 TURN이 없어 64–79도 QUIC이다) |
| IP1:P2(보조) | 3479 | STUN Binding만 |
| IP2:P1, IP2:P2(선택) | 3478, 3479 | STUN Binding만. 두 번째 공인 IP가 있을 때 |

- 3478은 STUN 기본 포트다([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489)). 포트는 서버 설정으로 바꿀 수 있다.
- 반사와 조정을 같은 주 포트에 두는 근거: 기기는 P2P 소켓 하나로 서버에 붙는다([P2P](p2p.md#소켓-하나와-패킷-나누기)). 서버가 조정 연결의 원격 주소로 보는 값이 곧 그 소켓의 반사 주소가 되고, 조정 연결의 keepalive가 같은 NAT 매핑을 살린다.
- NAT 판별에 보조 포트가 필요하다(같은 IP의 두 포트로 포트 의존을 본다). 주소 의존까지 보려면 두 번째 IP가 필요하다([P2P](p2p.md#nat-판별)).
- STUN은 Binding Request/Success Response와 XOR-MAPPED-ADDRESS만 쓰는 부분 집합이다([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §5·§14.2). 서버의 STUN 응답에는 인증 속성이 없다. 반사 주소는 공개돼도 되는 값이고, 조정 연결이 주 반사 주소를 TLS 위에서 다시 알려 준다.
- 크기와 증폭: 속성 없는 Binding Request는 헤더만 20바이트이고, XOR-MAPPED-ADDRESS 하나를 실은 Success Response는 IPv4 32바이트, IPv6 44바이트다(RFC 8489 §5의 20바이트 헤더 + §14.2 속성 4바이트 머리와 값 8·20바이트). 그래서 응답이 요청보다 1.6–2.2배 크다. 출발 주소를 속인 요청으로 제3자에게 응답을 보내게 하는 반사 공격에 쓰일 수 있다. 대응: 요청이 온 주소로만 답하고, 출발 주소마다 속도 상한(`stun_per_source_per_second`)을 둔다. 상한을 넘은 요청은 답하지 않고 센다.

### 광고 주소와 바인딩 주소

- 서버가 듣는 주소(`listen.*`, wildcard 가능)와 기기에게 알려 주는 주소(`advertise.*`)를 따로 둔다. wildcard 주소와 컨테이너 안 주소는 기기에게 알리지 않는다.
- 주 주소는 알리지 않는다. 기기는 자기가 접속한 주소(설정한 서버 `host:port`를 풀어 실제로 붙은 주소)를 주 STUN 주소로 쓴다.
- 보조 포트와 두 번째 IP는 `advertise.alternate_port`, `advertise.alternate_ip`로 준다. 서버는 이 값으로 `welcome.stun`을 만든다. 보조 포트를 듣는데 광고할 공인 주소를 정할 수 없으면(주 주소를 wildcard로 듣고 `advertise.primary_ip`도 없으면) 시작하지 않는다.
- 주소족: 서버는 기기가 붙은 주소족과 같은 족의 광고 주소만 `welcome.stun`에 싣는다. 기기도 자기 서버 연결과 같은 족의 주소에만 STUN을 보낸다.

## 기기 인증

- 조정 연결은 QUIC(ALPN `tasty-rendezvous/1`) 위의 상호 인증 TLS 1.3이다. 양쪽 모두 RFC 7250 raw public key를 쓴다. 키를 고르는 쪽과 받은 키를 검증하는 쪽의 구성은 [P2P](p2p.md#인증)와 같다.
- 서버가 기기를 확인하는 방법: 서버의 client 인증서 검증기는 raw public key를 요구하고(`client_auth_mandatory`), 받은 SPKI가 ed25519 형식인지 보고, TLS 1.3 CertificateVerify 서명을 그 키로 검증한다. 그 키가 곧 기기 ID다. 기기는 메시지에 자기 기기 ID를 싣지 않는다. 그래서 다른 기기 ID로 등록할 길이 없다.
- 기기가 서버를 확인하는 방법: 서버도 ed25519 서버 키를 가진다. 기기는 설정으로 받은 서버 공개키와 TLS의 raw public key를 비교한다(키 고정). 공용 인증 기관과 도메인 인증서는 쓰지 않는다. 서버 키는 `tasty-rendezvous keygen`으로 만들고, 시작할 때 공개키를 로그에 찍는다.
- 세션 재개와 0-RTT는 양쪽 모두 끈다. 매 연결이 전체 handshake를 하고 CertificateVerify를 거치게 하기 위해서다.
- 접속 허용: TLS가 끝난 직후 서버가 접근 정책으로 기기 ID를 본다. 라이브러리는 정책 trait `AccessPolicy`를 받는다. 실행 파일은 `everyone`(기본)과 `allowlist`(기기 ID 목록 파일) 둘을 준다. hub rendezvous가 누구를 받을지(로그인 사용자, 페어링된 기기 등)는 hub 쪽 결정이다([남은 결정](index.md#남은-결정)). 거절은 종료 코드 `access_denied`다.
- 서버가 기기를 찾는 키는 기기 ID(소문자 hex 64자, [기기 키·토큰](identity.md))다. `punch_request.peer`가 이 값이다.

## 메시지

### 프레임

- 기기가 연결을 연 뒤 양방향 스트림 하나(조정 스트림)를 연다. 연결마다 하나다.
- 프레임: 4바이트 big-endian 부호 없는 길이 + 그 길이의 UTF-8 JSON 객체 하나. 길이 0은 `malformed`다.
- 길이 상한은 서버 설정 `max_message_bytes`(기본 16 KiB)다. 설정값은 1 KiB 이상 1 MiB 이하만 받는다(u32와 모든 플랫폼의 `usize`에 들어간다). 읽는 쪽은 길이 4바이트를 다 받은 뒤 상한과 비교하고, 넘으면 본문을 읽거나 버퍼를 잡기 전에 `malformed`로 닫는다.
- 기기 쪽 수신 상한과 기한은 기기 설정이다. `welcome`을 받기 전에도 같은 값을 쓴다(첫 프레임을 읽기 전에는 서버 값을 알 수 없기 때문이다). 수신 상한은 `rendezvous.max_message_bytes`(기본 64 KiB, 1 KiB–1 MiB), 프레임 기한은 `rendezvous.frame_timeout`(기본 5 s, 서버 `frame_timeout`에 대응)이다. 넘으면 기기는 연결을 닫고 `io`(분류 `malformed`)로 backoff에 들어간다.
- 협상 뒤: `welcome`을 받으면 기기는 서버에 보내는 프레임을 `welcome.limits.max_message_bytes` 이하로 만든다. 수신 상한은 기기 값 그대로다. 서버 `max_message_bytes`가 기기 `rendezvous.max_message_bytes`보다 크면 `server_limits_mismatch`(서버가 기기가 받지 못할 크기를 보낼 수 있다)다.
- 부분 읽기: 한 번의 읽기에 프레임 일부만 오거나 여러 프레임이 붙어 와도 된다. 읽는 쪽은 프레임 경계로 다시 자른다. 첫 바이트를 받은 뒤 `frame_timeout`(서버 설정, 기본 5 s) 안에 그 프레임이 끝나지 않으면 `malformed`로 닫는다. 프레임 중간에 스트림이 끝나면 `malformed`로 본다.
- 최상위 키 `type`으로 메시지를 가른다. 모르는 `type`은 `malformed`, 모르는 필드는 버린다.
- 주소는 `"203.0.113.5:3478"`·`"[2001:db8::1]:3478"` 형식 문자열이다. 시간 값은 밀리초 정수(`*_ms`)다.
- `request_id`(기기가 정함)와 `punch_id`(서버가 정함)는 부호 없는 64비트 정수다. 기기 ID는 소문자 hex 64자 문자열이다.
- 목록 원소의 모양: `nat_report.stun_results`는 `{"server": 주소, "reflexive": 주소 또는 null}`의 목록, `punch_go.candidates`는 `{"addr": 주소, "kind": 종류}`의 목록이다. `nat_report.host_candidates`가 없으면 서버는 가진 호스트 주소를 그대로 둔다.
- 기기가 보낸 주소 문자열이 주소로 파싱되지 않으면 형식 오류가 아니라 버린 후보로 센다(아래 "후보 주소 규칙").

### 메시지 표

| `type` | 방향 | 필드 | 뜻 |
|---|---|---|---|
| `hello` | 기기 → 서버 | `proto`(지금 1), `host_candidates`(주소 목록) | 등록. 접속 뒤 `hello_timeout` 안에 와야 한다 |
| `welcome` | 서버 → 기기 | `reflexive`(서버가 본 원격 주소), `stun`(보조 STUN 주소 목록, 위 "광고 주소"), `limits` | 등록됨 |
| `reflexive_changed` | 서버 → 기기 | `reflexive` | 조정 연결의 원격 주소가 바뀐 것을 서버가 관측함. 기기는 NAT 판별을 다시 한다 |
| `nat_report` | 기기 → 서버 | `mapping`(`independent`·`independent_port_only`·`dependent`·`unknown`), `stun_results`(질의한 서버 STUN 주소마다 받은 반사 주소, 응답 없으면 `null`), `host_candidates` | NAT 판별 결과와 바뀐 호스트 주소 |
| `punch_request` | 기기 → 서버 | `request_id`, `peer` | 펀칭 요청 |
| `punch_refused` | 서버 → 기기 | `request_id`, `reason`(`not_found`·`declined`·`peer_limits_invalid`·`nat_unsupported`·`busy`·`self`) | 요청 거절 |
| `punch_offer` | 서버 → 기기 | `punch_id`, `from` | 받는 쪽에게 요청을 알림 |
| `punch_answer` | 기기 → 서버 | `punch_id`, `accept`, `reason`(거절일 때 `not_paired`·`limits_invalid`) | 받는 쪽의 답. `not_paired`는 A에게 `declined`, `limits_invalid`는 `peer_limits_invalid`로 간다 |
| `punch_go` | 서버 → 기기 | `punch_id`, `request_id`(A에게만), `peer`, `role`(`controlling`·`controlled`), `mode`(`all`·`host_only`), `candidates`(주소와 종류 `server_reflexive`·`reported_reflexive`·`host`), `peer_nat`(상대 `NatMapping`), `token_local`, `token_peer`(각 16바이트 hex), `start_after_ms`, `window_ms`, `handshake_ms` | 펀칭 시작 |
| `punch_cancel` | 서버 → 기기 | `punch_id`, `reason`(`peer_gone`·`expired`·`superseded`·`peer_limits_invalid`) | 진행 중 펀칭 취소. `superseded`는 같은 A가 새 요청으로 옛 세션을 바꿨을 때 B에게 간다 |
| `punch_result` | 기기 → 서버 | `punch_id`, `ok`, `reason`(실패일 때 `limits_invalid`·`timeout`·`handshake_failed`) | 끝 보고. `limits_invalid`면 서버는 상대에게 `punch_cancel{peer_limits_invalid}`를 보낸다 |

- `welcome.limits`: 기기가 자기 설정과의 관계를 검사할 서버 값이다. `offer_timeout_ms`, `start_delay_max_ms`, `punch_window_ms`, `quic_handshake_ms`, `punch_session_ttl_ms`, `max_host_candidates`, `max_reported_reflexive`, `max_message_bytes`. 기기는 이 값으로 `punch.request_timeout > offer_timeout_ms`, `punch_window_ms ≤ punch.window_max`, `quic_handshake_ms ≤ quic.handshake_timeout_max`, `1 + max_reported_reflexive + max_host_candidates ≤ punch.max_candidates`([후보 주소 규칙](#후보-주소-규칙))를 검사하고, 어긋나면 endpoint 이벤트 `server_limits_mismatch`(어긋난 키와 두 값)를 내고 그 서버로의 펀칭 시도를 `ServerLimitsInvalid`로 끝낸다([P2P](p2p.md#재시도와-포기)).
- 한계값 검사는 두 단계이고 A와 B가 똑같이 한다.
  1. 등록 때(`welcome.limits`): 위 관계가 어긋나면 기기는 `server_limits_mismatch`를 낸다. 등록은 남는다(상대의 요청을 받아 거절 사유를 돌려줘야 하기 때문이다). A는 그 서버로 `punch_request`를 보내지 않고 바로 `ServerLimitsInvalid`로 끝낸다. B는 `punch_offer`를 받으면 `punch_answer{accept: false, reason: limits_invalid}`로 답하고, 서버는 A에게 `punch_refused{peer_limits_invalid}`를 보낸다. A의 공개 오류는 `PeerLimitsInvalid`다.
  2. 펀칭 때(`punch_go`): 실린 값이 `welcome.limits`와 기기 상한을 벗어나면(`window_ms > punch.window_max`, `handshake_ms > quic.handshake_timeout_max`, `start_after_ms > start_delay_max_ms`, 후보가 상한 초과) 그 기기는 프로브를 보내지 않고 `punch_result{ok: false, reason: limits_invalid}`를 보낸다. 자기 쪽 결과는 A면 공개 오류 `ServerLimitsInvalid`, B면 `Closed{server_limits_invalid}`다. 서버는 상대에게 `punch_cancel{peer_limits_invalid}`를 보낸다. 받는 쪽은 `Scheduled`·`Punching`·`Handshaking` 어느 단계에서 받아도 재시도 없이 `Closed`로 끝나고, A면 공개 오류는 `PeerLimitsInvalid`, B면 `Closed{peer_limits_invalid}`다.
- 한 쌍에 세션 하나: 서버는 같은 두 기기(방향과 관계없이) 사이의 진행 중 펀칭을 하나만 둔다. 옛 세션을 연 A가 같은 B에게 새 `punch_request`를 보내면(펀칭 재시도), 서버는 옛 세션을 끝내고 B에게 `punch_cancel{superseded}`를 보낸 뒤 새 세션을 만든다. 반대 방향(B가 A에게) 요청이 진행 중 세션과 겹치면 `punch_refused{busy}`다. 이 규칙은 기기 ID 쌍에 대한 것이다. B가 들어온 Initial을 세션에 묶는 일은 주소와 기기 ID의 두 단계로 한다([P2P](p2p.md#펀칭-절차) 9).
- `punch_go.window_ms`·`handshake_ms`가 펀칭 창과 QUIC handshake 기한의 정본이다. 두 기기가 같은 값을 받으므로 양쪽 기한이 맞는다. 값은 서버 설정 `punch_window`·`quic_handshake_timeout`에서 온다.
- `mode = host_only`는 두 기기가 모두 `dependent`일 때다. 서버는 반사 주소 후보를 빼고 호스트 주소만 싣는다. 어느 한쪽에도 호스트 후보가 없으면 `punch_go` 대신 `punch_refused{nat_unsupported}`를 보낸다([P2P](p2p.md#nat-판별) "시도 규칙").

조정 연결의 QUIC 종료 코드와 TLS 단계 실패:

| 코드 | 이름 | 뜻 | 기기의 다음 동작 |
|---|---|---|---|
| 0 | `closed` | 보통 종료(서버 종료, 기기가 P2P를 끔) | 서버 종료면 backoff로 재연결 |
| 1 | `access_denied` | 접근 정책 거절 | 멈춤(`Stopped`) |
| 2 | `superseded` | 같은 기기 ID가 새로 등록함 | 멈춤 |
| 3 | `unsupported_proto` | 같은 ALPN에서 `hello.proto`를 서버가 모름 | 멈춤 |
| 4 | `malformed` | 형식 오류, 상한 초과, 프레임 기한 초과, 둘째 스트림 | backoff로 재연결 |
| 5 | `busy` | `max_devices` 초과 | backoff로 재연결 |
| 6 | `hello_timeout` | `hello`가 시간 안에 오지 않음 | backoff로 재연결 |
| (TLS) | ALPN 불일치 | 서버가 `tasty-rendezvous/<n>`을 모름. QUIC/TLS handshake가 `no_application_protocol` alert로 끝나 앱 종료 코드까지 가지 않는다([RFC 9001](https://www.rfc-editor.org/rfc/rfc9001) §8.1) | 멈춤(원인 `alpn_mismatch`) |
| (TLS) | 서버 키 불일치 | 서버 raw public key가 설정한 공개키와 다름 | 멈춤(원인 `server_key_mismatch`) |

### 펀칭 프로브

- 기기 사이 프로브는 STUN Binding Request/Success Response에 속성 `TASTY-PUNCH`(타입 `0xC0A1`)를 더한 것이다. 값은 보내는 쪽 토큰 16바이트다.
- 속성 번호: 0x8000–0xFFFF는 comprehension-optional이고, 그중 0xC000–0xFFFF는 Expert Review로 등록하는 범위다([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §18.3). `0xC0A1`은 등록하지 않은 번호다. 이 속성은 이 레포의 기기끼리만 오가고 받는 쪽이 토큰으로 거르므로, 다른 구현과 번호가 겹쳐도 그 패킷은 토큰 불일치로 버려진다. IANA 등록은 하지 않는다(외부 구현과 상호운용할 일이 없다). 등록이 필요해지면 번호를 바꾸고 조정 프로토콜 번호를 올린다([버전·호환](versioning.md)).
- 형식의 정본은 이 크레이트의 와이어 형식 모듈이고, 서버와 `tasty-net-p2p`가 같은 코드를 쓴다.
- 서버는 프로브를 받지 않는다(서버 STUN은 `TASTY-PUNCH`가 있는 요청을 버린다).

## 서버 상태

모든 상태는 메모리에만 있다. 디스크에 쓰지 않고, 서버가 다시 시작하면 비어 있다. 기기는 재연결로 다시 등록한다.

| 상태 | 담는 것 | 지우는 때 |
|---|---|---|
| 등록 | 기기 ID, 조정 연결, 반사 주소(조정 연결의 원격 주소), 보고된 보조 반사 주소, 호스트 후보, `NatMapping`, 등록 시각, 마지막 수신 시각(keepalive 포함) | 연결이 닫힘, idle timeout(`idle_timeout`), 같은 기기 ID의 새 등록(옛 연결을 `superseded`로 닫는다), 서버 종료 |
| 진행 중 펀칭 | `punch_id`, A·B 기기 ID, 상태(`offered`·`go_sent`), 두 토큰, 만든 시각, `punch_go`를 보낸 시각, 만료 시각, 받은 `punch_result` | 두 기기의 `punch_result`를 모두 받음, `punch_refused`, 같은 A의 새 요청(B에게 `punch_cancel{superseded}`), `offer_timeout`, 아래 TTL 만료(남은 쪽에 `punch_cancel{expired}`), `limits_invalid` 보고(상대에게 `punch_cancel{peer_limits_invalid}`), A나 B의 등록이 지워짐(남은 쪽에 `punch_cancel{peer_gone}`) |

- 세션 TTL: 두 단계로 잰다.
  - `offered`: `punch_request`를 받은 때부터 `offer_timeout`. 지나면 A에게 `not_found`.
  - `go_sent`: 서버가 `punch_go`를 보낸 때(두 기기에 같은 때 보낸다)부터 `start_delay_max + punch_window + 2 × quic_handshake_timeout + punch_session_grace`. 기본 1 + 5 + 10 + 5 = 21 s다. 앞의 세 항은 B의 기한 전체다: 시작 지연 뒤 창 동안 프로브, 그 뒤 `handshake_ms`까지 새 Initial을 받고, 마감 직전에 받은 handshake에 다시 `handshake_ms`([P2P](p2p.md#펀칭-절차) 10). `punch_session_grace`는 서버에서 각 기기까지의 한쪽 통지 지연(기기의 기한은 `punch_go`를 받은 때부터 재므로 늦게 받은 기기는 그만큼 늦게 끝난다)과 `punch_result`가 서버로 오는 시간을 덮는다. 그래서 통지 지연이 grace보다 짧으면 기기가 자기 기한 안에 `punch_cancel{expired}`를 받지 않는다. 받더라도 QUIC handshake 중이면 무시한다.
  - TTL은 계산값이고 `welcome.limits.punch_session_ttl_ms`(`go_sent` 단계 값)와 조회에 보인다.
- 설정 관계 검사: 서버는 시작할 때 `start_delay_margin ≤ start_delay_max`, `punch_session_grace ≥ 1 s`, `offer_timeout ≥ 100 ms`, `frame_timeout < idle_timeout`, `keepalive_interval < idle_timeout`, `1 + max_reported_reflexive + max_host_candidates ≤ 64`를 본다. 값의 범위도 본다: 시간 값은 모두 2^32 − 1 s 이하이고 현재 시각에 더해 시계 범위 안이어야 하며(세션 TTL 합도 같다), `quic.max_bidi_streams`는 1 이상 2^60 이하(조정 스트림 하나가 열려야 한다), `quic.max_uni_streams`는 2^60 이하다([RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §4.6). 어긋나면 어긋난 키와 값을 찍고 시작하지 않는다. 실행 중 기한은 검사한 덧셈으로 만든다.
- 반사 주소 바뀜: 서버는 `reflexive_poll_interval`마다 조정 연결의 원격 주소(quinn `Connection::remote_address`)를 읽고, 바뀌었으면 등록을 고치고 `reflexive_changed`를 보낸다. 기기가 QUIC 클라이언트라 이 연결에서는 기기 쪽 NAT 재매핑이 QUIC 경로 이동으로 이어진다([RFC 9000](https://www.rfc-editor.org/rfc/rfc9000) §9). 폴링 간격 안에 바뀌었다 돌아온 경우는 놓친다.
- STUN 반사는 상태를 남기지 않는다(출발 주소별 속도 상한의 계수만 둔다).
- 서버가 절대 갖지 않는 것: 기기 사이 데이터(중계 경로가 없다), 연결 토큰 원문(SSH 경로의 토큰이며 P2P와 서버에는 보내지 않는다, [기기 키·토큰](identity.md)), 페어링 목록, 기기 개인키, handshake v2 내용.
- 로그: 기본 수준에서 기기 ID와 사건 종류(등록, 펀칭 요청·결과, 종료 코드)를 남기고 IP 주소와 후보 주소는 남기지 않는다. 주소는 디버그 수준에서만 찍는다.

## 서버 설정값

[크레이트 구성](crates.md#설정값-규칙)의 규칙을 따른다. 모든 시간·횟수·상한은 설정 파일로 바꿀 수 있다.

- 실행 중 조회: 떠 있는 서버는 시작할 때 전체 설정(항목마다 값과 출처 `default`·`configured`)을 로그에 한 번 찍는다. 서버를 라이브러리로 띄운 쪽은 `Server::settings()`로 같은 값을 읽는다. 실행 중 상태(등록 수, 진행 중 펀칭 수, 상한에 걸린 횟수)는 `Server::status()`와 주기 로그(`status_log_interval`)로 본다. 원격 조회 엔드포인트는 두지 않는다(공격 면을 늘리지 않는다).
- `tasty-rendezvous settings --config <파일>`은 그 파일을 읽었을 때의 값과 출처를 찍는다(떠 있는 프로세스의 값이 아니라 파일 기준이다. 배포 전 확인용).
- 기기가 지켜야 할 시간 값은 `welcome.limits`로 기기에게 알린다(위 "메시지 표").

| 키 | 기본 | 근거 |
|---|---|---|
| `listen.primary` | `0.0.0.0:3478` | STUN 기본 포트 |
| `listen.alternate_port` | 3479 | NAT 판별의 보조 포트 |
| `listen.alternate_ip` | 없음 | 있으면 주소 의존까지 판별 |
| `advertise.primary_ip` | 없음 | 주 주소를 wildcard로 들을 때 보조 주소의 IP로 광고할 값 |
| `advertise.alternate_port` | `listen.alternate_port`와 같음 | 포트 게시가 다를 때(컨테이너) |
| `advertise.alternate_ip` | 없음 | 두 번째 공인 IP |
| `hello_timeout` | 5 s | 기기 쪽 `rendezvous.hello_timeout`과 같다 |
| `frame_timeout` | 5 s | 16 KiB 프레임 하나는 느린 망에서도 1초 안에 온다. 조금씩 보내 연결을 붙잡는 것을 끊는다 |
| `keepalive_interval` | 15 s | 기기 쪽과 같은 근거([P2P](p2p.md#연결-유지)) |
| `idle_timeout` | 45 s | 같다 |
| `reflexive_poll_interval` | 1 s | 반사 주소 바뀜을 1초 안에 기기에 알림 |
| `offer_timeout` | 2 s | 등록된 B의 답은 서버 왕복 한 번이면 된다. 기기 쪽 `punch.request_timeout`(3 s)보다 짧아야 한다 |
| `start_delay_margin` | 100 ms | RTT 추정 오차를 덮는 여유 |
| `start_delay_max` | 1 s | 시작 지연의 상한 |
| `punch_window` | 5 s(임시) | 후보 최대 8개(1 + 3 + 4)를 50 ms씩 돌면 한 바퀴 400 ms라 후보마다 12번 넘게 보낸다. [펀칭 실험](p2p.md#펀칭-실험)으로 확정 |
| `quic_handshake_timeout` | 5 s(임시) | 직접 경로 RTT 몇 번. 실험으로 확정 |
| `punch_session_grace` | 5 s | 한쪽 통지 지연과 결과 보고 시간을 덮는 여유. 느린 망의 한쪽 지연(수백 ms)에 손실 재전송 몇 번을 더해도 남는다. 세션 TTL 식은 위 "서버 상태"(기본 21 s) |
| `max_devices` | 10000 | 등록 하나는 연결 상태 수 KB다. 작은 서버 한 대가 감당할 수 있는 수 |
| `max_punches` | 1000 | 동시 펀칭 전체 상한 |
| `max_punches_per_device` | 4 | 한 기기가 다른 기기들에게 거는 동시 시도 |
| `punch_requests_per_minute` | 20 | 기기마다. 남의 주소를 훑는 요청을 늦춘다 |
| `max_host_candidates` | 4 | 사설 IPv4 하나둘과 전역 IPv6 고정·임시 주소면 보통의 기기를 덮는다. 기기 기본 `punch.max_candidates`(8)에서 서버 반사 1과 보고 반사 3을 뺀 값이다([후보 주소 규칙](#후보-주소-규칙)의 관계) |
| `max_reported_reflexive` | 3 | 보조 STUN 주소 수(보조 포트, 두 번째 IP의 두 포트) |
| `max_message_bytes` | 16 KiB | 가장 큰 메시지(`punch_go`)의 몇 배 |
| `stun_per_source_per_second` | 50 | 출발 주소마다. 판별 한 번은 많아야 12개(대상 4 × 3번) |
| `status_log_interval` | 60 s | 운영자가 상태를 보는 주기 |
| `quic.max_bidi_streams` | 2 | 기기가 여는 조정 스트림 하나와, 둘째 스트림을 받아 `malformed`로 닫을 자리 하나. 1이면 둘째 스트림이 열리지 못하고 기기 쪽에서 막혀 서버가 규칙 위반을 볼 수 없다(quinn `max_concurrent_bidi_streams`) |
| `quic.max_uni_streams` | 0 | 단방향 스트림을 쓰지 않는다(quinn `max_concurrent_uni_streams`) |

quinn·rustls의 값(QUIC 손실 복구 등)은 기기 쪽과 같은 항목을 서버 설정에도 둔다. 스트림 수 상한은 기기 쪽 "이 레포가 정하는 값" 표의 항목이라 서버 값을 위 표에 따로 둔다. 항목과 기본값, 고정값(세션 재개·0-RTT·티켓 끔, grease 끔 등)과 잘못된 설정의 거절 규칙은 [P2P](p2p.md#설정값)의 "의존성이 노출하는 값"·"고정값" 표를 따른다. 서버가 기기에게 알리는 값(`welcome.limits`, `punch_go`)은 기기 쪽 연결 정보에 출처 `server`로 보인다.

## 클라이언트 설정

| 질문 | 결정 |
|---|---|
| 서버를 누가 주나 | 소비자(tasty) 설정. 주소(`host:port`)와 서버 공개키 한 쌍이다. 이 레포에는 기본 서버 상수를 두지 않는다. 라이브러리를 다른 곳에서 쓸 때 모르게 hub에 붙지 않게 하고, hub 주소가 바뀌어도 이 레포 릴리스가 필요 없게 한다 |
| tasty의 기본 | hub rendezvous |
| 사용자가 자기 서버를 지정하면 | 그 서버 하나만 쓴다. 기본 서버에는 붙지 않는다. 서버는 하나만 둔다. 같은 서버의 기기끼리만 연결되므로 둘을 동시에 둘 까닭이 없다([P2P](p2p.md#정책)) |
| P2P를 켜지 않았으면 | 서버에 연결하지 않는다 |
| 켰는데 서버가 없으면 | `P2pEndpoint::bind`가 오류다 |

## 배포 형태

- 이 레포에 둘 것: `rendezvous/`(지금의 `relay/`를 바꾼다). `Dockerfile`(이 레포 소스를 태그 기준으로 빌드하는 다단계 이미지, 비root 사용자), `rendezvous.toml`(설정 예시, `advertise.*` 포함), `compose.yaml`(UDP 3478·3479 게시), `check-config.sh`(설정 검사와 STUN 응답 확인), `container-check.sh`(이미지로 띄운 서버에 같은 망의 기기 두 대를 붙이는 확인, D5 (b)), `README.md`(자체 운영 안내: 서버 키 만들기, 공개키 배포, 방화벽, 광고 주소, 두 번째 IP).
- 외부 이미지를 쓰지 않는다. 바이너리는 이 workspace의 `tasty-rendezvous`이고, 판은 workspace 버전을 따른다([버전·호환](versioning.md)).
- 서버 키 파일은 설정한 경로에 둔다. 파일 권한은 기기 키와 같은 규칙을 쓴다([기기 키·토큰](identity.md)).
- 컨테이너에서는 듣는 주소가 컨테이너 안 주소이므로 `advertise.*`를 반드시 준다(위 "광고 주소와 바인딩 주소").
- hub rendezvous는 hub가 이 패키지(또는 라이브러리)를 가져다 운영한다. hub 쪽 배포 방식은 이 레포에 적지 않는다.

### `relay/` 패키지와 `relay.yml`의 처리 방침

develop에는 iroh-relay용 `relay/`(설정 예시 두 개, compose, `check-config.sh`, `container-check.sh`, 운영 안내)와 CI workflow `.github/workflows/relay.yml`(`relay-config-check` 잡)이 들어가 있다. 이번 작업은 문서만 고치고 그 파일은 그대로 둔다.

- 방침: 둘 다 없앤다. 내용이 iroh-relay 설정·이미지 고정·metrics에 묶여 있어 새 서버에 다시 쓸 부분이 없다. 대신 `rendezvous/` 패키지와 workflow `rendezvous.yml`(잡 `rendezvous-check`)을 새로 만든다.
- 순서: rendezvous 서버 크레이트가 develop에 들어간 뒤 한 작업에서 `relay/`·`relay.yml` 제거와 `rendezvous/`·`rendezvous.yml` 추가를 함께 한다. 그 사이에는 `relay.yml`이 계속 돌아도 된다(이 레포 코드를 쓰지 않는다).
- 같은 작업이 함께 고칠 것: `docs/dev-guide/ci.md`의 `relay.yml` 서술, [동작 수준 기준](done-criteria.md) D5의 잡 이름, 레포 `CLAUDE.md`의 relay 서버 패키지 서술, 그리고 `relay/README.md`의 링크 때문에 남겨 둔 안내 문서 [relay.md](relay.md)의 삭제.

## 위협 범위

서버가 보는 것과 보지 못하는 것은 [위협 모델](threat-model.md) T5, 주소 수집·반사 남용은 T10에 있다.
