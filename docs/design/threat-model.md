# 위협 모델

`tasty:<경로>`는 tasty 레포 698aa49e03 기준이다.

## 신뢰 경계

- SSH 경로의 신뢰 경계는 SSH다. 상대 머신에 SSH로 들어갈 수 있는 사용자는 그 머신의 tasty에도 붙을 수 있다고 본다. 이 레포는 그 위에 새 권한 게이트를 만들지 않는다.
- 그 대신 "SSH로 들어오지 않았는데 붙을 수 있는 길"을 막는다. 루프백 포트가 그 길이다.
- P2P 경로의 신뢰 경계는 페어링이다. 양쪽 사람이 승인한 기기 ID만 받는다.

## 위협과 대응

| # | 위협 | 대응 | 맡는 쪽 |
|---|---|---|---|
| T1 | 연결 토큰 탈취 | 토큰은 인스턴스마다 새로 발급하고 인스턴스가 끝나면 지운다. 파일 권한은 Unix 0600·디렉터리 0700, Windows 현재 SID 하나. 로그에 쓰지 않는다. 토큰을 훔친 쪽은 SSH 없이 그 인스턴스에 붙을 수 있으므로 토큰 보호가 이 경로의 핵심이다. 붙은 기기 ID는 서명으로 확인된 값으로 받는 쪽 소비자에게 전달되므로(`Link::peer()`) 기록·표시할 수 있다 | 이 레포(발급·비교·권한), tasty(파일 위치와 삭제) |
| T2 | 루프백 포트에 붙는 같은 머신의 다른 OS 사용자. `-L`은 dialer 머신의 루프백에, 지금의 tasty 서버는 자기 루프백에 포트를 연다. `-R`은 원격 루프백에 포트를 연다 | 모든 `stream.open`에 토큰을 요구한다. 토큰 파일은 그 사용자만 읽는다. 역방향은 `-R`을 쓰지 않는다(역할 반전, [전송 추상화](transport.md) "역할") | 이 레포 |
| T3 | 포워딩 포트가 일반 JSON-RPC를 노출한다. 같은 포트에서 `stream.open`이 아닌 첫 줄은 요청·응답 루프로 가고(`tasty:src/adapters/production/tcp_ipc_server.rs` `handle_connection`), 토큰 없는 요청은 모든 메서드가 허용된다(`tasty:crates/tasty-ipc/src/caller.rs`) | 이 레포는 인증된 요청 채널(`purpose = request`)과, 첫 줄이 `stream.open`이 아닐 때 소비자가 바로 닫을 수 있는 받는 쪽 진입을 준다. 포워딩 대상 포트를 attach·요청 채널 전용으로 좁히는 일(별도 listener 또는 비인증 요청 거절)은 tasty가 한다 | tasty(포트 좁히기), 이 레포(대체 경로) |
| T4 | 상대 기기 위장(같은 이름의 다른 머신, 바뀐 키) | SSH 경로는 양방향 nonce 서명과 TOFU, P2P는 TLS의 `remote_id()`와 페어링 목록 | 이 레포 |
| T5 | relay가 보는 것 | relay는 QUIC 암호문만 중계한다. 내용은 보지 못한다. 접속한 endpoint ID, 접속 시각, 주고받은 양, 양쪽 IP 주소는 안다. hub relay는 `punch-only`이고 클라이언트는 `DirectRequired`라, 직접 경로를 기다리는 동안 낮은 속도로 중계되고 그 뒤에는 끊긴다. 중계 양이 0은 아니다([P2P](p2p.md#relay-정책과-직접-경로-감시-1안)). 사용자 relay는 사용자가 운영한다 | 문서로 알림 |
| T6 | bulk 채널을 다른 세션이 가로챔 | bulk 자격을 부모 링크와 grant에 묶는다. 부모가 끝나면 자격을 폐기하고(`bulk_credential_invalid`) 그 부모의 열린 bulk 채널도 이 레포가 닫는다. grant가 바뀌면 자격은 그대로이고 새 bulk 요청을 acceptor의 도메인 콜백이 `domain`으로 거절한다. 그때 이미 열린 bulk 채널은 이 레포가 끊지 않고 tasty의 완료 인가가 막는다([handshake v2](handshake.md) "bulk 자격") | 이 레포, 완료 인가는 tasty |
| T7 | `attach.acquire`의 `client_id` 위조(`tasty:src/adapters/ipc/handler/attach.rs`) | 점유를 연결에 묶는 일은 tasty 도메인이다. T3에서 일반 JSON-RPC를 좁히면 원격에서는 닿지 않는다. 로컬 호출자의 위조는 tasty가 별도로 막는다 | tasty |
| T8 | handshake 자원 고갈(인증 전 연결을 많이 열어 둠, 아주 긴 첫 줄) | 첫 줄 크기 상한과 기한(SSH 경로는 첫 줄을 읽는 소비자, P2P는 이 레포), 첫 줄 뒤 ACK까지 `LinkTimeouts.handshake_timeout`, 동시 handshake 상한. 역할 반전 승인 대기(`role_proposal_timeout`)는 토큰·서명을 통과한 연결에만 생긴다. 동시 handshake 상한을 넘으면 `busy`, 첫 줄이 `MAX_HANDSHAKE_LINE`을 넘으면 `malformed`(SSH 경로에서 소비자의 첫 줄 상한을 넘으면 소비자가 거절하고 닫는다, [전송 추상화](transport.md) "받는 쪽 진입") | 이 레포, tasty(SSH 첫 줄) |
| T9 | 손상되거나 바뀐 기기 키 파일 | 손상이면 오류로 멈추고 새로 만들지 않는다. 바뀐 키는 상대 쪽 TOFU에서 `peer_key_mismatch`로 드러난다 | 이 레포 |

## 이 레포가 하지 않는 것

- 원격 사용자 계정 관리, SSH 키 관리, hub 로그인.
- tasty 메서드별 권한 판단.
- 메모리 안 비밀 지우기(zeroize)는 하지 않는다. 같은 OS 사용자의 메모리 읽기는 위협 범위 밖이다.
