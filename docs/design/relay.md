# relay

relay는 iroh-relay를 쓴다. 이 레포는 자체 relay 바이너리나 관리 API를 만들지 않고 고정 버전 공식 이미지와 설정 예시를 둔다. 사실 출처는 iroh v1.5.0 `iroh-relay/src/main.rs`·`iroh-relay/src/defaults.rs`이고, 구현(설정 예시 작성) 때 같은 소스로 다시 확인한다.

## 모드

사용자 결정: relay 동작은 relay 운영자 설정으로 고른다. 두 모드 모두 `enable_relay = true`(기본값)다.

| 모드 | 설정 | 뜻 |
|---|---|---|
| `punch-only` | `[limits.client.rx]`의 `bytes_per_second`·`max_burst_bytes`를 낮게 | relay 쪽은 낮은 속도 제한일 뿐이다(아래). 중계 양을 줄이는 것은 이 제한과 클라이언트의 `DirectRequired` 정책([P2P](p2p.md#relay-정책과-직접-경로-감시-1안))의 조합이다. 이 조합도 직접 경로를 기다리는 동안의 낮은 속도 중계는 막지 않는다. hub relay가 쓴다 |
| `relay-fallback` | 상한 없음, 또는 운영자가 고른 상한 | 직접 경로가 안 되면 relay로 중계한다. 사용자 relay의 기본 |

- `enable_relay = false`는 쓰지 않는다. 초기 피어 중계까지 없애고 QUIC 주소 확인만 남기므로 펀칭 조정이 relay를 지나지 못한다(사용자 결정 기록의 Codex 확인).
- 수신 제한은 클라이언트별 유입 token bucket이다. `bytes_per_second`는 0이 아닌 u32여야 하고, `max_burst_bytes`(0이 아닌 u32)는 `bytes_per_second` 없이 쓰면 relay가 시작하지 않는다(`main.rs` limits 처리). 0으로 막을 수는 없으므로 낮은 속도의 중계는 언제나 남는다. 펀칭이 그 상한에서 성립하는지는 아직 재지 않았다([펀칭 실험](p2p.md#펀칭-실험)).
- 상한은 연결마다의 유입 속도와 burst만 제한한다. relay metrics의 `bytes_recv`·`bytes_sent`는 서버 공용 카운터라 연결별 상한 확인에 쓰지 않는다(`iroh-relay/src/server/metrics.rs`).
- `punch-only`의 상한 값은 [펀칭 실험](p2p.md#펀칭-실험) 뒤에 확정한다. 그 전까지 `relay/punch-only.toml`의 값은 임시이고 파일에 그렇게 적는다.

## 배포 형태

- 이미지: `n0computer/iroh-relay:v1.5.0`(Docker Hub). `latest`를 쓰지 않는다.
- 이 레포에 둘 것: `relay/punch-only.toml`, `relay/relay-fallback.toml`, `relay/compose.yaml`, `relay/check-config.sh`, `relay/README.md`(자체 운영 안내).
- 기본 포트: HTTP 80, metrics 9090(`http_bind_addr`의 IP + 9090). HTTPS 443과 QUIC 주소 확인 UDP 7842(`DEFAULT_RELAY_QUIC_PORT`)는 `[tls]`를 설정할 때의 기본이다. 기본 설정은 `tls`가 없다. TLS가 없으면 80이 relay 자체를 평문으로 서비스하고, TLS가 있으면 80은 captive portal 확인만 평문으로 받는다(`iroh-relay/src/server.rs`).
- 내장 Let's Encrypt(`cert_mode = LetsEncrypt`)는 TLS-ALPN-01로 인증한다. 인증 요청은 TCP 80이 아니라 HTTPS 리스너(443)로 온다(`iroh-relay/src/server/http_server.rs` `tls_serve_connection`). 그래서 외부 TCP 443이 relay 프로세스의 TLS 리스너에 그대로 닿아야 하고, 그 앞에서 TLS를 끝내는 프록시를 두면 발급이 안 된다.
- `enable_quic_addr_discovery` 기본값은 false다. 7842/UDP는 그 기능을 켤 때만 쓰인다. compose에 포트만 게시해서는 열리지 않는다. 운영 예시는 기능을 켜고 TLS를 갖추고, CI 예시는 끈다. 클라이언트도 relay마다 이 기능의 사용 여부와 포트를 따로 받는다([P2P](p2p.md) `RelayChoice.quic_addr_discovery`).
- `--dev`: HTTPS 리스너를 끄고 평문 HTTP로 띄운다(TLS 설정이 있어도 `dangerous_http_only`). 그래도 `[tls]` 절이 있으면 시작할 때 인증서 설정을 먼저 읽는다. `Manual`이면 인증서·키 파일을 읽고, `LetsEncrypt`면 hostname과 contact가 있어야 한다. 어긋나면 `--dev`여도 시작하지 못한다(`main.rs` `build_relay_config`). `http_bind_addr`를 주지 않았을 때만 `[::]:3340`이고, 주면 그 주소를 쓴다. QUIC 주소 확인을 켜면 TLS 설정을 쓰며 그때 `cert_mode = LetsEncrypt`를 거절한다. iroh 클라이언트는 HTTP relay URL에도 붙을 수 있다. 이 레포가 `--dev`를 CI 기동 검사와 로컬 확인에만 쓰는 것은 운영 정책이다(평문 relay는 접속 메타데이터와 접속 허용 토큰을 보호하지 못한다).
- CI 기동 검사(`relay-config-check`)는 운영 예시 두 개(`punch-only.toml`·`relay-fallback.toml`)에서 `[tls]`와 QUIC 주소 확인만 뺀 CI용 설정을 스크립트로 만들어 띄운다. 그래서 확인하는 공통 부분은 `enable_relay`, `[limits]`(수신 상한의 값 조합), `access`, metrics 설정이다. TLS와 QUIC 주소 확인은 CI에서 확인하지 않고 원격 빌드 환경의 컨테이너 확인(D5 (b))과 운영 안내가 맡는다.
- 리버스 프록시가 TLS를 처리하는 구성은 공식 문서에 없다. `tls`를 끄면 `http_bind_addr`에서 평문 HTTP로 서비스한다는 설명만 있다. 운영 안내에 이 한계를 적는다.

## 접속 허용

iroh-relay `access` 설정: `everyone`(기본), allowlist, denylist, `shared_token`, `access.http.url`.

`access.http.url` 콜백의 공개 규약(hub가 받는 쪽):

- relay가 접속을 받을 때 그 URL로 본문 없는 HTTP POST를 보낸다. 헤더 `X-Iroh-Endpoint-Id`에 접속한 endpoint ID가 소문자 hex 64자로 있다. 호환을 위해 같은 값이 deprecated 헤더 `X-Iroh-NodeId`에도 있다(iroh가 앞으로 뺀다고 적었으므로 hub는 `X-Iroh-Endpoint-Id`를 읽는다).
- relay 설정에 `access.http.bearer_token`(또는 환경 변수 `IROH_RELAY_HTTP_BEARER_TOKEN`)이 있으면 `Authorization: Bearer <token>` 헤더가 붙는다. hub는 이 헤더로 요청이 자기 relay에서 왔는지 확인한다.
- 응답 상태가 200이고 본문 텍스트가 정확히 `true`(앞뒤 공백·줄바꿈 없음)일 때만 허용, 그 밖은 모두 거절.
- `shared_token` 방식을 쓰는 relay는 클라이언트도 토큰을 보내야 한다. 클라이언트 쪽은 `RelayChoice.auth_token`으로 지원한다. 토큰은 자기 relay 목록에 있는 relay에만 보낼 수 있으므로, `shared_token` relay를 홈으로 쓰는 상대에게는 그 relay를 목록에 넣은 기기만 relay로 닿는다([P2P](p2p.md) "주소와 페어링").

hub가 누구를 허용할지(페어링된 기기만, 로그인 사용자만 등)는 hub 쪽 결정이다. 이 레포는 정하지 않는다([남은 결정](index.md#남은-결정)).

## 클라이언트 기본 relay

사용자 결정: 클라이언트 기본 relay는 tasty hub다.

| 질문 | 결정 |
|---|---|
| 사용자 지정이 없을 때 relay URL을 누가 주나 | 소비자(tasty) 설정. 이 레포에는 기본 URL 상수를 두지 않는다. 라이브러리를 다른 곳에서 쓸 때 모르게 hub에 붙지 않게 하고, hub 주소가 바뀌어도 이 레포 릴리스가 필요 없게 한다 |
| relay를 하나도 주지 않으면 | `P2pEndpoint::bind`가 오류를 돌려준다. `RelayMode::Default`(n0 운영 relay)로 바꾸지 않는다 |
| 사용자 relay와 기본 relay가 둘 다 있으면 | 사용자 relay가 이긴다. 사용자 relay를 설정하면 기본 relay를 relay 목록에 넣지 않는다. 이 우선순위는 이 기기의 홈 relay 후보(`RelayMode::Custom`)에만 적용된다. 상대의 페어링 주소에 있는 상대 홈 relay에는 목록과 관계없이 iroh가 접속하고, 그 경로의 정책은 [P2P](p2p.md#relay-정책과-직접-경로-감시-1안)의 `unknown_relay_policy` 규칙을 따른다 |
| 기본 relay의 정책 | `DirectRequired`(1안 적용). hub relay는 `punch-only`라 직접 경로를 기다리는 동안에도 낮은 속도로만 중계되고, 그 시간이 지나면 끊기 때문이다 |
| 사용자 relay의 정책 | `RelayAllowed`(끊지 않음). 소비자가 바꿀 수 있다 |

## 위협 범위

relay가 보는 것과 보지 못하는 것은 [위협 모델](threat-model.md) T5에 있다.
