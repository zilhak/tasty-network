# handshake v2

`tasty:<경로>`는 tasty 레포 698aa49e03 기준이다. 필드 이름·사유 이름은 이 문서가 정본이다. 정확한 바이트 형식과 서명 대상의 인코딩은 `tasty-net-transport`의 `handshake.rs`와 그 시험이 정본이다.

## 지금(v1)의 형식

- dialer가 JSON 한 줄을 보낸다: `{"jsonrpc":"2.0","id":1,"method":"stream.open","params":{"proto":1,...}}`. `params`는 `StreamOpenParams{proto, target, target_workspace, bulk_workspace}`(`tasty:crates/tasty-ipc/src/stream.rs:199`).
- 그 뒤로는 프레임이다. 서버의 ACK는 Control 프레임 안의 `StreamAck{ok, client_id, proto, error}` JSON이다(`stream.rs:263`).
- 서버는 `proto == STREAM_PROTO`만 받는다(`tasty:src/adapters/production/tcp_ipc_server.rs` `validate_stream_proto`). 잘못된 params는 파싱 실패로 버려져 `proto = 0`이 되고 "버전 불일치"로 거절된다(`parse_stream_handshake`).
- 토큰·기기 확인이 없다. 클라이언트는 `session_token: None`으로 연다(`tasty:crates/tasty-ipc/src/client/stream.rs:119`).

## 버전 결정: `proto`를 2로 올린다

- 인증을 넣는 변경은 기능 추가가 아니다. v1 peer는 토큰을 보낼 수 없으므로 v2 받는 쪽이 v1 요청을 받아 주면 인증이 무의미해진다. 그래서 capability 협상이 아니라 `proto` 값을 2로 올린다.
- 이후 기능 추가는 `proto`를 올리지 않고 capability 목록(`caps`)으로 협상한다. `proto`는 handshake나 프레임 형식이 호환되지 않게 바뀔 때만 올린다([버전·호환](versioning.md)).
- 프레임 형식(헤더 5바이트, tag 값, 상한)은 v1과 같다.

## 구버전 처리

| 상황 | 결과 |
|---|---|
| v2 받는 쪽이 `proto: 1` 요청을 받음 | `LegacyProtocol` 사유로 거절 ACK를 보내고 닫는다. 점유·등록 전에 끝난다. 설정으로 허용하는 길은 두지 않는다 |
| v2 받는 쪽이 `proto`가 없거나 정수가 아닌 요청을 받음 | `Malformed` |
| v2 받는 쪽이 `proto`가 3 이상인 요청을 받음 | `UnsupportedProtocol`(ACK에 자기 `proto`를 싣는다) |
| v2 dialer가 v1 받는 쪽에 연결 | v1 서버가 `StreamAck{ok:false, proto:1, error}`를 보낸다. v2 dialer는 이 형태를 알아보고 `PeerTooOld`(로컬 오류)로 끝낸다. 다시 걸지 않는다 |

v2의 거절 ACK도 `ok`·`proto`·`error` 필드를 가진다. 그래서 v1 클라이언트가 v2 서버에 붙으면 지금처럼 "unsupported stream proto" 형태의 거절을 읽는다.

