# relay 자체 운영 안내

tasty-network의 P2P 연결은 [iroh-relay](https://github.com/n0-computer/iroh/tree/v1.5.0/iroh-relay)를 relay로 쓴다. 이 폴더는 고정 판 공식 이미지(`n0computer/iroh-relay:v1.5.0`)를 띄우는 설정 예시와 검사 스크립트다. 자체 relay 바이너리나 관리 API는 없다. 설계와 그 근거는 [relay 설계](../docs/design/relay.md)에 있다.

| 파일 | 내용 |
|---|---|
| `punch-only.toml` | 수신 상한을 낮게 건 설정. 상한 값은 임시다 |
| `relay-fallback.toml` | 상한 없는 설정. 사용자 relay의 기본 |
| `compose.yaml` | Docker Compose 예시. 이미지를 digest로 고정한다 |
| `check-config.sh` | 두 설정의 기동 검사(CI `relay-config-check` 잡) |
| `container-check.sh` | 기동 검사 뒤 `p2p-peer` 두 개를 relay에 붙이는 확인 |

설정 키와 동작은 iroh-relay 1.5.0 소스(`iroh-relay/src/main.rs`, `iroh-relay/src/server.rs`)로 확인했다. 판을 올리면 다시 확인한다([버전·호환](../docs/design/versioning.md) "의존 고정").

## 준비물

- 공인 IP 주소가 있는 호스트 하나. 연결할 두 기기 모두에서 닿아야 한다.
- 그 주소를 가리키는 DNS 이름(A, IPv6를 쓰면 AAAA). 예시에서는 `relay.example.com`이다.
- Docker와 Docker Compose.
- 열 포트:

| 포트 | 용도 | 외부 개방 |
|---|---|---|
| TCP 443 | HTTPS relay. 내장 Let's Encrypt의 TLS-ALPN-01 인증도 이 포트로 받는다 | 연다 |
| TCP 80 | TLS 구성에서는 captive portal 확인(`/generate_204`)만 평문으로 받는다 | 연다 |
| UDP 7842 | QUIC 주소 확인. `enable_quic_addr_discovery = true`일 때만 쓰인다 | 그 기능을 켜면 연다 |
| TCP 9090 | metrics(Prometheus 텍스트) | 열지 않는다 |

## 띄우기

1. 모드를 고른다(아래 "모드 고르기").
2. 고른 설정 파일의 `[tls]`에서 `hostname`과 `contact`를 자기 값으로 고친다.
3. 띄운다.

```bash
cd relay
RELAY_CONFIG=relay-fallback.toml docker compose up -d   # 또는 punch-only.toml
docker compose logs -f iroh-relay
```

`RELAY_CONFIG`를 주지 않으면 `relay-fallback.toml`을 쓴다. 첫 기동 때 Let's Encrypt 인증서를 받아 `relay-certs` 볼륨(`cert_dir = "/data/certs"`)에 둔다.

주의할 iroh-relay 동작:

- 설정 경로(`--config-path`)에 파일이 없으면 오류 없이 기본값(TLS 없음, 평문 HTTP 80, 상한 없음)으로 뜬다. 볼륨 경로를 바꿨으면 기동 로그에 `[HTTPS] relay: serving on`이 있는지 본다.
- 설정의 모르는 키는 오류 없이 무시된다. 키 이름을 틀리면 그 설정이 빠진 채로 뜬다.
- `[limits.client.rx]`의 `bytes_per_second`·`max_burst_bytes`는 0이 아닌 u32다. `max_burst_bytes`만 쓰고 `bytes_per_second`를 빼면 relay가 시작하지 않는다.
- `enable_quic_addr_discovery = true`인데 `[tls]`가 없으면 relay가 시작하지 않는다.

## 모드 고르기

두 모드 모두 relay를 켠다(`enable_relay = true`). 차이는 클라이언트별 수신 상한이다.

| 모드 | 이럴 때 | 결과 |
|---|---|---|
| `relay-fallback` | 직접 경로가 안 되는 네트워크에서도 연결이 유지돼야 할 때. 자기 기기끼리 쓰는 relay | 직접 경로가 안 되면 relay가 데이터를 중계한다. 그 양만큼 relay 호스트의 대역폭을 쓴다 |
| `punch-only` | relay 호스트가 데이터를 중계하지 않게 하고 싶을 때(hub relay) | 펀칭 조정은 지나가고 데이터 중계는 낮은 속도로만 된다. 상한을 0으로 할 수는 없어서 낮은 속도의 중계는 남는다 |

- `enable_relay = false`는 쓰지 않는다. 펀칭 조정까지 relay를 지나지 못한다.
- `punch-only.toml`의 상한 값(`bytes_per_second = 16384`, `max_burst_bytes = 65536`)은 임시다. 펀칭이 그 상한에서 되는지는 아직 재지 않았다. 확정 값은 [P2P 설계](../docs/design/p2p.md) "펀칭 실험" 뒤에 이 파일에 반영한다.
- 상한은 연결마다의 유입 속도만 묶는다. 연결 수나 서버 전체 대역폭은 묶지 않는다. `[limits]`의 `accept_conn_limit`·`accept_conn_burst`는 1.5.0에서 설정 키는 있으나 구현되지 않아 효과가 없다(iroh-relay v1.5.0 `server.rs` `Limits`의 "Not currently implemented"). 새 연결 속도를 묶으려면 호스트 방화벽 등 relay 밖에서 한다.
- 클라이언트 쪽 정책과 함께 봐야 한다. tasty-network 클라이언트는 relay마다 `DirectRequired`(직접 경로가 시간 안에 안 서면 끊는다) 또는 `RelayAllowed`(relay로도 유지)를 고른다. `punch-only` relay를 `RelayAllowed`로 쓰면 연결은 유지되지만 아주 느리다.

## 접속 허용

`access` 키로 누가 이 relay에 붙을지 정한다. 표 머리(`[tls]` 등) 앞에 둬야 최상위 키가 된다. 기본은 `everyone`이다.

```toml
access = "everyone"

# 이 endpoint ID들만 받는다.
access.allowlist = ["<endpoint id>", "<endpoint id>"]

# 이 endpoint ID들만 막는다.
access.denylist = ["<endpoint id>"]

# 이 토큰 중 하나를 보낸 클라이언트만 받는다. 빈 목록·빈 문자열이면 시작하지 않는다.
# 환경 변수 IROH_RELAY_ACCESS_TOKEN이 있으면 그 값 하나가 목록을 대신한다.
access.shared_token = ["<token>"]

# 접속마다 외부 서비스에 묻는다(아래 규약).
access.http.url = "https://auth.example.com/relay-access"
access.http.bearer_token = "<token>"   # 또는 환경 변수 IROH_RELAY_HTTP_BEARER_TOKEN(이쪽이 우선)
```

한 설정에는 하나만 쓴다.

`access.http.url` 규약:

- relay가 접속을 받을 때 그 URL로 본문 없는 HTTP POST를 보낸다. 헤더 `X-Iroh-Endpoint-Id`에 접속한 endpoint ID(소문자 hex 64자)가 있다. 같은 값이 deprecated 헤더 `X-Iroh-NodeId`에도 있지만 iroh가 앞으로 뺀다고 했으므로 `X-Iroh-Endpoint-Id`를 읽는다.
- `bearer_token`이 있으면 `Authorization: Bearer <token>` 헤더가 붙는다. 받는 쪽은 이 헤더로 요청이 자기 relay에서 왔는지 확인한다.
- 응답 상태가 200이고 본문이 정확히 `true`(앞뒤 공백·줄바꿈 없음)일 때만 허용한다. 그 밖(다른 상태, 다른 본문, 요청 실패)은 모두 거절이다.

방식에 따른 클라이언트 쪽 차이:

- `shared_token` relay에는 토큰을 가진 클라이언트만 붙는다. tasty-network 클라이언트는 relay 항목의 `auth_token`으로 보낸다. 토큰은 자기 relay 목록에 넣은 relay에만 보낸다. 그래서 이 relay를 홈으로 쓰는 기기에 다른 기기가 relay로 닿으려면, 그 기기도 이 relay를 토큰과 함께 자기 목록에 넣어야 한다.
- `everyone`·allowlist·denylist·`access.http.url`은 endpoint ID로 판단하므로 클라이언트가 토큰을 보내지 않아도 된다. 서로 다른 relay를 쓰는 두 기기가 상대 홈 relay를 거쳐 닿을 수 있다.

## 리버스 프록시

- iroh-relay 공식 문서에는 앞단 리버스 프록시가 TLS를 처리하는 구성이 없다. `[tls]`를 빼면 `http_bind_addr`에서 평문 HTTP로 모든 서비스를 한다는 설명뿐이다. 이 레포는 그 구성을 확인하지 않았고 예시도 두지 않는다.
- 내장 Let's Encrypt(`cert_mode = "LetsEncrypt"`)는 TLS-ALPN-01이라 외부 TCP 443이 relay 프로세스의 TLS 리스너에 그대로 닿아야 한다. 앞에서 TLS를 끝내는 프록시를 두면 발급이 안 된다. 443을 다른 서비스와 나눠 써야 하면 운영자 인증서(`cert_mode = "Manual"`과 `manual_cert_path`·`manual_key_path`, PEM)를 쓰고 TLS를 끝내지 않는 TCP 전달만 둔다.
- QUIC 주소 확인(UDP 7842)은 HTTP 프록시를 지나지 않는다. 켜려면 UDP를 그대로 전달한다.

## 확인

띄운 뒤 다른 호스트에서 본다. `relay.example.com`을 자기 이름으로 바꾼다.

```bash
curl -fsS https://relay.example.com/healthz            # {"status":"ok","version":"1.5.0",...}
curl -s -o /dev/null -w '%{http_code}\n' http://relay.example.com/generate_204   # 204
echo | openssl s_client -connect relay.example.com:443 -servername relay.example.com 2>/dev/null \
  | openssl x509 -noout -subject -issuer -enddate     # 인증서 이름·발급자·만료일
```

- 기동 로그에 `[HTTPS] relay: serving on [::]:443`이 있어야 한다. QUIC 주소 확인을 켰으면 `QUIC server listening on`도 있어야 하고, 방화벽과 클라우드 보안 그룹에서 UDP 7842가 열렸는지 본다.
- 수신 상한이 설정대로 들어갔는지는 `RUST_LOG=iroh_relay=debug`로 띄웠을 때 시작 로그의 `client_rx`에서 본다(`relay/check-config.sh`가 같은 방법으로 본다).
- TLS와 QUIC 주소 확인은 이 레포의 CI와 컨테이너 확인이 판정하지 않는다. 배포할 때 위 절차로 본다.

## metrics

`http://127.0.0.1:9090/metrics`(호스트에서)가 Prometheus 텍스트 형식으로 응답한다. 자주 보는 값:

| 이름 | 뜻 |
|---|---|
| `relayserver_accepts_total` | 받은 relay 연결 수 |
| `relayserver_bytes_recv_total`·`relayserver_bytes_sent_total` | 클라이언트에서 받은·클라이언트로 보낸 데이터그램 바이트 |
| `relayserver_bytes_rx_ratelimited_total` | 수신 상한에 걸린 바이트 |
| `relayserver_conns_rx_ratelimited_total` | 수신 상한에 한 번이라도 걸린 연결 수 |

모두 서버 전체 카운터다. 연결 하나가 상한을 지켰는지는 이 값으로 알 수 없다. 클라이언트 쪽 경로 통계로 보거나, 시험 연결 하나만 둔 상태에서 카운터 차이로 본다.

## 클라이언트에 relay 넣기

tasty-network 클라이언트(`tasty-net-p2p`)는 relay 목록을 소비자가 준다. 라이브러리에 기본 relay URL은 없다([relay 설계](../docs/design/relay.md) "클라이언트 기본 relay"). 목록의 항목은 `RelayChoice { url, policy, auth_token, quic_addr_discovery }`다.

| 필드 | 넣을 값 |
|---|---|
| `url` | `https://relay.example.com` |
| `policy` | `RelayAllowed`(relay로도 유지) 또는 `DirectRequired`(직접 경로가 시간 안에 안 서면 끊음). 사용자 relay의 기본은 `RelayAllowed` |
| `auth_token` | `access.shared_token`을 쓰면 그 토큰. 아니면 비운다 |
| `quic_addr_discovery` | relay에서 `enable_quic_addr_discovery = true`면 `Port(7842)`(또는 바꾼 `quic_bind_addr`의 포트), 아니면 `Off`(기본) |

tasty에서 이 값을 넣는 설정 화면과 키 이름은 tasty 문서를 따른다.

## 검사 스크립트

두 스크립트 모두 docker와 curl이 필요하다. 이미지를 digest로 고정해 받고, 받은 이미지와 띄운 컨테이너, 임시 폴더를 끝날 때 지운다(`--keep-image`면 이미지는 남긴다).

```bash
bash relay/check-config.sh                         # 두 설정의 기동 검사
bash relay/container-check.sh --relay-only         # 같은 기동 검사(p2p-peer 없이)
bash relay/container-check.sh --peer-bin <경로>    # 기동 검사 + p2p-peer 두 개 연결
```

- 운영 설정에서 `[tls]`와 `enable_quic_addr_discovery`를 뺀 설정을 임시로 만들어 `--dev`(평문 HTTP)로 띄운다. 그래서 TLS와 QUIC 주소 확인은 보지 않고, `enable_relay`·`[limits]`·`access`·metrics 설정을 본다.
- 설정마다 `relay-check config=<이름> result=<ok|relay-only|fail> ...` 한 줄을 쓴다. 종료 코드는 0 통과, 1 실패, 2 판정 불가(docker·`p2p-peer` 없음 등)다.
- `--dev`는 검사와 로컬 확인에만 쓴다. 평문 relay는 접속 메타데이터와 접속 허용 토큰을 보호하지 못한다.
