# P2P 전송 (iroh)

크레이트는 `tasty-net-p2p`다. iroh는 `=1.5.0`으로 고정한다([버전·호환](versioning.md)). relay 배포와 relay 모드는 [relay](relay.md)에 있다.

## 쓰는 iroh 1.5.0 API

아래 목록은 계획 단계에서 docs.rs와 v1.5.0 소스로 확인한 것이다(<https://github.com/n0-computer/iroh/tree/v1.5.0>). 구현 때 v1.5.0 소스로 다시 확인하고, 다르면 이 표를 고친다.

| API | 쓰는 곳 |
|---|---|
| `Endpoint::builder(presets::Minimal)` + `Builder::secret_key` + `Builder::relay_mode` + `Builder::alpns` | 기기 키 seed로 endpoint를 만든다. endpoint ID = 기기 ID. preset은 `Minimal`(crypto provider만 정함)이다. iroh는 `tls-ring`이나 `tls-aws-lc-rs` 중 하나가 켜져 있으면 `Minimal`을 주고 둘 다면 ring을 고른다(`src/endpoint/presets.rs`). 이 설계는 `tls-ring`을 고른다. `N0` preset은 n0 DNS 주소 조회와 n0 relay를 켜므로 쓰지 않는다 |
| `RelayMode::Custom(RelayMap)` | 이 endpoint의 홈 relay 후보를 소비자가 준 relay로 한정한다. `RelayMode::Default`(n0 운영 relay)는 쓰지 않는다. 상대 주소에 다른 relay URL이 있으면 iroh는 로컬 map에 없어도 그 relay에 접속한다. 그때 인증 토큰은 로컬 map의 같은 URL 항목에서만 찾는다(`src/socket/transports/relay/actor.rs` `start_active_relay`). QUIC 주소 확인은 map 항목의 `RelayConfig.quic`으로만 시도한다(`src/net_report/reportgen.rs`) |
| `EndpointAddr` | 상대 주소: endpoint ID, relay URL, 직접 주소 |
| `Connection::remote_id()` | TLS로 확인된 상대 ID. 서명 증명을 다시 하지 않는다 |
| `Connection::path_events`(`PathEvent`: 열림·닫힘·선택 경로 바뀜·`Lagged`), `Connection::paths`·`paths_stream`(스냅샷), `Path::{is_selected, is_ip, is_relay, rtt}` | 선택 경로가 직접인지 relay인지, RTT. 정책은 두 입력을 함께 쓴다. `PathEvent::Selected`는 선택이 바뀔 때마다 오므로 짧게 relay로 갔다 돌아온 것도 이것으로 본다. `paths_stream` 스냅샷은 poll할 때의 상태만 주므로 그 사이의 전환이 빠질 수 있다. 대신 시작 때의 상태, 선택 경로가 없는 구간(이때는 `Selected`가 오지 않는다), `Lagged` 뒤의 현재 상태를 스냅샷으로 맞춘다. `path_events`는 경로 전환 이력(로그·펀칭 측정)에도 쓴다 |
| `Path::stats()`, `PathEvent::Closed` | 경로별 바이트(펀칭 실험의 relay 경유 바이트) |
| `iroh-base` `SecretKey::from_bytes`·`to_bytes`·`public`, `PublicKey::from_bytes`·`as_bytes`·`Display` | 기기 키와 같은 32바이트 |
| feature `test-utils`의 `run_relay_server`·`run_relay_server_with`·`run_relay_server_with_access`(async, `Result<(RelayMap, RelayUrl, Server), _>`) | 시험 안 relay. 자체 서명 인증서라 시험용 TLS 신뢰 설정을 함께 넘긴다 |
| `ConnectWithOptsError::SelfConnect` | 같은 endpoint ID(같은 기기 키)로 연결하면 iroh가 스트림 전에 거절한다. 로컬 오류 `SelfAttach`로 분류한다 |
| `clear_ip_transports()` | 직접 경로를 실제로 막는 시험. 상대 주소에서 IP만 빼는 방법은 iroh가 직접 경로로 바꾸므로 막지 못한다 |

- 시험용 TLS 신뢰 설정과 `test-utils`는 dev-dependency로만 넣는다. 제품 경로에는 들어가지 않는다.
- iroh 기본 feature에 `fast-apple-datapath`가 있다. macOS CI에서 기본 feature로 빌드한다.

## endpoint와 런타임

- `P2pEndpoint::bind(config)`가 tokio 런타임을 만들고 endpoint를 연다. config: 기기 키, relay 목록과 정책(아래), 직접 경로 대기 시간, 페어링 허용 목록 저장소.
- 동기 API와 런타임의 관계는 [전송 추상화](transport.md) "동기·비동기 경계".
- ALPN은 `tasty-net/2` 하나다. 받는 쪽은 `Builder::alpns`로 등록하고 dialer는 연결할 때 같은 값을 넘긴다. 숫자는 handshake `proto`와 같이 움직인다. `proto`를 올리면 ALPN도 새 값을 쓰고, 다른 ALPN의 연결은 iroh가 받지 않는다([버전·호환](versioning.md)).
- QUIC 연결 하나 위의 스트림마다 handshake 메시지 1의 `purpose`로 용도를 정한다. 연결 뒤 첫 스트림이 `request`여도 된다(attach 전에 목록·상태를 묻는 경우). 순서 채널은 `purpose = attach`로 연 스트림이고, bulk 스트림은 같은 QUIC 연결 위의 attach 링크를 부모로 가진다. 한 스트림 안에서 프레임 순서가 지켜진다.
- 같은 기기 키를 쓰는 두 인스턴스(같은 키 디렉터리)끼리는 endpoint ID가 같아 iroh가 `SelfConnect`로 거절한다. 다른 인스턴스끼리 P2P로 붙으려면 키 디렉터리가 달라야 한다. endpoint ID가 다를 때만 handshake의 인스턴스 ID 비교가 의미를 가진다.
- 같은 기기 키로 P2P endpoint는 하나만 연다. iroh-relay는 같은 endpoint ID의 접속 중 가장 최근 것 하나만 활성으로 두고 패킷을 그쪽에만 보낸다(`iroh-relay/src/server/clients.rs` `register`). 그래서 두 인스턴스가 같은 키로 endpoint를 열면 제3자는 둘 중 하나에만 닿는다. `P2pEndpoint::bind`는 키 디렉터리의 잠금 파일을 배타로 잡고, 이미 잡혀 있으면 `P2pError::EndpointInUse`를 돌려준다. 한 인스턴스가 그 기기 ID를 대표하고, 독립 P2P endpoint가 필요한 인스턴스는 다른 키 디렉터리를 쓴다. SSH 경로는 이 제약 없이 같은 키를 함께 쓴다.

## 주소와 페어링

- 기기 ID만 저장하면 사용자 relay 환경에서 상대를 찾는다는 보장이 없다. 페어링 때 양쪽이 `EndpointAddr`(endpoint ID, 홈 relay URL, 알려진 직접 주소)를 교환해 저장한다. 연결할 때는 저장한 주소로 건다.
- 두 기기의 relay가 달라도 되는 조건: 상대 홈 relay가 이 기기를 클라이언트 토큰 없이 받아야 한다(`everyone`, allowlist·denylist, `access.http.url`처럼 endpoint ID로 판단하는 방식). iroh는 상대 relay의 토큰을 로컬 relay 목록에서만 찾기 때문이다. 상대 relay가 `shared_token`을 요구하면 그 relay를 토큰과 함께 자기 relay 목록에 넣어야 닿는데, 그러면 그 relay가 이 기기의 홈 relay 후보도 된다. 소비자는 이 대가를 알고 목록을 정한다. D4의 `different_relays_connect_with_pairing_address`는 토큰 없는 두 relay로 확인한다.
- 페어링은 양쪽 사람이 승인한다. 이 레포는 페어링 메시지 형식(`PairingOffer{endpoint_addr, display_name}`, `PairingAccept{endpoint_addr, display_name}`)과 허용 목록 검사를 준다. 페어링 메시지를 어떤 경로(이미 있는 attach 링크, 사용자가 복사하는 문자열 등)로 나를지와 승인 화면은 tasty가 정한다.
- 허용 목록에 없는 endpoint의 연결은 QUIC 연결을 받은 직후 닫는다. handshake와 소비자 콜백까지 가지 않는다([handshake v2](handshake.md) "흐름 (P2P 경로)").

## relay 정책과 직접 경로 감시 (1안)

사용자 결정(2026-11-26) 1+2안 중 (1)이 이 크레이트 몫이다.

- relay 목록의 각 항목은 `RelayChoice { url, policy, auth_token, quic_addr_discovery }`이고 `policy`는 `DirectRequired` 또는 `RelayAllowed`다. `auth_token`은 relay가 `shared_token` 접속 허용을 쓸 때 iroh `RelayConfig::with_auth_token`으로 넘긴다. `quic_addr_discovery`는 `Off` 또는 `Port(u16)`이고 기본은 `Off`다. relay 서버의 QUIC 주소 확인은 기본 꺼짐인데, iroh의 URL → `RelayConfig` 기본 변환은 7842 포트로 주소 확인을 시도하므로(`iroh-relay/src/relay_map.rs`) 그대로 두지 않고 이 값으로 `RelayConfig.quic`을 정한다.
  - `DirectRequired`: 정해 둔 시간(`direct_wait`) 안에 선택 경로가 직접(`is_ip`)이 되지 않으면 끊는다. 직접이었다가 relay로 되돌아가도 끊는다. 되돌아감은 `Selected` 이벤트나 스냅샷에서 relay 경로가 선택된 것을 본 때다. `Lagged`로 이벤트를 놓쳤으면 그동안의 이력은 복원하지 않고 스냅샷의 현재 상태로 판정한다. 지금 relay가 선택돼 있으면 되돌아감으로 끊고, 직접이면 유지하고 놓친 구간이 있었다고 로그에 남긴다. 끊을 때 사유는 `P2pCloseReason::DirectPathTimeout`(코드 2) 또는 `DirectPathLost`(코드 3)다.
  - `RelayAllowed`: relay 경로로도 연결을 유지한다.
- 정책은 양쪽이 각자 자기 목록으로 판단한다. 기준은 지금 선택된 relay 경로의 relay URL이다. 그 URL이 자기 목록에 있으면 그 항목의 정책, 없으면(페어링 주소로 받은 상대의 홈 relay 등) config의 `unknown_relay_policy`(기본 `DirectRequired`)를 쓴다. 한쪽이 끊으면 그 QUIC 연결 전체를 두 사유 중 하나의 종료 코드로 닫는다([handshake v2](handshake.md) "QUIC 종료 코드"). 상대는 종료 프레임을 받은 경우에만 같은 사유를 보고, 받지 못하면 idle timeout으로 끝난다.
- `DirectRequired`가 보장하는 것은 직접 경로를 시간 안에 얻지 못하거나 잃었을 때의 종료다. relay 중계가 0이라는 보장은 아니다. iroh는 선택 경로로 앱 데이터를 보내므로, `direct_wait` 동안과 직접 → relay로 되돌아간 뒤 끊기 전까지 handshake와 앱 프레임은 relay를 지날 수 있다. `punch-only` relay의 수신 상한이 그 양을 낮은 속도로 묶는다. 직접 경로가 확인될 때까지 앱 프레임을 보류하는 계약은 지금 두지 않았다([남은 결정](index.md#남은-결정)).
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
| 추가 조합 | (1) 두 기기가 같은 공유기 안, (2) 한쪽이 나가는 UDP를 막은 상태, (3) 가능하면 양쪽 모두 endpoint-dependent mapping(펀칭 실패가 예상되는 관측 조합. 포트 예측이나 포트 매핑(iroh 기본 feature의 portmapper)으로 직접 경로가 설 수도 있어 실패를 확정하는 조건은 아니다. 직접 경로를 확실히 막는 대조는 (2)와 CI의 `clear_ip_transports()`다) |
| relay 위치 | 공인 IPv4 주소가 있는 호스트 1대. 두 네트워크 어디에서도 닿아야 한다. 사설망 안의 머신은 안 된다 |
| relay 포트 | TCP 443(HTTPS relay. 내장 Let's Encrypt의 TLS-ALPN-01 인증도 이 포트로 받는다)·80(TLS 구성에서는 captive portal용 평문 HTTP) 외부 개방. QUIC 주소 확인을 켜면 UDP 7842도. metrics 9090은 외부에 열지 않는다 |
| relay 이름·인증서 | 그 공인 주소를 가리키는 DNS 이름과 TLS 인증서(Let's Encrypt 또는 운영자 인증서). iroh 클라이언트는 HTTP relay URL에도 접속할 수 있지만, 이 실험은 제품과 같은 조건을 재려는 것이라 운영 정책으로 `--dev`(평문 HTTP)를 쓰지 않는다 |
| relay 설정 | `relay/punch-only.toml`(상한 값을 단계별로 바꿈. 상한을 낮춰도 0은 아니어서 낮은 속도의 중계는 남는다)과 `relay/relay-fallback.toml`(상한 없음) 두 가지로 번갈아 띄운다 |

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
