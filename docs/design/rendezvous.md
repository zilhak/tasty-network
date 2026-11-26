# rendezvous 서버

크레이트는 `tasty-net-rendezvous`(라이브러리 + 실행 파일 `tasty-rendezvous`)다. 이 서버는 두 가지만 한다(사용자 결정 2026-11-27).

1. 반사: 기기에게 바깥에서 보이는 주소를 알려 준다(STUN Binding).
2. 조정: 같은 서버에 붙은 두 기기의 동시 펀칭을 조정한다.

기기 쪽 절차(NAT 판별, 펀칭, 정책, 상태 기계)는 [P2P](p2p.md)에 있다. 이 문서는 서버 프로토콜, 서버 상태, 배포를 다룬다. hub가 운영하는 rendezvous 서버를 이 문서에서 hub rendezvous라고 부른다.

## 중계 없음

서버는 데이터를 중계하지 않는다. 이것을 설정이나 상한이 아니라 구조로 보장한다.

- 서버에는 한 기기에서 받은 바이트를 다른 기기에게 보내는 코드 경로가 없다. 서버가 기기에게 보내는 것은 서버가 자기 상태로 만든 정해진 형식의 메시지(아래 표)와 STUN 응답뿐이다. 임의 바이트를 담는 메시지 종류가 없다.
- 기기가 알린 값 중 상대에게 넘어가는 것은 호스트 후보 주소뿐이다. 서버는 그것을 IP 주소와 포트로 파싱한 값만 넘기고(개수 상한 있음), 받은 문자열을 그대로 넘기지 않는다.
- STUN 응답은 요청이 온 주소로만 보낸다. 다른 주소로 보내라는 요청(RFC 5780의 RESPONSE-PORT·CHANGE-REQUEST)은 받지 않는다. 응답 크기는 요청과 비슷해 증폭이 없다.
- 기기 사이 QUIC 연결과 펀칭 프로브는 서버를 지나지 않는다([P2P](p2p.md#흐름)의 8~13).
- 이 성질은 시험(`server_never_forwards_device_bytes`, [CI와 시험 환경](testing.md#p2p-시험))과 리뷰 규칙(서버 송신 함수는 메시지 enum과 STUN 응답 두 가지만 받는다)으로 지킨다.

iroh 설계 단계의 relay 모드(`punch-only`·`relay-fallback`), 수신 상한, `enable_relay`, 접속 허용 콜백(`access.http.url`)은 이 구조에 해당하는 것이 없어 없앤다.

## 포트와 전송

모두 UDP다. TCP는 쓰지 않는다. P2P 자체가 UDP를 요구하므로 UDP가 막힌 망에서 서버만 TCP로 닿아도 쓸모가 없다.

| 주소 | 기본 포트 | 받는 것 |
|---|---|---|
| IP1:P1(주) | 3478 | QUIC(조정 연결) + STUN Binding. 첫 바이트로 나눈다(0–3 STUN, 64–191 QUIC, [RFC 7983](https://www.rfc-editor.org/rfc/rfc7983)·[RFC 9443](https://www.rfc-editor.org/rfc/rfc9443)) |
| IP1:P2(보조) | 3479 | STUN Binding만 |
| IP2:P1(선택) | 3478 | STUN Binding만. 두 번째 공인 IP가 있을 때 |

- 3478은 STUN 기본 포트다([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489)). 포트는 서버 설정으로 바꿀 수 있고, 클라이언트는 `welcome`에 실린 주소를 쓰므로 P2·IP2를 따로 설정하지 않는다.
- 반사와 조정을 같은 주 포트에 두는 근거: 기기는 P2P 소켓 하나로 서버에 붙는다([P2P](p2p.md#소켓-하나와-패킷-나누기)). 서버가 조정 연결의 원격 주소로 보는 값이 곧 그 소켓의 반사 주소가 되고, 조정 연결의 keepalive가 같은 NAT 매핑을 살린다.
- NAT 판별에 보조 포트가 필요하다(같은 IP의 두 포트로 APDM을 가른다). ADM까지 가르려면 두 번째 IP가 필요하다([P2P](p2p.md#nat-판별)).
- STUN은 Binding Request/Success Response와 XOR-MAPPED-ADDRESS만 쓰는 부분 집합이다([RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §5·§14.2). 서버의 STUN 응답에는 인증 속성이 없다. 반사 주소는 공개돼도 되는 값이고, 조정 연결이 같은 값을 TLS 위에서 다시 알려 준다.

## 기기 인증

- 조정 연결은 QUIC(ALPN `tasty-rendezvous/1`) 위의 상호 인증 TLS 1.3이다. 양쪽 모두 RFC 7250 raw public key를 쓴다([P2P](p2p.md#인증)).
- 서버가 기기를 확인하는 방법: 기기의 raw public key(기기 키 ed25519 공개키)가 곧 기기 ID다. TLS CertificateVerify가 개인키 소유를 증명하므로 서명 단계를 따로 두지 않는다. 기기는 메시지에 자기 기기 ID를 싣지 않는다. 그래서 다른 기기 ID로 등록할 길이 없다.
- 기기가 서버를 확인하는 방법: 서버도 ed25519 서버 키를 가진다. 기기는 설정으로 받은 서버 공개키와 TLS의 raw public key를 비교한다(키 고정). 공용 인증 기관과 도메인 인증서는 쓰지 않는다. 서버 키는 `tasty-rendezvous keygen`으로 만들고, 시작할 때 공개키를 로그 첫 줄에 찍는다.
- 접속 허용: TLS가 끝난 직후 서버가 접근 정책으로 기기 ID를 본다. 라이브러리는 정책 trait `AccessPolicy`를 받는다. 실행 파일은 `everyone`(기본)과 `allowlist`(기기 ID 목록 파일) 둘을 준다. hub rendezvous가 누구를 받을지(로그인 사용자, 페어링된 기기 등)는 hub 쪽 결정이다([남은 결정](index.md#남은-결정)). 거절은 종료 코드 `access_denied`다.
- 서버가 기기를 찾는 키는 기기 ID(소문자 hex 64자, [기기 키·토큰](identity.md))다. `punch_request.peer`가 이 값이다.

## 메시지

- 기기가 연결을 연 뒤 양방향 스트림 하나(조정 스트림)를 연다. 연결마다 하나다.
- 프레임: 4바이트 big-endian 길이 + JSON 객체 하나. 길이 상한은 서버 설정 `max_message_bytes`(기본 16 KiB)다. 넘으면 종료 코드 `malformed`.
- 최상위 키 `type`으로 메시지를 가른다. 모르는 `type`은 `malformed`, 모르는 필드는 무시한다.
- 주소는 `"203.0.113.5:3478"`·`"[2001:db8::1]:3478"` 형식 문자열이다.

| `type` | 방향 | 필드 | 뜻 |
|---|---|---|---|
| `hello` | 기기 → 서버 | `proto`(지금 1), `host_candidates`(주소 목록) | 등록. 접속 뒤 `hello_timeout` 안에 와야 한다 |
| `welcome` | 서버 → 기기 | `reflexive`(서버가 본 원격 주소), `stun`(STUN을 받는 서버 주소 목록: 주·보조·두 번째 IP), `limits`(기기가 알아야 할 서버 상한: `max_host_candidates`, `max_message_bytes`) | 등록됨 |
| `nat_report` | 기기 → 서버 | `mapping`(`independent`·`dependent`·`unknown`), `host_candidates` | NAT 판별 결과와 바뀐 호스트 주소 |
| `punch_request` | 기기 → 서버 | `request_id`, `peer` | 펀칭 요청 |
| `punch_refused` | 서버 → 기기 | `request_id`, `reason`(`not_found`·`declined`·`nat_unsupported`·`busy`·`self`) | 요청 거절 |
| `punch_offer` | 서버 → 기기 | `punch_id`, `from` | 받는 쪽에게 요청을 알림 |
| `punch_answer` | 기기 → 서버 | `punch_id`, `accept` | 받는 쪽의 답 |
| `punch_go` | 서버 → 기기 | `punch_id`, `request_id`(A에게만), `peer`, `role`(`controlling`·`controlled`), `candidates`, `token_local`, `token_peer`(각 16바이트 hex), `start_after_ms` | 펀칭 시작 |
| `punch_cancel` | 서버 → 기기 | `punch_id`, `reason`(`peer_gone`·`expired`) | 진행 중 펀칭 취소 |
| `punch_result` | 기기 → 서버 | `punch_id`, `ok` | 끝 보고. 서버는 상태를 지운다 |

조정 연결의 QUIC 종료 코드:

| 코드 | 이름 | 뜻 | 기기의 다음 동작 |
|---|---|---|---|
| 0 | `closed` | 보통 종료(서버 종료, 기기가 P2P를 끔) | backoff로 재연결(서버 종료일 때) |
| 1 | `access_denied` | 접근 정책 거절 | 멈춤(`Stopped`) |
| 2 | `superseded` | 같은 기기 ID가 새로 등록함 | 멈춤 |
| 3 | `unsupported_proto` | `hello.proto`를 서버가 모름 | 멈춤 |
| 4 | `malformed` | 형식 오류, 상한 초과 | backoff로 재연결 |
| 5 | `busy` | `max_devices` 초과 | backoff로 재연결 |
| 6 | `hello_timeout` | `hello`가 시간 안에 오지 않음 | backoff로 재연결 |

### 펀칭 프로브

- 기기 사이 프로브는 STUN Binding Request/Success Response에 속성 `TASTY-PUNCH`(타입 `0xC0A1`, comprehension-optional 범위 0x8000–0xFFFF 중 사설 범위, [RFC 8489](https://www.rfc-editor.org/rfc/rfc8489) §18.3)를 더한 것이다. 값은 보내는 쪽 토큰 16바이트다.
- 형식의 정본은 이 크레이트의 형식 모듈이고, 서버와 `tasty-net-p2p`가 같은 코드를 쓴다.
- 서버는 프로브를 받지 않는다(서버 STUN은 `TASTY-PUNCH`가 있는 요청을 버린다).

## 서버 상태

모든 상태는 메모리에만 있다. 디스크에 쓰지 않고, 서버가 다시 시작하면 비어 있다. 기기는 재연결로 다시 등록한다.

| 상태 | 담는 것 | 지우는 때 |
|---|---|---|
| 등록 | 기기 ID, 조정 연결, 반사 주소(조정 연결의 원격 주소), 호스트 후보, `NatMapping`, 등록 시각, 마지막 수신 시각(keepalive 포함) | 연결이 닫힘, idle timeout(`idle_timeout`), 같은 기기 ID의 새 등록(옛 연결을 `superseded`로 닫는다), 서버 종료 |
| 진행 중 펀칭 | `punch_id`, A·B 기기 ID, 상태(`offered`·`go_sent`), 두 토큰, 만든 시각, 만료 시각 | 양쪽 `punch_result`, `punch_refused`, `offer_timeout`, `punch_session_ttl` 만료, A나 B의 등록이 지워짐(남은 쪽에 `punch_cancel{peer_gone}`) |

- STUN 반사는 상태를 남기지 않는다.
- 서버가 절대 갖지 않는 것: 기기 사이 데이터(중계 경로가 없다), 연결 토큰 원문(SSH 경로의 토큰이며 P2P와 서버에는 보내지 않는다, [기기 키·토큰](identity.md)), 페어링 목록, 기기 개인키, handshake v2 내용.
- 로그: 기본 수준에서 기기 ID와 사건 종류(등록, 펀칭 요청·결과, 종료 코드)를 남기고 IP 주소와 후보 주소는 남기지 않는다. 주소는 디버그 수준에서만 찍는다.

## 서버 설정값

[P2P](p2p.md#설정값)와 같은 규칙이다. 모든 시간·횟수·상한은 설정 파일로 바꿀 수 있고, `tasty-rendezvous settings --config <파일>`과 라이브러리 `Server::settings()`가 항목마다 값과 출처(`default`·`configured`)를 보인다. 실행 중 상태(등록 수, 진행 중 펀칭 수, 상한에 걸린 횟수)는 `Server::status()`와 주기 로그(`status_log_interval`)로 본다.

| 키 | 기본 | 근거 |
|---|---|---|
| `listen.primary` | `0.0.0.0:3478` | STUN 기본 포트 |
| `listen.alternate_port` | 3479 | NAT 판별의 보조 포트 |
| `listen.alternate_ip` | 없음 | 있으면 ADM까지 판별 |
| `hello_timeout` | 5 s | 기기 쪽 `rendezvous.hello_timeout`과 같다 |
| `keepalive_interval` | 15 s | 기기 쪽과 같은 근거([P2P](p2p.md#연결-유지)) |
| `idle_timeout` | 45 s | 같다 |
| `offer_timeout` | 2 s | 등록된 B의 답은 서버 왕복 한 번이면 된다. 기기 쪽 `punch.request_timeout`(3 s)보다 짧아야 한다 |
| `start_delay_margin` | 100 ms | RTT 추정 오차를 덮는 여유 |
| `start_delay_max` | 1 s | 시작 지연의 상한 |
| `punch_session_ttl` | 20 s | `offer_timeout` + 시작 지연 + 기기 `punch.window`(5 s) + `quic.handshake_timeout`(5 s)에 여유를 더한 값. 결과 보고가 오지 않은 펀칭을 치운다 |
| `max_devices` | 10000 | 등록 하나는 연결 상태 수 KB다. 작은 서버 한 대가 감당할 수 있는 수 |
| `max_punches` | 1000 | 동시 펀칭 전체 상한 |
| `max_punches_per_device` | 4 | 한 기기가 다른 기기들에게 거는 동시 시도 |
| `punch_requests_per_minute` | 20 | 기기마다. 남의 주소를 훑는 요청을 늦춘다 |
| `max_host_candidates` | 6 | 기기 쪽 `punch.max_candidates`(8)에서 반사 2를 뺀 값 |
| `max_message_bytes` | 16 KiB | 가장 큰 메시지(`punch_go`)의 몇 배 |
| `stun_per_source_per_second` | 50 | 출발 주소마다. 판별 한 번은 많아야 9개 |
| `status_log_interval` | 60 s | 운영자가 상태를 보는 주기 |

## 클라이언트 설정

| 질문 | 결정 |
|---|---|
| 서버를 누가 주나 | 소비자(tasty) 설정. 주소(`host:port`)와 서버 공개키 한 쌍이다. 이 레포에는 기본 서버 상수를 두지 않는다. 라이브러리를 다른 곳에서 쓸 때 모르게 hub에 붙지 않게 하고, hub 주소가 바뀌어도 이 레포 릴리스가 필요 없게 한다 |
| tasty의 기본 | hub rendezvous |
| 사용자가 자기 서버를 지정하면 | 그 서버 하나만 쓴다. 기본 서버에는 붙지 않는다. 서버는 하나만 둔다. 같은 서버의 기기끼리만 연결되므로 둘을 동시에 둘 까닭이 없다([P2P](p2p.md#정책)) |
| P2P를 켜지 않았으면 | 서버에 연결하지 않는다 |
| 켰는데 서버가 없으면 | `P2pEndpoint::bind`가 오류다 |

## 배포 형태

- 이 레포에 둘 것: `rendezvous/`(지금의 `relay/`를 바꾼다). `Dockerfile`(이 레포 소스를 태그 기준으로 빌드하는 다단계 이미지, 비root 사용자), `rendezvous.toml`(설정 예시), `compose.yaml`(UDP 3478·3479 게시), `check-config.sh`(설정 검사와 STUN 응답 확인), `README.md`(자체 운영 안내: 서버 키 만들기, 공개키 배포, 방화벽, 두 번째 IP).
- 외부 이미지를 쓰지 않는다. 바이너리는 이 workspace의 `tasty-rendezvous`이고, 판은 workspace 버전을 따른다([버전·호환](versioning.md)).
- 서버 키 파일은 설정한 경로에 둔다. 파일 권한은 기기 키와 같은 규칙을 쓴다([기기 키·토큰](identity.md)).
- hub rendezvous는 hub가 이 패키지(또는 라이브러리)를 가져다 운영한다. hub 쪽 배포 방식은 이 레포에 적지 않는다.

### `relay/` 패키지와 `relay.yml`의 처리 방침

develop에는 iroh-relay용 `relay/`(설정 예시 두 개, compose, `check-config.sh`, `container-check.sh`, 운영 안내)와 CI workflow `.github/workflows/relay.yml`(`relay-config-check` 잡)이 들어가 있다. 이번 작업은 문서만 고치고 그 파일은 그대로 둔다.

- 방침: 둘 다 없앤다. 내용이 iroh-relay 설정·이미지 고정·metrics에 묶여 있어 새 서버에 다시 쓸 부분이 없다. 대신 `rendezvous/` 패키지와 workflow `rendezvous.yml`(잡 `rendezvous-check`)을 새로 만든다.
- 순서: rendezvous 서버 크레이트가 develop에 들어간 뒤 한 작업에서 `relay/`·`relay.yml` 제거와 `rendezvous/`·`rendezvous.yml` 추가를 함께 한다. 그 사이에는 `relay.yml`이 계속 돌아도 된다(이 레포 코드를 쓰지 않는다).
- 같은 작업이 `docs/dev-guide/ci.md`의 `relay.yml` 서술과 [동작 수준 기준](done-criteria.md) D5의 잡 이름을 고친다.

## 위협 범위

서버가 보는 것과 보지 못하는 것은 [위협 모델](threat-model.md) T5에 있다.
