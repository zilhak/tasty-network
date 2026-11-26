# P2P 전송 (iroh)

크레이트는 `tasty-net-p2p`다. iroh는 `=1.5.0`으로 고정한다([버전·호환](versioning.md)). relay 배포와 relay 모드는 [relay](relay.md)에 있다.

## 쓰는 iroh 1.5.0 API

아래 목록은 계획 단계에서 docs.rs와 v1.5.0 소스로 확인한 것이다(<https://github.com/n0-computer/iroh/tree/v1.5.0>). 구현 때 v1.5.0 소스로 다시 확인하고, 다르면 이 표를 고친다.

| API | 쓰는 곳 |
|---|---|
| `Endpoint::builder()` + `Builder::secret_key` | 기기 키 seed로 endpoint를 만든다. endpoint ID = 기기 ID |
| `RelayMode::Custom(RelayMap)` | 소비자가 준 relay만 쓴다. `RelayMode::Default`(n0 운영 relay)는 쓰지 않는다 |
| `EndpointAddr` | 상대 주소: endpoint ID, relay URL, 직접 주소 |
| `Connection::remote_id()` | TLS로 확인된 상대 ID. 서명 증명을 다시 하지 않는다 |
| `Connection::paths_stream`, `Path::{is_selected, is_ip, is_relay, rtt}` | 선택 경로가 직접인지 relay인지, RTT |
| `Path::stats()`, `PathEvent::Closed` | 경로별 바이트(펀칭 실험의 relay 경유 바이트) |
| `iroh-base` `SecretKey::from_bytes`·`to_bytes`·`public`, `PublicKey::from_bytes`·`as_bytes`·`Display` | 기기 키와 같은 32바이트 |
| feature `test-utils`의 `run_relay_server`·`run_relay_server_with`·`run_relay_server_with_access`(async, `Result<(RelayMap, RelayUrl, Server), _>`) | 시험 안 relay. 자체 서명 인증서라 시험용 TLS 신뢰 설정을 함께 넘긴다 |
| `clear_ip_transports()` | 직접 경로를 실제로 막는 시험. 상대 주소에서 IP만 빼는 방법은 iroh가 직접 경로로 바꾸므로 막지 못한다 |

- 시험용 TLS 신뢰 설정과 `test-utils`는 dev-dependency로만 넣는다. 제품 경로에는 들어가지 않는다.
- iroh 기본 feature에 `fast-apple-datapath`가 있다. macOS CI에서 기본 feature로 빌드한다.

## endpoint와 런타임

- `P2pEndpoint::bind(config)`가 tokio 런타임을 만들고 endpoint를 연다. config: 기기 키, relay 목록과 정책(아래), 직접 경로 대기 시간, 페어링 허용 목록 저장소.
- 동기 API와 런타임의 관계는 [전송 추상화](transport.md) "동기·비동기 경계".
- 순서 채널 = dialer가 연 첫 양방향 스트림, bulk·요청 채널 = 같은 연결의 추가 양방향 스트림. 한 스트림 안에서 프레임 순서가 지켜진다.

## 주소와 페어링

- 기기 ID만 저장하면 사용자 relay 환경에서 상대를 찾는다는 보장이 없다. 페어링 때 양쪽이 `EndpointAddr`(endpoint ID, 홈 relay URL, 알려진 직접 주소)를 교환해 저장한다. 연결할 때는 저장한 주소로 건다. 두 기기의 relay가 달라도 된다.
- 페어링은 양쪽 사람이 승인한다. 이 레포는 페어링 메시지 형식(`PairingOffer{endpoint_addr, display_name}`, `PairingAccept{endpoint_addr, display_name}`)과 허용 목록 검사를 준다. 페어링 메시지를 어떤 경로(이미 있는 attach 링크, 사용자가 복사하는 문자열 등)로 나를지와 승인 화면은 tasty가 정한다.
- 허용 목록에 없는 endpoint의 연결은 QUIC 연결을 받은 직후 닫는다. handshake와 소비자 콜백까지 가지 않는다([handshake v2](handshake.md) "흐름 (P2P 경로)").

## relay 정책과 직접 경로 감시 (1안)

사용자 결정(2026-11-26) 1+2안 중 (1)이 이 크레이트 몫이다.

- relay 목록의 각 항목은 `RelayChoice { url, policy }`이고 `policy`는 `DirectRequired` 또는 `RelayAllowed`다.
  - `DirectRequired`: 정해 둔 시간(`direct_wait`) 안에 선택 경로가 직접(`is_ip`)이 되지 않으면 끊는다. 직접이었다가 relay로 되돌아가도 끊는다. 끊을 때 사유는 `P2pCloseReason::DirectPathTimeout` 또는 `DirectPathLost`다.
  - `RelayAllowed`: relay 경로로도 연결을 유지한다.
- 기본 relay(tasty hub)는 `DirectRequired`, 사용자 relay는 `RelayAllowed`가 기본이다. 어떤 항목에 어떤 정책을 줄지는 소비자가 정한다([relay](relay.md) "클라이언트 기본 relay").
- `direct_wait` 기본값은 임시로 10초다. [펀칭 실험](#펀칭-실험) 결과로 확정한다.
- 끊긴 연결을 다시 걸지는 소비자가 정한다. `DirectPathTimeout`·`DirectPathLost`는 같은 네트워크 조건에서 다시 걸어도 같은 결과일 가능성이 높으므로 `is_retryable()`이 false다.

## 펀칭 실험

목적: 1안의 `direct_wait` 기본값과 `punch-only` relay의 수신 상한(`[limits.client.rx]`)을 실측으로 정한다. 상한이 너무 낮으면 펀칭 자체가 실패하고, 너무 높으면 hub가 데이터를 중계하게 된다.

### 필요한 조건

| 항목 | 조건 |
|---|---|
| 기기 | 최소 2대. 각 기기에서 `punch-probe`를 실행할 수 있어야 한다(Linux·macOS·Windows 중 아무거나). UDP 차단 조합을 따로 하려면 방화벽 규칙을 바꿀 수 있는 기기 1대 |
| 네트워크 A | 가정용 공유기 뒤. 보통 endpoint-independent mapping(주소·포트 유지형) NAT |
| 네트워크 B | A와 다른 망. 모바일 테더링(이동통신사 CGNAT, endpoint-dependent mapping일 수 있음)을 1순위로 한다 |
| 추가 조합 | (1) 두 기기가 같은 공유기 안, (2) 한쪽이 나가는 UDP를 막은 상태, (3) 가능하면 양쪽 모두 endpoint-dependent mapping(펀칭이 실패해야 하는 기준 조합) |
| relay 위치 | 공인 IPv4 주소가 있는 호스트 1대. 두 네트워크 어디에서도 닿아야 한다. 사설망 안의 머신은 안 된다 |
| relay 포트 | TCP 80(ACME 인증)·443(HTTPS relay) 외부 개방. QUIC 주소 확인을 켜면 UDP 7842도. metrics 9090은 외부에 열지 않는다 |
| relay 이름·인증서 | 그 공인 주소를 가리키는 DNS 이름과 TLS 인증서(Let's Encrypt 또는 운영자 인증서). `--dev`(평문 HTTP)는 제품 클라이언트가 받지 않으므로 이 실험에 쓰지 않는다 |
| relay 설정 | `relay/punch-only.toml`(상한 값을 단계별로 바꿈)과 `relay/relay-fallback.toml`(상한 없음) 두 가지로 번갈아 띄운다 |

측정 항목: 연결 시작부터 선택 경로가 직접이 되기까지의 시간, 그동안 그 연결이 relay를 지난 바이트(클라이언트 경로 통계), 직접 → relay 되돌아감 발생 여부, RTT. relay metrics의 `bytes_recv`·`bytes_sent`는 서버 공용 카운터라 전체 부하로만 본다. 반복 횟수와 상한 값 단계는 실험 작업 지시로 정한다.

공개 문서에 실험 기록을 적을 때 기기 이름, 공인 IP, 사설 경로를 쓰지 않는다. 네트워크 종류만 적는다.

### 결과 (실험 뒤 채운다)

| 조합 | 상한 | 성공/시도 | 전환 시간 중앙값·최댓값 | relay 경유 바이트 |
|---|---|---|---|---|
| (미측정) | | | | |

확정 값: `direct_wait` = (미정), `punch-only` 상한 = (미정). 확정 근거는 이 표의 행을 가리킨다.

### 동작 수준 기준과의 관계

펀칭 실험은 "제대로 동작하는 수준"([동작 수준 기준](done-criteria.md)) 기준에 넣지 않는다. 대신 "P2P를 사용자에게 내놓기 전 기준"(같은 문서 P1)에 넣는다. 근거:

1. "제대로 동작하는 수준"은 tasty 본체에서 네트워크 코드를 걷어내기 시작해도 되는지 판단하는 기준이다. 걷어내기의 첫 대상은 지금 tasty에 있는 SSH attach다. P2P는 지금 tasty에 없는 새 기능이라, 펀칭 값이 정해지지 않아도 걷어내기를 시작할 수 있다.
2. 1안 정책이 맞게 동작하는지(시간 안에 직접 경로가 안 서면 끊고, 사용자 relay에서는 유지)는 CI에서 `clear_ip_transports()`로 직접 경로를 막은 시험이 확인한다. 실험이 정하는 것은 정책의 코드가 아니라 두 설정 값이다.
3. 실험은 사람이 서로 다른 두 네트워크의 기기와 공인 relay를 준비해야 한다. 일정을 이 레포가 정할 수 없다. 기준에 넣으면 SSH 쪽 준비가 끝나도 전환 판단이 그 일정에 묶인다.
4. 값이 정해지지 않은 채 공개하면 문제가 되는 곳은 hub relay 배포와 P2P 사용자 공개다. 그래서 그 두 일의 선행 조건으로 둔다.
