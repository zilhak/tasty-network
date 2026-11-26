# tasty-network 설계

이 폴더는 tasty-network 라이브러리의 설계 정본이다. 크레이트·타입·메시지 이름은 각 문서가 정본이고, 구현이 이름을 바꾸면 같은 커밋에서 해당 문서를 고친다.

## 표기

- `tasty:<경로>`는 [tasty](https://github.com/zilhak/tasty) 레포 커밋 698aa49e03 기준 경로다. 줄 번호도 같은 커밋 기준이다.
- iroh·iroh-relay 사실은 v1.5.0 기준이다. 구현 때 그 판의 소스로 다시 확인한다.

## 문서

| 문서 | 다루는 것 |
|---|---|
| [레포 경계와 tasty-ssh 의존 끊기](repo-boundary.md) | 이 레포가 가질 것과 tasty에 남을 것(파일 단위 표), `StreamOpenParams` 도메인 필드, tasty-ssh의 tasty 의존 4개를 끊는 방식 |
| [크레이트 구성](crates.md) | 크레이트 표(정본), 의존 방향, 외부 의존, 오류·동기·unsafe 규칙, 시험의 자리 |
| [전송 추상화, 채널, 역할](transport.md) | 전송 trait, 채널 대응, heartbeat, 읽기 실패 분류, 취소·닫기·회수, 동기·비동기 경계, 순서 계약 표, 역할 표, 요청 채널 |
| [handshake v2](handshake.md) | 버전 결정, 구버전 처리, SSH·P2P 흐름, 검사 순서, 필드, 거절 사유, 도메인 확장, bulk 자격 |
| [기기 키·토큰](identity.md) | 기기 키와 연결 토큰의 수명·저장·권한, 동시 첫 생성, TOFU |
| [P2P](p2p.md) | iroh API, endpoint, 주소와 페어링, 직접 경로 정책(1안), 펀칭 실험 조건과 결과 |
| [relay](relay.md) | relay 모드, 배포 형태, 접속 허용 규약, 클라이언트 기본 relay |
| [토폴로지](topology.md) | 노드·엣지 모델, 질의·결과 메시지, 탐색 규칙, 공개 범위 |
| [위협 모델](threat-model.md) | 신뢰 경계, 위협별 대응과 맡는 쪽 |
| [버전·호환](versioning.md) | 크레이트 semver, 태그, tasty가 쓰는 방식, 와이어 버전, CHANGELOG |
| [CI와 시험 환경](testing.md) | CI 러너, SSH 통합 시험의 sshd, P2P 시험의 relay |
| [동작 수준 기준](done-criteria.md) | "제대로 동작하는 수준" 기준과 판정 규칙, 판정 기록 |

## 결정 요약

| 주제 | 결정 | 문서 |
|---|---|---|
| tasty-ssh 의존 | i18n → 오류 enum + 영어 `Display`(a), utils → 이 레포에서 자체 구현, reaper → 자식 생성 훅 주입(b), remote-profiles → 값 타입만 두고 프로필 함수는 tasty에(c). 남는 tasty 의존 0 | [레포 경계](repo-boundary.md#tasty-ssh-역방향-의존-끊기) |
| 크레이트 | `tasty-net-{frame, ssh, identity, transport, p2p, topology}` + 시험·예제 `tasty-net-demo` + `relay/` | [크레이트 구성](crates.md) |
| 동기·비동기 | 소비자 API는 동기. P2P 크레이트가 tokio 런타임을 소유 | [전송 추상화](transport.md#동기비동기-경계) |
| 채널 | 순서 채널 하나(키 입력·PTY 출력·Control·mesh) + bulk 채널. mesh 분리는 하지 않음 | [전송 추상화](transport.md#순서-계약과-채널) |
| 역방향 attach | (b) `-L` 위 역할 반전. `-R`은 쓰지 않음 | [전송 추상화](transport.md#역할) |
| handshake | `proto` 2로 올림. 토큰 + 양방향 nonce 서명(SSH), TLS `remote_id()` + 페어링(P2P). v1 peer는 사유를 붙여 거절 | [handshake v2](handshake.md) |
| 포트 기반 조회·생성·상태 확인 | 인증된 요청 채널(`purpose = request`). SSH·P2P 모두 | [전송 추상화](transport.md#요청-채널-포트-기반-조회생성상태-확인) |
| 기본 relay | URL은 소비자(tasty) 설정, 라이브러리 상수 없음. 사용자 relay 우선. hub = 직접 경로 필수(1안) | [relay](relay.md#클라이언트-기본-relay) |
| CI | GitHub 호스트 러너 세 OS | [CI와 시험 환경](testing.md) |
| 버전 | workspace 버전 하나, 태그 `v<semver>`(main), tasty는 git + `rev` 고정, `CHANGELOG.md` | [버전·호환](versioning.md) |
| 동작 수준 | D1~D7(세 OS CI, SSH 정방향·역할 반전, P2P, relay, 원격 전체 시험, tasty 의존 없음). 펀칭 실험은 넣지 않음 | [동작 수준 기준](done-criteria.md) |

## 남은 결정

이 레포가 정하지 않는 것, 또는 사용자 확인이 필요한 것.

| 항목 | 상태 | 누구 몫 |
|---|---|---|
| 한쪽만 새 버전인 두 기기 사이 attach가 안 되는 점(v1 거절). 전환 공지와 업데이트 안내 방식 | 확인 필요 | 사용자 |
| hub relay의 접속 허용 범위(누구의 endpoint ID를 받을지) | 미정 | hub |
| 클라이언트 기본 relay URL(hub relay 주소) | 미정. hub relay 배포 때 정함 | hub, tasty |
| 펀칭 실험 기기·네트워크·공인 relay 준비와 일정 | 미정. 조건은 [P2P](p2p.md#필요한-조건) | 사용자 |
| 토폴로지 원격 응답에 실을 workspace 메타데이터 범위(이름·ID) | 미정 | tasty 제품 결정 |
| 페어링 메시지를 나르는 경로와 승인 화면 | 미정 | tasty 제품 결정 |
| 서로 다른 사용자 relay를 쓰는 두 기기에서 상대 홈 relay 경로의 정책. 지금 기본 `unknown_relay_policy = DirectRequired`라 직접 경로가 없으면 끊긴다. tasty가 사용자 relay를 설정했을 때 이 값을 `RelayAllowed`로 둘지([P2P](p2p.md#relay-정책과-직접-경로-감시-1안)) | 확인 필요 | 사용자, tasty 제품 결정 |
| `DirectRequired` relay에서 직접 경로가 확인될 때까지 앱 프레임을 보류할지(지금은 보류하지 않아 그동안 낮은 속도로 중계될 수 있다, [P2P](p2p.md#relay-정책과-직접-경로-감시-1안)) | 확인 필요. 1안의 뜻에 닿는다 | 사용자 |
