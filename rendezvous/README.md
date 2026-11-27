# rendezvous 서버 자체 운영 안내

tasty-network의 P2P 연결은 rendezvous 서버 하나로 서로를 찾는다. 서버는 두 가지만 한다. 기기에게 바깥에서 보이는 주소를 알려 주고(STUN Binding), 같은 서버에 붙은 두 기기의 동시 펀칭을 조정한다. 기기 사이 데이터는 중계하지 않는다. 이 폴더는 그 서버(`tasty-rendezvous`, 크레이트 `tasty-net-rendezvous`)를 직접 운영하기 위한 이미지 빌드 파일, 설정 예시, 검사 스크립트다. 설계와 그 근거는 [rendezvous 설계](../docs/design/rendezvous.md)에 있다.

| 파일 | 내용 |
|---|---|
| `Dockerfile` | 이 레포 소스로 `tasty-rendezvous`를 빌드하는 다단계 이미지. 비root 사용자로 돈다 |
| `rendezvous.toml` | 설정 예시. 기본값과 같은 값은 주석으로만 적었다 |
| `compose.yaml` | Docker Compose 예시. UDP 3478·3479를 게시하고 서버 키를 볼륨에 둔다 |
| `check-config.sh` | 예시 설정 검사와 주·보조 포트의 STUN 응답 확인(CI `rendezvous-check` 잡) |

외부 서버 이미지는 쓰지 않는다. 이미지의 서버 판은 빌드한 트리의 workspace 버전이다([버전·호환](../docs/design/versioning.md)). 배포할 판은 그 태그(`v<semver>`)를 checkout한 트리에서 빌드한다.

## 준비물

- 공인 IP 주소가 있는 호스트 하나. 연결할 기기들이 모두 닿아야 한다.
- Docker와 Docker Compose. 컨테이너 없이 돌리려면 Rust 툴체인(`rust-toolchain.toml`의 판)으로 `cargo build --release --locked -p tasty-net-rendezvous --bin tasty-rendezvous`를 빌드한다.
- 열 포트. 모두 UDP다. TCP는 쓰지 않는다.

| 포트 | 받는 것 | 외부 개방 |
|---|---|---|
| UDP 3478(주) | QUIC 조정 연결과 STUN Binding | 연다 |
| UDP 3479(보조) | STUN Binding만. 기기의 NAT 판별(포트 의존)에 쓴다 | 연다 |

두 번째 공인 IP를 쓰면 그 IP의 UDP 3478·3479도 연다(아래 "두 번째 IP").

## 서버 키 만들기

기기는 서버 주소와 서버 공개키 한 쌍을 설정으로 받고, 접속할 때 TLS로 받은 서버 키가 그 공개키와 같은지 본다(키 고정). 공용 인증 기관과 도메인 인증서는 쓰지 않는다. 그래서 서버를 띄우기 전에 서버 키를 한 번 만든다.

```bash
cd rendezvous
docker compose build
docker compose run --rm rendezvous keygen --dir /var/lib/tasty-rendezvous
# server key created
# public key: <소문자 hex 64자>
```

- 키는 `rendezvous-key` 볼륨(설정의 `key_dir`)에 `device_key` 파일로 남는다. 폴더는 0700, 파일은 0600이고 컨테이너의 `rendezvous` 사용자만 읽는다.
- 같은 명령을 다시 부르면 키를 새로 만들지 않고 `existing server key kept`와 같은 공개키를 찍는다. 공개키를 다시 볼 때도 이 명령을 쓴다. 서버도 시작할 때 `rendezvous server started` 줄에 `public_key=`로 찍는다.
- 볼륨을 지우면 키가 바뀐다. 그러면 모든 기기의 서버 공개키 설정을 고쳐야 한다. 볼륨은 백업해 둔다.

컨테이너 없이 돌릴 때는 `tasty-rendezvous keygen --dir <폴더>`로 만들고 설정의 `key_dir`를 그 폴더로 둔다.

## 공개키 배포

기기(tasty)의 P2P 설정에 서버 주소(`host:port`, 주 포트)와 위 공개키를 한 쌍으로 적는다. 서버를 지정한 기기는 그 서버 하나만 쓴다. 같은 서버에 붙은 기기끼리만 서로를 찾으므로 연결할 기기들은 모두 같은 서버와 공개키를 적는다.

- 공개키는 비밀이 아니다. 다만 기기 설정에 적힌 값이 곧 신뢰의 근거이므로, 기기 주인이 믿는 경로(직접 복사, 자기 저장소)로 전한다.
- 개인키(`device_key` 파일)는 서버 밖으로 내보내지 않는다.

## 광고 주소 정하기

