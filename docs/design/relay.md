# relay

relay는 iroh-relay를 쓴다. 이 레포는 자체 relay 바이너리나 관리 API를 만들지 않고 고정 버전 공식 이미지와 설정 예시를 둔다. 사실 출처는 iroh v1.5.0 `iroh-relay/src/main.rs`·`iroh-relay/src/defaults.rs`이고, 구현(설정 예시 작성) 때 같은 소스로 다시 확인한다.

## 모드

사용자 결정: relay 동작은 relay 운영자 설정으로 고른다. 두 모드 모두 `enable_relay = true`(기본값)다.

| 모드 | 설정 | 뜻 |
|---|---|---|
| `punch-only` | `[limits.client.rx]`의 `bytes_per_second`·`max_burst_bytes`를 낮게 | 펀칭 조정에 필요한 초기 중계는 되고, 데이터 중계는 사실상 막힌다. hub relay가 쓴다 |
| `relay-fallback` | 상한 없음, 또는 운영자가 고른 상한 | 직접 경로가 안 되면 relay로 중계한다. 사용자 relay의 기본 |

- `enable_relay = false`는 쓰지 않는다. 초기 피어 중계까지 없애고 QUIC 주소 확인만 남기므로 펀칭 조정이 relay를 지나지 못한다(사용자 결정 기록의 Codex 확인).
- 상한은 연결마다의 유입 속도와 burst만 제한한다. relay metrics의 `bytes_recv`·`bytes_sent`는 서버 공용 카운터라 연결별 상한 확인에 쓰지 않는다(`iroh-relay/src/server/metrics.rs`).
- `punch-only`의 상한 값은 [펀칭 실험](p2p.md#펀칭-실험) 뒤에 확정한다. 그 전까지 `relay/punch-only.toml`의 값은 임시이고 파일에 그렇게 적는다.

## 배포 형태

- 이미지: `n0computer/iroh-relay:v1.5.0`(Docker Hub). `latest`를 쓰지 않는다.
- 이 레포에 둘 것: `relay/punch-only.toml`, `relay/relay-fallback.toml`, `relay/compose.yaml`, `relay/check-config.sh`, `relay/README.md`(자체 운영 안내).
- 기본 포트: HTTP 80, HTTPS 443, metrics 9090(`http_bind_addr`의 IP + 9090), QUIC 주소 확인 UDP 7842(`DEFAULT_RELAY_QUIC_PORT`).
- `enable_quic_addr_discovery` 기본값은 false다. 7842/UDP는 그 기능을 켤 때만 쓰인다. compose에 포트만 게시해서는 열리지 않는다. 운영 예시는 기능을 켜고 TLS를 갖추고, CI 예시는 끈다.
- `--dev`: 평문 HTTP `[::]:3340`으로 띄우고 HTTPS 바인딩을 끈다. QUIC 주소 확인을 켜면 TLS 설정을 쓰며 그때 `cert_mode = LetsEncrypt`를 거절한다. CI 기동 검사와 로컬 확인에만 쓴다.
- 리버스 프록시가 TLS를 처리하는 구성은 공식 문서에 없다. `tls`를 끄면 `http_bind_addr`에서 평문 HTTP로 서비스한다는 설명만 있다. 운영 안내에 이 한계를 적는다.

## 접속 허용

iroh-relay `access` 설정: `everyone`(기본), allowlist, denylist, `shared_token`, `access.http.url`.

`access.http.url` 콜백의 공개 규약(hub가 받는 쪽):

- relay가 접속을 받을 때 그 URL로 HTTP POST를 보낸다. 헤더 `X-Iroh-Endpoint-Id`에 접속한 endpoint ID가 있다.
- 응답이 200이고 본문이 `true`면 허용, 그 밖은 거절.

hub가 누구를 허용할지(페어링된 기기만, 로그인 사용자만 등)는 hub 쪽 결정이다. 이 레포는 정하지 않는다([남은 결정](index.md#남은-결정)).

## 클라이언트 기본 relay

사용자 결정: 클라이언트 기본 relay는 tasty hub다.

| 질문 | 결정 |
|---|---|
| 사용자 지정이 없을 때 relay URL을 누가 주나 | 소비자(tasty) 설정. 이 레포에는 기본 URL 상수를 두지 않는다. 라이브러리를 다른 곳에서 쓸 때 모르게 hub에 붙지 않게 하고, hub 주소가 바뀌어도 이 레포 릴리스가 필요 없게 한다 |
| relay를 하나도 주지 않으면 | `P2pEndpoint::bind`가 오류를 돌려준다. `RelayMode::Default`(n0 운영 relay)로 바꾸지 않는다 |
| 사용자 relay와 기본 relay가 둘 다 있으면 | 사용자 relay가 이긴다. 사용자 relay를 설정하면 기본 relay를 relay 목록에 넣지 않는다 |
| 기본 relay의 정책 | `DirectRequired`(1안 적용). hub relay는 `punch-only`라 relay 경로로 유지해도 데이터가 거의 흐르지 않기 때문이다 |
| 사용자 relay의 정책 | `RelayAllowed`(끊지 않음). 소비자가 바꿀 수 있다 |

## 위협 범위

relay가 보는 것과 보지 못하는 것은 [위협 모델](threat-model.md) T5에 있다.
