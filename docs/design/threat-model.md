# 위협 모델

`tasty:<경로>`는 tasty 레포 698aa49e03 기준이다.

## 신뢰 경계

- SSH 경로의 신뢰 경계는 SSH다. 상대 머신에 SSH로 들어갈 수 있는 사용자는 그 머신의 tasty에도 붙을 수 있다고 본다. 이 레포는 그 위에 새 권한 게이트를 만들지 않는다.
- 그 대신 "SSH로 들어오지 않았는데 붙을 수 있는 길"을 막는다. 루프백 포트가 그 길이다.
- P2P 경로의 신뢰 경계는 페어링이다. 양쪽 사람이 승인한 기기 ID만 받는다. rendezvous 서버는 신뢰 경계 안에 있지 않다. 서버가 거짓 후보를 주더라도 기기 사이 QUIC의 raw public key 확인이 상대를 가린다.

## 위협과 대응

| # | 위협 | 대응 | 맡는 쪽 |
|---|---|---|---|
| T1 | 연결 토큰 탈취 | 토큰은 인스턴스마다 새로 발급하고 인스턴스가 끝나면 지운다. 파일 권한은 Unix 0600·디렉터리 0700, Windows 현재 SID 하나. 로그에 쓰지 않는다. 토큰을 훔친 쪽은 SSH 없이 그 인스턴스에 붙을 수 있으므로 토큰 보호가 이 경로의 핵심이다. 붙은 기기 ID는 서명으로 확인된 값으로 받는 쪽 소비자에게 전달되므로(`Link::peer()`) 기록·표시할 수 있다 | 이 레포(발급·비교·권한), tasty(파일 위치와 삭제) |
| T2 | 루프백 포트에 붙는 같은 머신의 다른 OS 사용자. `-L`은 dialer 머신의 루프백에, 지금의 tasty 서버는 자기 루프백에 포트를 연다. `-R`은 원격 루프백에 포트를 연다 | 모든 `stream.open`에 토큰을 요구한다. 토큰 파일은 그 사용자만 읽는다. 역방향은 `-R`을 쓰지 않는다(역할 반전, [전송 추상화](transport.md) "역할") | 이 레포 |
| T3 | 포워딩 포트가 일반 JSON-RPC를 노출한다. 같은 포트에서 `stream.open`이 아닌 첫 줄은 요청·응답 루프로 가고(`tasty:src/adapters/production/tcp_ipc_server.rs` `handle_connection`), 토큰 없는 요청은 모든 메서드가 허용된다(`tasty:crates/tasty-ipc/src/caller.rs`) | 이 레포는 인증된 요청 채널(`purpose = request`)과, 첫 줄이 `stream.open`이 아닐 때 소비자가 바로 닫을 수 있는 받는 쪽 진입을 준다. 포워딩 대상 포트를 attach·요청 채널 전용으로 좁히는 일(별도 listener 또는 비인증 요청 거절)은 tasty가 한다 | tasty(포트 좁히기), 이 레포(대체 경로) |
| T4 | 상대 기기 위장(같은 이름의 다른 머신, 바뀐 키, 서버가 준 거짓 후보 주소) | SSH 경로는 양방향 nonce 서명과 TOFU. P2P는 TLS 1.3 raw public key로 확인한 상대 기기 ID가 요청한 ID와 같은지(dialer)와 페어링 목록(acceptor). 다르면 `PeerIdentityMismatch`·`not_paired`로 끝난다([P2P](p2p.md#인증)) | 이 레포 |
| T5 | rendezvous 서버가 보는 것 | 서버는 기기 사이 데이터를 보지도 나르지도 않는다. 중계 코드 경로가 없다([rendezvous](rendezvous.md#중계-없음)). 그래서 handshake와 attach 데이터는 언제나 기기 사이 직접 QUIC으로만 간다. 서버가 아는 것: 등록한 기기 ID, 접속 시각, 반사 주소와 호스트 후보 주소(사설 주소 포함), NAT 판별 결과, 누가 누구에게 펀칭을 요청했는지와 결과. 메모리에만 두고 기본 로그에는 주소를 남기지 않는다. 서버가 갖지 않는 것: 데이터, 연결 토큰 원문, 페어링 목록, 개인키. 사용자 서버는 사용자가 운영한다 | 이 레포(구조), 문서로 알림 |
| T6 | bulk 채널을 다른 세션이 가로챔 | bulk 자격을 부모 링크와 grant에 묶는다. 부모가 끝나면 자격을 폐기하고(`bulk_credential_invalid`) 그 부모의 열린 bulk 채널도 이 레포가 닫는다. grant가 바뀌면 자격은 그대로이고 새 bulk 요청을 acceptor의 도메인 콜백이 `domain`으로 거절한다. 그때 이미 열린 bulk 채널은 이 레포가 끊지 않고 tasty의 완료 인가가 막는다([handshake v2](handshake.md) "bulk 자격") | 이 레포, 완료 인가는 tasty |
| T7 | `attach.acquire`의 `client_id` 위조(`tasty:src/adapters/ipc/handler/attach.rs`) | 점유를 연결에 묶는 일은 tasty 도메인이다. T3에서 일반 JSON-RPC를 좁히면 원격에서는 닿지 않는다. 로컬 호출자의 위조는 tasty가 별도로 막는다 | tasty |
| T8 | handshake 자원 고갈(인증 전 연결을 많이 열어 둠, 아주 긴 첫 줄) | 첫 줄 크기 상한과 기한(SSH 경로는 첫 줄을 읽는 소비자, P2P는 이 레포), 첫 줄 뒤 ACK까지 `LinkTimeouts.handshake_timeout`, 동시 handshake 상한. 역할 반전 승인 대기(`role_proposal_timeout`)는 전송별 인증(SSH는 토큰·서명, P2P는 페어링 목록에 있는 TLS 확인 기기 ID)을 통과한 연결에만 생긴다. 동시 handshake 상한을 넘으면 `busy`, 첫 줄이 `MAX_HANDSHAKE_LINE`을 넘으면 `malformed`(SSH 경로에서 소비자의 첫 줄 상한을 넘으면 소비자가 거절하고 닫는다, [전송 추상화](transport.md) "받는 쪽 진입") | 이 레포, tasty(SSH 첫 줄) |
| T9 | 손상되거나 바뀐 기기 키 파일 | 손상이면 오류로 멈추고 새로 만들지 않는다. 바뀐 키는 상대 쪽 TOFU에서 `peer_key_mismatch`로 드러난다 | 이 레포 |
| T10 | rendezvous 서버를 통한 주소 수집·남용(남의 기기 ID로 펀칭을 요청해 IP를 얻음, 요청 폭주, STUN 반사 공격) | 받는 쪽이 페어링 목록으로 수락하기 전에는 서버가 양쪽 주소를 서로에게 주지 않는다. 기기는 자기 키로만 등록할 수 있다(TLS로 기기 ID를 정한다). 기기별 요청 속도·동시 펀칭 상한, 출발 주소별 STUN 속도 상한, 요청이 온 주소로만 응답. STUN 응답은 요청보다 크다(요청 20바이트, 응답 IPv4 32·IPv6 44바이트)라 출발 주소를 속인 요청으로 제3자에게 1.6–2.2배의 응답을 보내게 하는 반사 공격이 가능하다. 속도 상한이 그 양을 묶는다([rendezvous](rendezvous.md#포트와-전송)). 기기 사이 QUIC은 펀칭 세션에서 확인한 주소와 상대에만 열린다([P2P](p2p.md#펀칭-절차) 9). 받는 쪽의 존재 여부는 `declined`로 드러나지만 주소는 드러나지 않는다 | 이 레포, 접근 정책은 서버 운영자 |

## 이 레포가 하지 않는 것

- 원격 사용자 계정 관리, SSH 키 관리, hub 로그인.
- tasty 메서드별 권한 판단.
- 메모리 안 비밀 지우기(zeroize)는 하지 않는다. 같은 OS 사용자의 메모리 읽기는 위협 범위 밖이다.