tasty 전환 뒤에는 한쪽만 새 버전인 두 기기 사이 attach가 되지 않는다. 사용자에게는 거절 사유로 "상대를 업데이트하라"는 안내가 나간다(문구는 tasty 몫). 이 점은 [남은 결정](index.md#남은-결정)에 적었다.

## 흐름 (SSH 경로)

메시지 1은 JSON 한 줄, 메시지 2부터는 Control 프레임 안의 JSON이다. 받는 쪽이 첫 줄로 `stream.open`인지 판정하는 지금 구조를 그대로 쓴다.

| 순서 | 방향 | 메시지 | 받는 쪽이 하는 검사 |
|---|---|---|---|
| 1 | dialer → acceptor | `stream.open` 줄. `params = {proto:2, purpose, token, device_id, instance_id, nonce, holder, caps, ext}` | `proto`·형식 검사, 토큰 상수 시간 비교. 실패하면 바로 거절 ACK |
| 2 | acceptor → dialer | `challenge = {device_id, instance_id, nonce, caps, sig}`. `sig`는 받는 쪽 키로 서명 대상(아래)에 서명한 값 | dialer: 서명 검증, TOFU 판정, 자기 인스턴스 비교 |
| 3 | dialer → acceptor | `proof = {sig}`. dialer 키로 서명 대상에 서명한 값 | 서명 검증, TOFU 판정, 자기 인스턴스 비교, 역할 검사 |
| 4 | acceptor → dialer | `ack = {ok, proto, reason?, error?, client_id?, bulk_credential?, ext?}` | — |

- 서명 대상은 도메인 구분 문자열(`tasty-net handshake v2`), `proto`, `purpose`, 양쪽 기기 ID, 양쪽 인스턴스 ID, 양쪽 nonce(32바이트 OS 난수), `holder`, `ext`의 바이트를 이어 붙인 값의 SHA-256이다. 양쪽이 같은 대상에 서명하되 서명자 표지(`dialer`/`acceptor`)를 넣어 서로의 서명을 되쓰지 못하게 한다. 그래서 두 기기 ID와 역할과 도메인 요청이 이 채널 하나에 묶인다.
- 받는 쪽은 토큰 검사를 통과하기 전에는 challenge를 보내지 않는다. 토큰 없는 상대는 기기 ID조차 받지 못한다.
- 4번 ACK가 `ok:true`이면 양쪽이 일반 프레임 흐름으로 넘어간다. 4번 뒤 프레임의 첫 Control은 지금처럼 도메인 descriptor다(O2, [전송 추상화](transport.md) "순서 계약과 채널").

## 흐름 (P2P 경로)

- iroh 연결은 TLS로 양쪽 endpoint ID를 확인한다. 받는 쪽은 `Connection::remote_id()`가 페어링 허용 목록에 있는지 QUIC 연결을 받은 직후에 본다. 없으면 handshake 전에 닫는다(`NotPaired`).
- 메시지 1의 `token`·`nonce`와 메시지 2·3의 서명은 없다. 기기 ID는 `remote_id()`로 정한다. 메시지 1의 `device_id`가 `remote_id()`와 다르면 `Malformed`.
- 그래서 P2P 흐름은 1 → (challenge 대신 받는 쪽 정보 `hello = {device_id, instance_id, caps}`) → 4다. 메시지 1·hello·4의 필드 이름은 SSH 경로와 같다.

## 검사 순서

받는 쪽 `accept_handshake`는 아래 순서를 API 형태로 강제한다. 소비자의 도메인 콜백은 앞의 검사가 모두 통과한 뒤에만 불린다. 점유·hub 등록은 도메인 콜백 안에서 하므로 인증 전에 일어나지 않는다.

1. 형식·`proto`(`Malformed`·`LegacyProtocol`·`UnsupportedProtocol`)
2. 토큰(SSH 경로, `TokenMissing`·`TokenInvalid`) 또는 페어링 목록(P2P, `NotPaired`)
3. 서명(SSH 경로, `SignatureInvalid`)
4. TOFU(`PeerKeyMismatch`). 소비자가 `PeerKeyStore`를 준 쪽만 본다. tasty dialer는 원격 프로필 이름으로 상대 키를 기록한다. 받는 쪽은 상대를 부를 이름이 없으므로 보통 주지 않고, 확인된 상대 기기 ID를 `Link::peer()`로 받아 기록·표시한다
5. 자기 자신(`SelfAttach`)
6. 역할(`RoleRefused`)
7. bulk 자격(`purpose = bulk`일 때, `BulkCredentialInvalid`)
8. 소비자 도메인 콜백(`Domain`)

dialer 쪽도 2번 메시지를 받은 뒤 서명 → TOFU → 자기 자신 순으로 보고, 실패하면 3번을 보내지 않고 닫는다.

## 필드

| 필드 | 메시지 | 뜻 |
|---|---|---|
| `proto` | 1, 4 | 2 |
| `purpose` | 1 | `attach` · `bulk` · `request` |
| `token` | 1 | 받는 쪽 인스턴스의 연결 토큰, 64자 소문자 hex([기기 키·토큰](identity.md)). SSH 경로만 |
| `device_id` | 1, 2 | 기기 ID 문자열 |
| `instance_id` | 1, 2 | 인스턴스 ID(소비자가 준 불투명 문자열). 자기 자신 판정용 |
| `nonce` | 1, 2 | 32바이트 난수의 hex. SSH 경로만 |
| `holder` | 1 | `dialer` · `acceptor`. `purpose = attach`일 때만 |
| `caps` | 1, 2 | 이 레포 수준 capability 이름 목록(예: `net.request`, `net.topology`). 모르는 이름은 무시 |
| `ext` | 1, 4 | 도메인 확장. JSON 객체, 이 레포는 해석하지 않는다 |
| `sig` | 2, 3 | ed25519 서명의 hex |
| `ok`, `error` | 4 | v1 `StreamAck`과 같은 뜻. `error`는 영어 설명(사람이 읽을 로그용) |
| `reason` | 4 | 거절 사유(아래). `ok:false`일 때 |
| `client_id` | 4 | 받는 쪽이 붙인 연결 번호. tasty가 `ext`가 아니라 여기에 싣는 것은 v1과의 모양 호환 때문이다 |
| `bulk_credential` | 4 | `purpose = attach` 성공 ACK에만. 아래 "bulk 자격" |

## 거절 사유

거절 사유는 enum `RejectReason`이다. 와이어에는 snake_case 문자열로 싣는다. 모르는 값은 `Unknown(String)`으로 읽는다. 문구는 소비자가 만든다.

| 값 | 뜻 | 다시 걸어도 되는가(`is_retryable`) |
|---|---|---|
| `malformed` | 메시지 형식 오류, 필드 타입 틀림 | 아니오 |
| `legacy_protocol` | 상대가 v1(토큰 없음) | 아니오 |
| `unsupported_protocol` | `proto` 값이 다름 | 아니오 |
| `token_missing` | 토큰이 없음 | 아니오 |
| `token_invalid` | 토큰이 틀림 | 아니오 |
| `not_paired` | P2P 허용 목록에 없음 | 아니오 |
| `signature_invalid` | 서명 검증 실패 | 아니오 |
| `peer_key_mismatch` | TOFU 기록과 다른 기기 키 | 아니오 |
| `self_attach` | 같은 인스턴스끼리 연결 | 아니오 |
| `role_refused` | 받는 쪽이 요청한 역할을 받지 않음(역방향 제안 거절 포함) | 아니오 |
| `bulk_credential_invalid` | bulk 자격이 없거나 폐기됨 | 아니오 |
| `domain` | 소비자 도메인 콜백이 거절. `ext`에 소비자 코드(예: `conflicting_targets`)를 싣는다 | 소비자가 정함 |
| `busy` | 받는 쪽이 지금 받을 수 없음(동시 handshake 상한 등) | 예 |

시간 초과와 취소는 와이어 사유가 아니라 로컬 오류(`HandshakeError::TimedOut`·`Cancelled`)다.

## 도메인 확장

- `ext`는 소비자가 채우고 소비자가 검사한다. tasty는 `target`·`target_workspace`·`bulk_workspace`를 `ext`에 그대로 싣는다.
- 소비자 도메인 콜백은 `ext`를 받아 수락(ACK의 `ext`와 bulk grant를 돌려줌)이나 거절(`domain` 사유와 코드)을 돌려준다.
- tasty 쪽 계약: `target`·`target_workspace`·`bulk_workspace`는 서로 배타다. 둘 이상이면 `domain` + `conflicting_targets`로 거절한다([레포 경계](repo-boundary.md) "`StreamOpenParams` 도메인 필드").

## bulk 자격

인스턴스 공용 토큰과 기기 ID만으로는 같은 기기의 두 세션을 구분하지 못한다. 그래서 bulk 채널을 부모 attach 링크에 묶는다.

- `purpose = attach` 성공 ACK에 `bulk_credential`(32바이트 난수의 hex)을 싣는다. 받는 쪽 라이브러리는 자격 → (부모 링크, grant)를 기억한다. grant는 소비자 도메인 콜백이 돌려준 불투명 바이트다(tasty는 workspace ID와 점유 세대를 넣는다).
- bulk 채널의 메시지 1은 `purpose = bulk`, 토큰, `ext.bulk_credential`을 싣는다. SSH 경로에서도 서명은 다시 하지 않는다. 자격이 인증된 부모 링크에서 나왔기 때문이다.
- 부모 링크가 끝나면 그 자격은 폐기된다. 소비자가 grant를 바꾸면(`Link::replace_grant`) 이전 자격은 폐기되고 새 자격이 나간다. 폐기된 자격의 bulk 요청은 `bulk_credential_invalid`.
- 열려 있는 bulk 채널이 쓰던 자격이 폐기돼도 그 채널을 이 레포가 끊지는 않는다. 완료 인가는 tasty가 commit 시점에 확인한다(`tasty:src/remote/server.rs` bulk 완료).

## capability

- `caps`는 이 레포 수준의 기능만 담는다: `net.request`(요청 채널), `net.topology`(토폴로지 질의). attach 도메인 capability(`ipc.stream.*`)는 지금처럼 tasty descriptor에 둔다.
- 상대가 모르는 capability는 무시한다. 기능을 쓰기 전에 상대의 `caps`를 본다.
