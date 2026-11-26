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
- 두 모드의 차이는 기동 상태에서 드러난다(2026-11-27, 원격 빌드 환경, 같은 머신 루프백). iroh v1.5.0 `examples/transfer`(`--relay-only`, 연결 하나, 내려받기 10초 요청)를 각 CI용 설정의 컨테이너에 붙였을 때 `punch-only`(임시 상한 16384 B/s, burst 65536)는 1,411,712 B를 87.2초에 받아 약 16.2 KB/s였고 `relayserver_conns_rx_ratelimited_total`이 1이 됐다. `relay-fallback`은 848,170,596 B를 10.0초에 받아 약 84.7 MB/s였고 상한 카운터는 0이었다. 보낸 쪽이 relay에 넣는 속도가 상한에 묶이므로, 요청한 10초가 지나도 쌓인 데이터를 받느라 전송이 길어졌다.
- `punch-only`의 상한 값은 [펀칭 실험](p2p.md#펀칭-실험) 뒤에 확정한다. 그 전까지 `relay/punch-only.toml`의 값은 임시이고 파일에 그렇게 적는다.

## 배포 형태

- 이미지: `n0computer/iroh-relay:v1.5.0`(Docker Hub). `latest`를 쓰지 않는다. 태그는 옮겨질 수 있어 `relay/compose.yaml`과 검사 스크립트는 multi-arch index digest(`sha256:50a2f569…`, 2026-11-27 확인)를 함께 고정한다. 이미지는 alpine 위에 `/iroh-relay` 하나를 ENTRYPOINT로 두고 사용자를 정하지 않는다(root). `EXPOSE`는 80·443·3478/udp·9090이라 7842/udp가 없으므로 compose가 직접 게시한다(iroh v1.5.0 `docker/Dockerfile.ci`).
- 이 레포에 둘 것: `relay/punch-only.toml`, `relay/relay-fallback.toml`, `relay/compose.yaml`, `relay/check-config.sh`, `relay/container-check.sh`, `relay/README.md`(자체 운영 안내).
- 기본 포트: HTTP 80, metrics 9090(`http_bind_addr`의 IP + 9090). HTTPS 443과 QUIC 주소 확인 UDP 7842(`DEFAULT_RELAY_QUIC_PORT`)는 `[tls]`를 설정할 때의 기본이다. 기본 설정은 `tls`가 없다. TLS가 없으면 80이 relay 자체를 평문으로 서비스하고, TLS가 있으면 80은 captive portal 확인만 평문으로 받는다(`iroh-relay/src/server.rs`).
- 내장 Let's Encrypt(`cert_mode = LetsEncrypt`)는 TLS-ALPN-01로 인증한다. 인증 요청은 TCP 80이 아니라 HTTPS 리스너(443)로 온다(`iroh-relay/src/server/http_server.rs` `tls_serve_connection`). 그래서 외부 TCP 443이 relay 프로세스의 TLS 리스너에 그대로 닿아야 하고, 그 앞에서 TLS를 끝내는 프록시를 두면 발급이 안 된다.
- `enable_quic_addr_discovery` 기본값은 false다. 7842/UDP는 그 기능을 켤 때만 쓰인다. compose에 포트만 게시해서는 열리지 않는다. 운영 예시는 기능을 켜고 TLS를 갖추고, CI 예시는 끈다. 클라이언트도 relay마다 이 기능의 사용 여부와 포트를 따로 받는다([P2P](p2p.md) `RelayChoice.quic_addr_discovery`).
- `--dev`: HTTPS 리스너를 끄고 평문 HTTP로 띄운다(TLS 설정이 있어도 `dangerous_http_only`). 그래도 `[tls]` 절이 있으면 시작할 때 인증서 설정을 먼저 읽는다. `Manual`이면 인증서·키 파일을 읽고, `LetsEncrypt`면 hostname과 contact가 있어야 한다. 어긋나면 `--dev`여도 시작하지 못한다(`main.rs` `build_relay_config`). `http_bind_addr`를 주지 않았을 때만 `[::]:3340`이고, 주면 그 주소를 쓴다. QUIC 주소 확인을 켜면 TLS 설정을 쓰며 그때 `cert_mode = LetsEncrypt`를 거절한다. iroh 클라이언트는 HTTP relay URL에도 붙을 수 있다. 이 레포가 `--dev`를 CI 기동 검사와 로컬 확인에만 쓰는 것은 운영 정책이다(평문 relay는 접속 메타데이터와 접속 허용 토큰을 보호하지 못한다).
- CI 기동 검사(`relay-config-check`)는 운영 예시 두 개(`punch-only.toml`·`relay-fallback.toml`)에서 `[tls]`와 QUIC 주소 확인만 뺀 CI용 설정을 스크립트로 만들어 띄운다. 그래서 확인하는 공통 부분은 `enable_relay`, `[limits]`(수신 상한의 값 조합), `access`, metrics 설정이다. TLS와 QUIC 주소 확인은 CI에서도 D5 (b)의 컨테이너 확인(`relay/container-check.sh`, 같은 CI용 설정을 쓴다)에서도 판정하지 않는다. 직접 경로 전환은 QUIC 주소 확인이 꺼져 있어도 일어나므로 그 로그가 두 기능의 증거가 되지 않기 때문이다. 두 기능은 relay를 실제로 배포할 때 운영 안내(`relay/README.md`)의 확인 절차로 본다. CI 잡은 GitHub 러너의 docker로 위 고정 이미지를 띄운다(컨테이너 확인과 같은 이미지).
- `relay/container-check.sh`가 띄우는 `p2p-peer`의 인터페이스는 `p2p-peer`가 생기기 전에 스크립트가 가정한 것이다. `p2p-peer`를 만드는 작업이 이 인터페이스를 따르거나, 실제 옵션에 맞춰 스크립트의 `cc_run_peers`·`cc_path_switched`와 이 항목을 같이 고친다.
  - listener: `p2p-peer --listen --relay-url <URL> --key-dir <폴더> --addr-file <파일>`. dialer가 `--dial`에 그대로 넘길 주소 문자열을 파일에 쓰고 연결 하나를 받는다.
  - dialer: `p2p-peer --dial <주소> --relay-url <URL> --key-dir <폴더>`. 선택 경로가 바뀔 때마다 `p2p-peer path-selected path=relay relay=<relay URL>` 또는 `p2p-peer path-selected path=direct addr=<IP:포트>` 한 줄을 쓰고, relay 경로 뒤에 직접 경로가 선택되면 0으로 끝난다.
  - D5 (b) 통과 조건: dialer 종료 코드 0, dialer 로그에서 시험 relay URL(끝 `/` 무시)의 `path=relay` 줄 뒤에 `path=direct` 줄이 있음, peer 실행 전후 `relayserver_accepts_total` 증가 2 이상. 처음부터 직접으로 붙은 연결은 relay를 거치지 않았으므로 통과하지 않는다.
  - dialer는 `PEER_TIMEOUT_SECONDS`(기본 60) 뒤 TERM, 그 뒤 `PEER_KILL_SECONDS`(기본 5) 뒤 KILL로 끝낸다. listener도 TERM → 대기 → KILL → 회수로 정리한다.
- 설정 파일: `--config-path`에 파일이 없으면 오류 없이 기본 설정으로 뜬다(`main.rs` `Config::load`). 설정 구조체에 `deny_unknown_fields`가 없어 모르는 키도 무시된다. 그래서 `relay/check-config.sh`는 파일이 있는지 먼저 보고, `RUST_LOG=iroh_relay=debug`일 때 시작 로그에 찍히는 `ServerConfig`(`{:#?}`)의 `client_rx`가 파일의 수신 상한과 같은지로 설정을 읽었는지 판정한다.
- HTTP 경로(`server.rs`): TLS가 없으면 `http_bind_addr` 한 곳에서 `/`·`/index.html`(200), `/healthz`(200, JSON `status`·`version`), `/ping`, `/robots.txt`, `/generate_204`(204), `/relay`(WebSocket 업그레이드. 일반 GET은 400)를 받는다. TLS가 있으면 이 경로들은 HTTPS로 가고 80은 `/generate_204`만 받는다. metrics 서버는 `/metrics`에서 Prometheus 텍스트를 내고, 이름은 `relayserver_` 접두에 카운터는 `_total`이 붙는다(예: `relayserver_accepts_total`).
- 수신 상한에 걸린 양은 `relayserver_bytes_rx_ratelimited_total`(바이트)과 `relayserver_conns_rx_ratelimited_total`(한 번이라도 걸린 연결 수)로 보인다(`server/metrics.rs`). 이것도 서버 공용 카운터라 상한이 동작했다는 것만 보여 주고 연결별 값은 아니다.
- 리버스 프록시가 TLS를 처리하는 구성은 공식 문서에 없다. `tls`를 끄면 `http_bind_addr`에서 평문 HTTP로 서비스한다는 설명만 있다. 운영 안내에 이 한계를 적는다.

## 접속 허용

iroh-relay `access` 설정: `everyone`(기본), allowlist, denylist, `shared_token`, `access.http.url`.

`access.http.url` 콜백의 공개 규약(hub가 받는 쪽):

- relay가 접속을 받을 때 그 URL로 본문 없는 HTTP POST를 보낸다. 헤더 `X-Iroh-Endpoint-Id`에 접속한 endpoint ID가 소문자 hex 64자로 있다. 호환을 위해 같은 값이 deprecated 헤더 `X-Iroh-NodeId`에도 있다(iroh가 앞으로 뺀다고 적었으므로 hub는 `X-Iroh-Endpoint-Id`를 읽는다).
- relay 설정에 `access.http.bearer_token`(또는 환경 변수 `IROH_RELAY_HTTP_BEARER_TOKEN`)이 있으면 `Authorization: Bearer <token>` 헤더가 붙는다. hub는 이 헤더로 요청이 자기 relay에서 왔는지 확인한다.
- 응답 상태가 200이고 본문 텍스트가 정확히 `true`(앞뒤 공백·줄바꿈 없음)일 때만 허용, 그 밖은 모두 거절.
- 콜백 요청은 reqwest 0.13.5의 기본 리디렉션 정책(최대 10번)을 따르므로, 200·`true` 판정은 리디렉션을 따라간 최종 응답의 상태와 본문으로 한다. 다른 호스트·포트·scheme으로 리디렉션되면 reqwest가 `Authorization` 헤더를 빼므로(`redirect.rs` `remove_sensitive_headers`) 콜백 URL은 리디렉션하지 않는 주소를 쓴다(iroh-relay v1.5.0 `main.rs` `HttpAccess`, reqwest 0.13.5 `src/redirect.rs`).
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
| 사용자 relay의 정책 | `RelayAllowed`(끊지 않음). 소비자가 바꿀 수 있다. 이 정책은 자기 목록의 relay에만 적용된다. 상대가 다른 사용자 relay를 쓰면 그 경로는 `unknown_relay_policy`를 따르고, 그 값은 [남은 결정](index.md#남은-결정)에 있다 |

## 위협 범위

relay가 보는 것과 보지 못하는 것은 [위협 모델](threat-model.md) T5에 있다.
