# 토폴로지

크레이트는 `tasty-net-topology`다. 사용자 결정: 토폴로지는 두 방식이다. (1) 기기마다 둔 정의 파일의 상태 표시(오프라인 기기 포함), (2) 기준 머신에서 depth를 정해 살아 있는 연결을 탐색. 이 레포는 (2)의 형식·알고리즘과 (1)이 쓰는 노드 상태 타입을 맡는다. 정의 파일과 화면은 tasty 몫이다.

## 모델

| 타입 | 필드 |
|---|---|
| `Node` | `device_id`, `display_name`(선택), `status: NodeStatus`, `explored_at`(응답 받은 시각, Unix ms) |
| `NodeStatus` | `online` · `offline`(정의 파일 방식에서만) · `auth_rejected`(인증 필수라 연결이 거절된 구버전) · `no_capability`(연결은 되지만 상대 `caps`에 `net.topology`가 없음) · `timed_out` · `sharing_off`(공개 꺼짐) |
| `Edge` | `dialer: DeviceId`, `acceptor: DeviceId`, `holder: dialer \| acceptor`, `transport: TransportKind`, `rtt_ms`(선택), `connected_at`(Unix ms), `workspace`(선택, `WorkspaceRef{id, name}` — 공개 범위에 따라 빠짐) |
| `TransportKind` | `ssh_forward` · `ssh_role_reversed` · `p2p_direct` · `p2p_relay` |

- 엣지는 연결 방향(dialer → acceptor)과 holder를 따로 담는다. 역할 반전과 P2P에서 둘이 다르기 때문이다([전송 추상화](transport.md) "역할").
- 같은 두 기기 사이 attach가 여럿이면 엣지도 여럿이다. 같은 `(dialer, acceptor, holder, transport, workspace)`는 하나로 합친다.
- P2P 엣지의 `transport`와 `rtt_ms`는 선택 경로(`Path::is_ip`·`is_relay`·`rtt`)에서 온다.

## 엣지 목록 입력

엣지를 실제로 모으는 일은 tasty가 한다(점유 레지스트리, 연결 등록). 이 레포는 입력 형식 `LocalLink{peer, holder, transport, rtt, connected_at, workspace, kind}`와 집계 함수를 준다.

- `kind`가 `attach`인 인증된 링크만 엣지가 된다. bulk·요청 채널, debug echo 연결은 넣지 않는다.
- surface 단위 attach도 엣지다(`workspace`에는 그 surface가 속한 workspace).
- 재연결로 링크가 바뀌면 소비자가 이전 링크를 입력에서 뺀다. 집계는 입력 스냅샷만 본다.

## 질의와 결과 메시지

질의는 인증된 attach 링크의 순서 채널로만 오간다. tasty Control 밖 raw JSON `event` 태그 확장(`tasty:docs/dev-guide/attach-behavior.md` "커스텀 이벤트 확장") 자리에 싣는다.

| 메시지 | `event` 값 | 필드 |
|---|---|---|
| 질의 | `net.topology.query` | `request_id`, `origin`(기준 머신), `depth`(남은 depth), `visited`(본 기기 ID 목록), `relay_hops_left`(기준 머신까지 전달 hop 상한), `deadline_ms`(전달 구간 전체 마감, Unix ms) |
| 결과 | `net.topology.result` | `request_id`, `part`, `last`(bool), `nodes`, `edges` |

- 상대가 `caps`에 `net.topology`를 알리지 않았으면 질의를 보내지 않고 그 노드를 `no_capability`로 둔다. tasty는 모르는 event를 조용히 버리므로(`tasty:crates/tasty-ipc/src/stream_hub.rs` `collect_other_control`) 보내 봐야 답이 오지 않는다.
- 결과 프레임 하나의 JSON은 `MAX_FRAME_LEN / 2` 이하다(tasty 원격 읽기 결과 규칙, `tasty:docs/dev-guide/attach-behavior.md` "mirror 터미널 읽기"). 넘으면 `part`로 나누고 마지막 조각에 `last: true`.

## 탐색 규칙

- 기준 머신이 질의를 받은 기기 자신이 아니면 기준 머신까지 전달한다. 그 전달 hop은 depth에 세지 않고 `relay_hops_left`를 1씩 줄인다. 0이 되거나 `deadline_ms`가 지나면 `timed_out`으로 끝낸다.
- 기준 머신부터는 hop마다 depth를 1 줄인다. depth 0인 노드는 자기 엣지만 답하고 더 전달하지 않는다.
- 들어오는 링크와 나가는 링크를 모두 따라간다.
- `visited`에 있는 기기에는 다시 보내지 않는다. 순환(A↔B)이 끝난다.
- 더 짧은 경로로 같은 노드에 다시 닿아 남은 depth가 더 크면, 그 남은 depth로 그 노드부터 이어 탐색한다(다이아몬드에서 긴 경로가 먼저 닿은 경우).
- 이웃마다 응답 시간 제한을 둔다. 넘으면 그 노드를 `timed_out`으로 두고 탐색은 계속한다.
- 결과는 도착하는 대로 점진적으로 낸다. 노드에는 응답 시각을 붙인다.
- 탐색은 "이웃에게 질의를 보내는 함수"를 주입받는 상태 기계다. 그래서 전송 없이 가짜 이웃으로 시험한다.

## 공개 범위

사용자 결정: 공개 범위는 기기 단위 설정 하나다. 설정이 없으면 제공한다. 끄면 질의에 자기 자신만 답하는 말단 노드가 된다.

- 원격 질의 응답: 공개 꺼짐이면 자기 노드 하나(`sharing_off`)만 답하고 전달하지 않는다. 공개 켜짐이어도 `workspace` 메타데이터는 소비자가 정한 범위(이름·ID 중 무엇을 내보내는가)만 싣는다.
- 로컬 조회(자기 화면): 공개 설정과 관계없이 자기 엣지를 모두 보여 준다.
- 두 범위를 함수로 나눈다: `local_view(links)`와 `remote_answer(links, sharing)`. 설정을 저장하는 일은 tasty 몫이다.
- 응답 측 공통 함수 `answer_query`가 질의를 받아 자기 엣지를 답하고 다음 hop으로 전달한다. GUI·headless 배선은 tasty가 한다.
