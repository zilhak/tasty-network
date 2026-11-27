# tasty-network 설계

이 폴더는 tasty-network 라이브러리의 설계 정본이다. 크레이트·타입·메시지 이름은 각 문서가 정본이고, 구현이 이름을 바꾸면 같은 커밋에서 해당 문서를 고친다.

## 표기

- `tasty:<경로>`는 [tasty](https://github.com/zilhak/tasty) 레포 커밋 698aa49e03 기준 경로다. 줄 번호도 같은 커밋 기준이다.
- quinn·rustls 사실은 2026-11-27에 확인한 quinn 0.11.12·rustls 0.23.46 기준이다. 구현 때 고정하는 판으로 다시 확인한다([P2P](p2p.md#외부-의존과-고정)).

## 문서

| 문서 | 다루는 것 |
|---|---|
| [레포 경계와 tasty-ssh 의존 끊기](repo-boundary.md) | 이 레포가 가질 것과 tasty에 남을 것(파일 단위 표), `StreamOpenParams` 도메인 필드, tasty-ssh의 tasty 의존 4개를 끊는 방식 |
| [크레이트 구성](crates.md) | 크레이트 표(정본), 의존 방향, P2P 크레이트 격리, 외부 의존, 오류·동기·설정값·unsafe 규칙, 시험의 자리 |
| [전송 추상화, 채널, 역할](transport.md) | 전송 trait, 채널 대응, heartbeat, 읽기 실패 분류, 취소·닫기·회수, 동기·비동기 경계, 순서 계약 표, 역할 표, 요청 채널, 세 경로의 연결 목록 |
| [handshake v2](handshake.md) | 버전 결정, 구버전 처리, SSH·P2P 흐름, 검사 순서, 필드, 거절 사유, 도메인 확장, bulk 자격 |
| [기기 키·토큰](identity.md) | 기기 키와 연결 토큰의 수명·저장·권한, 동시 첫 생성, TOFU |
| [P2P](p2p.md) | quinn + rendezvous 구조, 크레이트와 외부 의존, 소켓과 패킷 나누기, NAT 판별, 펀칭 절차, 흐름 표, 정책, 연결 유지, 재시도, 상태 기계, 설정값, 연결 정보, 인증, 어댑터와의 경계, 페어링, 펀칭 실험 |
| [rendezvous](rendezvous.md) | 중계 없음의 보장, 포트와 전송, 기기 인증, 메시지와 종료 코드, 펀칭 프로브, 서버 상태, 서버 설정값, 클라이언트 설정, 배포 형태, `relay/` 처리 방침 |
| [토폴로지](topology.md) | 노드·엣지 모델, 질의·결과 메시지, 탐색 규칙, 공개 범위 |
| [위협 모델](threat-model.md) | 신뢰 경계, 위협별 대응과 맡는 쪽 |
| [버전·호환](versioning.md) | 크레이트 semver, 태그, tasty가 쓰는 방식, 와이어 버전, CHANGELOG |
| [CI와 시험 환경](testing.md) | CI 러너, SSH 통합 시험의 sshd, P2P 시험(프로세스 안 서버, 모의 NAT, 크레이트 단독), 설정값 시험, 연결 목록 시험 |
| [동작 수준 기준](done-criteria.md) | "제대로 동작하는 수준" 기준과 판정 규칙, 판정 기록 |

## 결정 요약

| 주제 | 결정 | 문서 |
|---|---|---|
| tasty-ssh 의존 | i18n → 오류 enum + 영어 `Display`(a), utils → 이 레포에서 자체 구현, reaper → 자식 생성 훅 주입(b), remote-profiles → 값 타입만 두고 프로필 함수는 tasty에(c). 남는 tasty 의존 0 | [레포 경계](repo-boundary.md#tasty-ssh-역방향-의존-끊기) |
| 크레이트 | `tasty-net-{frame, ssh, identity, transport, p2p, rendezvous, topology}` + 시험·예제 `tasty-net-demo` + `rendezvous/`(지금의 `relay/`를 바꾼다) | [크레이트 구성](crates.md) |
| P2P 구조 | iroh를 쓰지 않는다. rendezvous 서버(반사·펀칭 조정만, 중계 코드 경로 없음) + 펀칭된 UDP 위 quinn QUIC. 인증은 기기 키 raw public key(RFC 7250) | [P2P](p2p.md), [rendezvous](rendezvous.md) |
| P2P 격리 | p2p·rendezvous는 identity만 쓴다(p2p → rendezvous). transport가 p2p를 어댑터로 쓰고 `P2pCloseReason`은 p2p가 정의한다 | [크레이트 구성](crates.md#p2p-크레이트-격리) |
| P2P 정책 | 기본 꺼짐, 켰을 때만 서버 연결, 거는 쪽도 켜야 함, 같은 서버의 기기끼리만(비교 로직 없음), 전역 discovery 없음, 펀칭 실패 = 연결 실패(중계 없음). Q3 보류는 중계 경로가 없어 저절로 충족 | [P2P](p2p.md#정책) |
| 연결 상태 | P2P는 상태 기계와 원인 포함 이벤트. 세 경로를 모은 연결 목록은 transport | [P2P](p2p.md#상태-기계), [전송 추상화](transport.md#연결-목록) |
| 설정값 | 모든 타이머·횟수·상한은 설정 가능하고 적용값·출처를 조회할 수 있다. 새 P2P 크레이트부터 적용, 기존 크레이트 가운데 identity·topology·ssh는 맞춤, 나머지는 후속 | [크레이트 구성](crates.md#설정값-규칙) |
| 동기·비동기 | 소비자 API는 동기. P2P 크레이트가 tokio 런타임을 소유 | [전송 추상화](transport.md#동기비동기-경계) |
| 채널 | 순서 채널 하나(키 입력·PTY 출력·Control·mesh) + bulk 채널. mesh 분리는 하지 않음 | [전송 추상화](transport.md#순서-계약과-채널) |
| 역방향 attach | (b) `-L` 위 역할 반전. `-R`은 쓰지 않음 | [전송 추상화](transport.md#역할) |
| handshake | `proto` 2로 올림. 토큰 + 양방향 nonce 서명(SSH), TLS raw public key로 확인한 기기 ID + 페어링(P2P). v1 peer는 사유를 붙여 거절. 마이그레이션 미지원 | [handshake v2](handshake.md) |
| 포트 기반 조회·생성·상태 확인 | 인증된 요청 채널(`purpose = request`). SSH·P2P 모두 | [전송 추상화](transport.md#요청-채널-포트-기반-조회생성상태-확인) |
| 기본 rendezvous 서버 | 주소·공개키는 소비자(tasty) 설정, 라이브러리 상수 없음. tasty 기본은 hub rendezvous, 사용자 서버를 지정하면 그 하나만 | [rendezvous](rendezvous.md#클라이언트-설정) |
| CI | GitHub 호스트 러너 세 OS | [CI와 시험 환경](testing.md) |
| 버전 | workspace 버전 하나, 태그 `v<semver>`(main), tasty는 git + `rev` 고정, `CHANGELOG.md` | [버전·호환](versioning.md) |
| 동작 수준 | D1~D10(세 OS CI, SSH 정방향·역할 반전, P2P, rendezvous 서버, 원격 전체 시험, tasty 의존 없음, P2P 격리, 연결 목록, 설정값 노출). 다른 망 펀칭 실험은 넣지 않음(P1) | [동작 수준 기준](done-criteria.md) |

## 남은 결정

이 레포가 정하지 않는 것, 또는 사용자 확인이 필요한 것.

| 항목 | 상태 | 누구 몫 |
|---|---|---|
| 한쪽만 새 버전인 두 기기 사이 attach가 안 되는 점(v1 거절) | 결정됨(2026-11-27): 마이그레이션 미지원, 허용 설정·호환 계층·안내 작업 없음. 거절 사유 문구로만 알린다 | 사용자 |
| 펀칭 실험 기기·망·공인 서버 준비와 일정 | 결정됨(2026-11-27, Q1): hub rendezvous 배포 때 준비, 그 전엔 같은 망·로컬 서버로만 확인. 조건은 [P2P](p2p.md#필요한-조건-hub-rendezvous-배포-때) | 사용자 |
| 다른 서버를 쓰는 두 기기, 직접 경로 전 데이터 보류(Q3·Q4) | 결정됨(2026-11-27): 같은 서버의 기기끼리만 연결되고 다른 서버면 `PeerNotFound`. 중계 경로가 없어 데이터는 언제나 직접 경로로만 간다([P2P](p2p.md#정책)) | 사용자 |
| hub rendezvous의 접근 정책(누구의 기기 ID를 받을지) | 미정 | hub |
| tasty 기본 rendezvous 서버 주소와 서버 공개키 | 미정. hub rendezvous 배포 때 정함 | hub, tasty |
| 판단필요: hub rendezvous에 두 번째 공인 IP를 둘지 | hub rendezvous 배포 때 결정(Q1). 두 번째 IP가 없으면 주소 의존 매핑 NAT를 가르지 못해 `independent_port_only`로 보고 시도한다(실패하면 연결 실패). 있으면 주소 의존 매핑 NAT를 `dependent`로 가른다. 두 기기가 모두 `dependent`일 때만 호스트 후보만 시도한다(한쪽만이면 모든 후보로 시도). 추천: 처음에는 IP 하나 + 포트 둘로 시작하고 펀칭 실험(P1)에서 주소 의존 매핑의 비율을 보고 정한다([P2P](p2p.md#nat-판별)) | hub, 사용자 |
| 판단필요: 서버 신뢰 방식 | hub rendezvous 배포 때 결정(Q1). 지금 설계는 서버 공개키 고정(raw public key)이다. 대안은 도메인 인증서(WebPKI)다. 키 고정은 공용 인증 기관에 기대지 않고 기기 사이와 같은 코드를 쓰지만, 서버 키를 바꾸면 tasty 설정의 공개키도 바꿔야 한다. 추천: 키 고정. 키 교체 절차(새 키를 미리 설정에 더해 두는 두 키 허용)는 hub rendezvous 배포 때 정한다([rendezvous](rendezvous.md#기기-인증)) | hub, tasty |
| 토폴로지 원격 응답에 실을 workspace 메타데이터 범위(이름·ID) | 미정 | tasty 제품 결정 |
| 페어링 메시지를 나르는 경로와 승인 화면 | 미정 | tasty 제품 결정 |