서버는 듣는 주소(`listen.*`)와 기기에게 알리는 주소(`advertise.*`)를 따로 둔다. 기기는 자기가 접속한 주소를 주 STUN 주소로 쓰고, 보조 포트와 두 번째 IP는 서버가 알려 준 주소로 질의한다.

- 컨테이너 안에서는 듣는 주소가 컨테이너 주소(`0.0.0.0`)이므로 `advertise.primary_ip`를 반드시 준다. 없으면 서버가 시작하지 않는다. `rendezvous.toml`의 `203.0.113.10`(문서용 주소)을 이 호스트의 공인 IP로 바꾼다.
- 보조 포트를 다른 바깥 포트로 게시하면(`compose.yaml`의 `ports`) `advertise.alternate_port`에 그 바깥 포트를 적는다. 주 포트는 기기 설정의 주소로 정해지므로 광고하지 않는다.
- 로컬 IP가 여럿인 호스트에서 컨테이너 없이 돌리면 `listen.primary`를 특정 IP로 둔다. wildcard로 들으면 STUN 응답이 질의 대상과 다른 IP에서 나갈 수 있고, 기기는 그 응답을 버린다. 서버는 wildcard로 묶이면 시작할 때 경고 한 줄을 찍는다.
- IPv6도 받으려면 `listen.primary`를 `[::]:3478`로 둔다. 서버는 이중 스택 소켓을 연다.

설정한 값과 출처(`default`·`configured`·`fixed`·`computed`)는 띄우기 전에 확인한다.

```bash
docker compose run --rm rendezvous settings --config /etc/tasty-rendezvous/rendezvous.toml
```

## 띄우기

```bash
cd rendezvous
docker compose up -d
docker compose logs -f rendezvous
```

- 시작 로그에 `rendezvous server started`와 설정 항목마다 `setting` 줄이 찍힌다.
- 상태(등록 수, 진행 중 펀칭 수, 상한에 걸린 횟수)는 `status_log_interval`(기본 60 s)마다 로그로 찍힌다. 원격 조회 엔드포인트는 없다.
- 로그 수준은 `TASTY_RENDEZVOUS_LOG`다. `info`(기본)는 기기 ID와 사건만, `debug`는 IP와 후보 주소까지 찍는다.
- `docker compose down`은 SIGINT를 보내고 서버는 연결을 닫고 끝난다.

## 방화벽

- 호스트 방화벽과 클라우드 보안 그룹에서 위 표의 UDP 포트를 모든 출발지에 연다. 기기는 아무 망에서나 붙는다.
- 같은 출발 주소의 STUN 요청은 초당 `stun_per_source_per_second`(기본 50)를 넘으면 답하지 않는다. 출발 주소를 속인 반사 공격을 막는 상한이다.
- 접속할 기기를 정해 두려면 아래 "접속할 기기 정하기"의 `allowlist`를 쓴다. 목록 밖 기기는 TLS 직후 `access_denied`로 닫힌다.

## 접속할 기기 정하기

`rendezvous.toml` 끝의 `allowlist` 예에서 주석을 풀면 목록 파일의 기기만 받는다. 목록 파일은 기기 ID(소문자 hex 64자)를 한 줄에 하나씩 적고, `#`으로 시작하는 줄과 빈 줄은 건너뛴다. `file`의 상대 경로는 설정 파일이 있는 폴더 기준이다.

컨테이너로 돌리면 설정 파일 하나만 `/etc/tasty-rendezvous/`에 붙어 있으므로 목록 파일도 같은 폴더에 읽기 전용으로 붙인다. 붙이지 않으면 `settings --config`와 서버 시작이 `allowlist file could not be read: No such file or directory`로 멈춘다. `compose.yaml`의 `volumes`에 주석으로 둔 줄을 푼다.

```yaml
    volumes:
      - ./rendezvous.toml:/etc/tasty-rendezvous/rendezvous.toml:ro
      - ./allowlist.txt:/etc/tasty-rendezvous/allowlist.txt:ro
```

- 컨테이너 안의 서버는 UID 10001(`rendezvous`)로 돈다. 붙인 파일의 권한은 호스트 파일 그대로이므로 UID 10001이 읽을 수 있어야 한다. 호스트 사용자 전용(0600) 파일이면 `Permission denied`로 멈춘다. 목록과 설정 파일은 비밀이 아니므로 `chmod 644`로 둔다.
- 목록을 고친 뒤에는 서버를 다시 띄운다. 서버는 시작할 때 한 번만 읽는다.

## 두 번째 IP

NAT 판별은 같은 IP의 두 포트로 포트 의존까지 본다. 주소 의존(주소마다 다른 매핑)까지 보려면 두 번째 공인 IP가 필요하다. 없어도 서버는 돈다. 띄우는 곳에 따라 설정이 다르다.

### 호스트에서 직접 띄울 때

1. 호스트에 두 번째 공인 IP를 붙인다.
2. `listen.primary`를 첫 IP(`"203.0.113.10:3478"`)로, `listen.alternate_ip`를 두 번째 IP(`"198.51.100.20"`)로 둔다. 서버가 두 번째 IP의 주 포트·보조 포트에도 소켓을 열고, 그 둘은 STUN만 받는다. 듣는 주소가 공인 IP 자체이므로 `advertise.*`는 필요 없다(호스트가 NAT 뒤면 `advertise.primary_ip`·`advertise.alternate_ip`에 바깥 주소를 적는다).
3. 두 IP의 UDP 3478·3479를 모두 연다.

### 컨테이너로 띄울 때

기본 브리지 네트워크의 컨테이너에는 호스트의 공인 IP가 없다. `ports`에 IP를 붙여 게시해도 그 IP가 컨테이너 안에 생기지 않는다. 그래서 `listen.primary`나 `listen.alternate_ip`에 호스트 IP를 적으면 서버가 `server socket could not be bound: Cannot assign requested address`로 시작하지 않는다. 방법은 둘이다.

- 호스트 네트워크: `compose.yaml`에서 `ports`를 지우고 `network_mode: host`를 둔다. 컨테이너가 호스트의 주소를 그대로 쓰므로 위 "호스트에서 직접 띄울 때"의 `listen.*` 설정이 그대로 된다. 서버가 응답의 출발 IP를 질의 대상 IP와 같게 낸다. 두 IP를 쓸 때 추천하는 방법이다.
- 브리지 그대로: 컨테이너는 wildcard(기본 `listen.primary`)로 듣고, `listen.alternate_ip`는 두지 않는다. `advertise.primary_ip`에 첫 IP를, `advertise.alternate_ip`에 두 번째 IP를 적고, `ports`는 IP 없는 게시 대신 두 IP의 두 포트를 하나씩 게시한다. IP 없는 게시(`"3478:3478/udp"`)는 모든 호스트 주소의 그 포트를 잡으므로 IP를 붙인 게시와 함께 둘 수 없다. 두 IP로 온 질의는 컨테이너의 같은 소켓으로 들어가고, 응답은 Docker의 주소 변환을 거쳐 질의받은 IP에서 나간다.

  ```yaml
      ports:
        - "203.0.113.10:3478:3478/udp"
        - "203.0.113.10:3479:3479/udp"
        - "198.51.100.20:3478:3478/udp"
        - "198.51.100.20:3479:3479/udp"
  ```

확인한 것과 하지 않은 것: 사설 IP 두 개가 있는 Linux 호스트(Docker 29.6)에서 세 경우를 띄우고, 같은 호스트에서 네 주소(두 IP × 두 포트)로 STUN Binding을 보냈다. 브리지에서 `listen.primary`에 호스트 IP를 적으면 위 오류로 시작하지 않았다. 호스트 네트워크와 `listen.alternate_ip`는 네 주소 모두 답했고 반사 주소가 보낸 소켓 주소와 같았다. 브리지에서 IP별 게시와 `advertise.alternate_ip`도 네 주소 모두 답했다. 공인 IP 두 개와 다른 망의 기기에서 본 반사 주소(브리지의 주소 변환 뒤에도 기기의 바깥 주소가 보이는지)는 확인하지 않았다. 같은 호스트에서 보낸 질의는 Docker가 출발 주소를 바꿔 이 확인에 쓸 수 없었다.

## 설정 검사

```bash
bash rendezvous/check-config.sh --self-test   # 서버 없이 검사 자체를 시험한다
bash rendezvous/check-config.sh               # 서버와 stun_probe 예제를 빌드해 예시 설정을 검사한다
```

`check-config.sh`는 예시 설정을 임시 폴더에 복사해(`key_dir`만 임시 폴더로) 서버 키 만들기, `settings --config` 출력, 주석의 기본값, 접근 정책 두 예를 본 뒤 서버를 띄워 주·보조 포트에 STUN Binding을 보낸다. 마지막 줄이 `rendezvous-check result=ok`면 통과다. 예시의 UDP 3478·3479가 비어 있어야 한다. STUN 확인에는 이 레포의 STUN 코덱을 쓰는 예제 `stun_probe`(`crates/tasty-net-rendezvous/examples/stun_probe.rs`)를 쓴다. 자기 서버에 같은 확인을 하려면 다음처럼 부른다.

```bash
cargo run --locked -p tasty-net-rendezvous --example stun_probe -- 203.0.113.10:3479 2000
# stun-probe server=203.0.113.10:3479 local=... mapped=<바깥에서 보인 주소>
```
