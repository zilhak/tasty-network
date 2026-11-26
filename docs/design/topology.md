# 토폴로지

크레이트는 `tasty-net-topology`다. 사용자 결정: 토폴로지는 두 방식이다. (1) 기기마다 둔 정의 파일의 상태 표시(오프라인 기기 포함), (2) 기준 머신에서 depth를 정해 살아 있는 연결을 탐색. 이 레포는 (2)의 형식·알고리즘과 (1)이 쓰는 노드 상태 타입을 맡는다. 정의 파일과 화면은 tasty 몫이다.

## 모델

| 타입 | 필드 |
|---|---|
| `Node` | `device_id`(모르면 빈 값), `display_name`(선택), `status: NodeStatus`, `reason`(선택, 예: `route_broken`. 응답이 온전히 오지 않았으면 `incomplete`), `reported_by`(이 노드 상태를 만든 기기, 선택), `peer_hint`(소비자가 준 표시 이름, 선택), `explored_at`(응답 받은 시각, Unix ms, 표시용. 마감 판정에 쓰지 않는다) |
| `NodeStatus` | `online` · `offline`(정의 파일 방식에서만) · `auth_rejected`(인증 단계에서 연결이 거절됨. 인증을 못 하는 구버전 포함) · `no_capability`(연결은 되지만 상대 `caps`에 `net.topology`가 없음) · `timed_out` · `sharing_off`(공개 꺼짐) |
| `Edge` | `dialer: DeviceId`, `acceptor: DeviceId`, `holder: dialer \| acceptor`, `transport: TransportKind`, `rtt_ms`(선택), `connected_at`(Unix ms), `workspace`(선택, `WorkspaceRef{id, name}` — 공개 범위에 따라 빠짐) |
| `TransportKind` | `ssh_forward` · `ssh_role_reversed` · `p2p_direct` · `p2p_relay` |

- 엣지는 연결 방향(dialer → acceptor)과 holder를 따로 담는다. 역할 반전과 P2P에서 둘이 다르기 때문이다([전송 추상화](transport.md) "역할").
- 같은 두 기기 사이 attach가 여럿이면 엣지도 여럿이다. 같은 `(dialer, acceptor, holder, transport, workspace)`는 하나로 합친다.
- P2P 엣지의 `transport`와 `rtt_ms`는 선택 경로(`Path::is_ip`·`is_relay`·`rtt`)에서 온다.
- SSH 엣지의 `transport`는 링크의 holder에서 온다. transport `peer()`의 전송 종류가 `Ssh`·`Tcp`이고 holder가 dialer면 `ssh_forward`, acceptor면 `ssh_role_reversed`다(위 "역할" 표). 메모리 전송 링크는 시험용이라 엣지가 아니다. 대응표는 `tasty_net_topology::TransportKind` 문서 주석에 있다.
- 기기가 자기 자신에 대해 답한 노드는 `reported_by`가 없다. 다른 기기에 대해 보고한 노드(실패 관측, `no_capability`, 경로 중단의 `origin`)에는 보고한 기기가 들어 있다.

## 엣지 목록 입력

엣지를 실제로 모으는 일은 tasty가 한다(점유 레지스트리, 연결 등록). 이 레포는 입력 형식 `LocalLink{peer, local_side, holder, transport, rtt, connected_at, workspace, kind}`와 집계 함수를 준다.

- `local_side`(`dialer` · `acceptor`)는 이 기기가 그 링크를 열었는지 받았는지다. 집계는 이 값으로 엣지의 `dialer`·`acceptor`를 채운다.
- `kind`가 `attach`인 인증된 링크만 엣지가 된다. bulk·요청 채널, debug echo 연결은 넣지 않는다.
- surface 단위 attach도 엣지다(`workspace`에는 그 surface가 속한 workspace).
- 재연결로 링크가 바뀌면 소비자가 이전 링크를 입력에서 뺀다. 집계는 입력 스냅샷만 본다.

연결 실패 관측은 엣지와 별개 입력이다: `ConnectFailure{peer_hint, device_id, kind, observed_at}`. 거절된 접속은 인증된 링크가 아니라 `LocalLink`에 들어가지 않기 때문이다.

- 만드는 쪽은 그 상대에게 연결을 시도한 기기의 소비자다. tasty는 dialer로서 받은 실패(handshake 거절 사유, `PeerTooOld`, 시간 초과)를 원격 프로필마다 마지막 하나만 넣는다.
- `kind`는 `auth_rejected`(`PeerTooOld`, `legacy_protocol`, 토큰·서명·TOFU 거절처럼 인증 단계의 거절), `timed_out`(연결·handshake 시간 초과)이다. 받는 쪽에서 거절한 접속은 상대 이름을 알 수 없으므로 넣지 않는다.
- `device_id`는 아는 경우만 싣는다(TOFU 기록의 기대값). `peer_hint`는 소비자가 정한 표시 이름(tasty는 프로필 이름)이다.
- 같은 상대에 대해 엣지와 실패가 둘 다 있으면 엣지(살아 있는 연결)가 이긴다.
- `local_view(me, links, failures, now)`·`remote_answer(me, links, failures, sharing, now)`는 엣지와 실패를 함께 받는다(`me`는 자기 기기 ID와 표시 이름, `now`는 노드에 찍는 Unix ms). 실패는 `reported_by` = 자기 기기로 노드가 된다.

## 질의와 결과 메시지

질의는 인증된 attach 링크의 순서 채널로만 오간다. tasty Control 밖 raw JSON `event` 태그 확장(`tasty:docs/dev-guide/attach-behavior.md` "커스텀 이벤트 확장") 자리에 싣는다.

| 메시지 | `event` 값 | 필드 |
|---|---|---|
| 질의 | `net.topology.query` | `request_id`, `requester`(질의를 처음 보낸 기기), `origin`(기준 머신), `route`(요청한 기기에서 기준 머신까지 거칠 기기 ID 목록, 요청자가 기준 머신이면 빈 목록. 길이 상한 `MAX_ROUTE_HOPS`, 아래 "전달 hop 상한"), `path`(기준 머신부터 이 질의가 지나온 기기 ID 목록), `depth`(남은 depth), `remaining_ms`(보낼 때의 남은 시간, 상한 `MAX_QUERY_MS`), `branch`(이 전달 한 건의 식별자. 전달하는 기기가 전달마다 새로 만드는 128비트 난수의 hex. 요청자가 처음 보내는 질의에도 붙인다) |
| 결과 | `net.topology.result` | `request_id`, `branch`(이 응답을 부른 질의의 `branch`), `from`(이 조각을 만든 기기), `return_path`(결과를 되돌려 보낼 기기 ID 목록), `part`, `last`(bool, 이 `branch`에 대한 `from`의 자기 조각이 끝남), `nodes`, `edges` |
| 억제 | `net.topology.suppressed` | `request_id`, `branch`, `from`, `return_path`. 같은 질의를 이미 같거나 큰 남은 depth로 답했다는 뜻 |
| 분기 완료 | `net.topology.done` | `request_id`, `branch`, `from`, `return_path`, `timed_out`(`from`이 이 `branch`에서 기다리다 포기한 이웃 기기 ID 목록), `final`(bool. 이 `done`으로 질의 전체가 끝남. 기준 머신, 또는 `route` 전달을 중간에서 멈춘 hop만 `true`로 보낸다), `more`(bool, 없으면 `false`. `true`면 같은 `branch`·`from`의 `done`이 `timed_out` 나머지를 싣고 더 온다. `more`가 없는 마지막 것만 분기를 끝낸다), `part`(정수, 없으면 0. 같은 `branch`·`from`의 `done` 중 몇 번째인지, 0부터), `incomplete`(선택, 기기 ID 목록. `from`이 이 `branch`에서 응답을 온전히 중계하지 못한 기기. `timed_out`과 함께 `more`로 나눈다) |

- 상대가 `caps`에 `net.topology`를 알리지 않았으면 질의를 보내지 않고 그 노드를 `no_capability`로 둔다. tasty는 모르는 event를 조용히 버리므로(`tasty:crates/tasty-ipc/src/stream_hub.rs` `collect_other_control`) 보내 봐야 답이 오지 않는다.
- 결과 프레임 하나의 JSON은 `MAX_FRAME_LEN / 2` 이하다(tasty 원격 읽기 결과 규칙, `tasty:docs/dev-guide/attach-behavior.md` "mirror 터미널 읽기"). 넘으면 `part`로 나누고 마지막 조각에 `last: true`. 조각은 `(branch, from, part)`로 구별한다. 그래서 여러 기기가 같은 `request_id`로 보내도, 한 기기가 더 큰 남은 depth로 다시 방문돼 두 번 답해도 섞이지 않는다. 전달한 기기는 자기가 보낸 `branch`마다 `done`·`suppressed`를 기다리므로 같은 이웃에게 보낸 두 전달을 구별한다. `branch`는 전달 추적용이고, 노드 상태와 엣지의 합치기는 계속 요청자가 노드·엣지 기준으로 한다.
- 모든 메시지가 `MAX_FRAME_LEN / 2` 이하다. 받는 쪽은 그보다 긴 JSON을 읽기 전에 거절한다(`MessageError::TooLarge`). `done`의 `timed_out`이 한 메시지에 들어가지 않으면 `more`로 나눈다(`split_done`). 헤더(`return_path` 등)만으로 넘는 결과·`done`은 보내지 않는다.
- 항목 하나(노드나 엣지)가 혼자서도 한 메시지에 들어가지 않으면 그 항목을 빼고 보내되, 자기 노드를 `timed_out` + 사유 `incomplete`로 바꿔 싣는다. 요청자는 그런 노드가 있으면 `final` `done`을 받아도 탐색 상태를 `Finished`가 아니라 `Incomplete`로 둔다.
- 조각 송신이나 중계가 실패하면 그 기기는 그 응답의 뒤 조각과 `done`을 보내지 않는다(그 분기는 실패로 끝나고, 보낸 쪽은 그 분기의 이웃 응답도 기다리지 않는다). 요청자는 `(branch, from)`마다 받은 `part`와 `last`를 세어, 그 `done`이 왔거나 탐색이 끝났을 때 빠진 조각이 있으면 그 기기를 `timed_out` + `incomplete`로 바꾸고 탐색 상태를 `Incomplete`로 둔다. 조각을 하나도 받지 못한 `(branch, from)`의 `done`이 와도 같다. `done`도 `(branch, from)`마다 `part`를 세어(`more`가 없는 것이 마지막), 마지막 `done`이 왔거나 탐색이 끝났을 때 빠진 것이 있으면 같게 표시한다.
- 첫 조각 송신이 실패한 응답자는 그 질의의 응답 기록을 되돌린다. 같은 질의가 다시 오면 억제하지 않고 다시 답한다. 중계가 실패한 기기는 그 링크의 이웃을 기다리는 자기 분기마다 응답 주인 `from`을 `done`의 `incomplete`에 싣는다. 그 응답이 그 이웃이 이 분기에 보낸 자기 응답이면 그 이웃을 더 기다리지 않는다(사유 없는 `timed_out`이 되지 않는다). 요청자는 `incomplete`의 기기를 직접 응답이 왔어도 `timed_out` + `incomplete`로 둔다(그 응답의 일부가 빠졌다).

## 탐색 규칙

- 기준 머신까지의 경로(`route`)는 요청한 기기가 정한다. 요청자가 이미 아는 연결(자기 이웃 또는 앞선 탐색 결과)에서 고른 경로다. 각 hop은 `route`의 다음 기기와 인증된 attach 링크가 있을 때만 전달한다. 전달하지 못하면 기다리지 않고 바로 요청자에게 `origin` 노드 상태 `timed_out`과 사유를 담은 결과를 돌려주고, 이어서 `done`(`final: true`)을 보낸다. 사유는 `route_broken`(다음 hop과 링크가 없음), `route_no_capability`(다음 hop과 링크는 있지만 그 상대 `caps`에 `net.topology`가 없음), `route_sharing_off`(이 hop의 공개가 꺼져 있음, 아래 "공개 범위"), `route_invalid`(아래 "전달 hop 상한") 중 하나다. 이때 결과의 `return_path`는 `route`에서 자기 앞의 hop들을 거꾸로 놓고 끝에 `requester`를 붙인 목록이다. `timed_out`은 "응답을 받지 못했다"는 상태이고, 사유가 기다려서 못 받았는지(사유 없음) 전달하지 못했는지(위 세 사유)를 가른다. 전달 hop은 depth에 세지 않는다. 남은 시간이 0이면 남은 hop은 전달하지 않는다.
- 전달 hop 상한: `route` 길이는 `MAX_ROUTE_HOPS`(`tasty-net-topology` 상수, 16) 이하이고, 같은 기기 ID가 두 번 나오지 않고, 요청자와 `origin`을 담지 않아야 한다. 요청자가 기준 머신이면 `route`는 비어 있어야 한다. 요청자는 질의를 만들 때 검사해 어기면 보내지 않고 로컬 오류 `TopologyError::InvalidRoute`를 돌려준다. 각 `route` hop도 받을 때 질의의 `requester`·`origin`으로 같은 검사를 하고, 어기면 전달하지 않고 사유 `route_invalid`로 위처럼 끝낸다. 그보다 먼저 각 hop은 질의를 받은 링크의 상대가 바로 앞 기기인지(첫 hop은 `requester`, 그다음은 `route`의 앞 hop), 그리고 자기가 `route`에 있는지 본다. 어긋나면 돌아갈 경로를 믿을 수 없으므로 답하지 않고 버린다(debug 기록, 송신 0). `route_invalid` 답은 링크 상대를 확인한 질의의 구조 위반에만 보낸다. 시험은 `tasty-net-topology`의 `route_over_hop_limit_is_rejected`, `route_with_repeated_device_is_rejected`다.
- 마감은 각 기기의 단조 시계로 판정한다. 질의를 받은 기기는 받은 때의 단조 시각 + `min(remaining_ms, MAX_QUERY_MS)`를 자기 마감으로 두고, 전달할 때 그때의 남은 시간을 `remaining_ms`에 넣는다. 전달할 때는 자기 남은 시간에서 여유분(`AgentConfig::hop_reserve`)을 빼고 넣어, 다음 기기가 자기보다 먼저 끝나 `done`이 제때 돌아오게 한다. `MAX_QUERY_MS`는 30,000이다. 요청자는 자기 단조 시계로 전체 상한을 지킨다. 기기 사이의 벽시계 차이는 판정에 들어가지 않는다.
- 결과는 질의가 온 길을 거꾸로 돌아간다. 기준 머신부터의 결과는 `path`를 거꾸로, 그다음 `route`를 거꾸로, 마지막에 `requester`로 간다. 중간 기기는 결과를 합치지 않고 그대로 넘긴다. 합치기(같은 노드의 중복 제거, 상태 고르기)는 요청자 한 곳에서만 한다.
- 기준 머신부터는 hop마다 depth를 1 줄인다. depth 0인 노드는 자기 엣지만 답하고 더 전달하지 않는다.
- 들어오는 링크와 나가는 링크를 모두 따라간다.
- 순환 차단은 경로별이다. 질의를 받은 기기는 `path`에 이미 있는 기기에는 다시 보내지 않는다. 그래서 A↔B 순환이 끝난다. 질의 전체가 공유하는 방문 집합은 두지 않는다(그러면 아래 "더 짧은 경로로 같은 노드에 다시 닿아 남은 depth가 더 크면" 항목의 이어 탐색을 막는다).
- 중복 응답 억제는 기기별 기록으로 한다. 각 기기는 `request_id`마다 지금까지 답한 가장 큰 남은 depth를 잠시(자기 마감까지) 기억한다. 같은 `request_id`가 더 작거나 같은 남은 depth로 다시 오면 노드·엣지를 다시 싣지 않고 `net.topology.suppressed`만 보낸다. 전달한 쪽은 이것을 정상 응답으로 보고 그 이웃을 `timed_out`으로 두지 않는다. 더 큰 남은 depth로 오면 다시 답하고 그 depth로 전달한다.
- 더 짧은 경로로 같은 노드에 다시 닿아 남은 depth가 더 크면, 위 기록 규칙에 따라 그 남은 depth로 그 노드부터 이어 탐색한다(다이아몬드에서 긴 경로가 먼저 닿은 경우).
- 이웃마다 응답 시간 제한을 둔다(`AgentConfig::hop_timeout`, 전달한 뒤 그 이웃의 첫 결과 조각이나 `suppressed`가 올 때까지). 넘으면 그 노드를 `timed_out`으로 두고 탐색은 계속한다. 첫 조각이 온 이웃은 자기 마감까지 마지막 `done`을 기다리고, 마감이 지나면 `timed_out`이 아니라 `incomplete` 목록에 넣는다(직접 응답은 왔지만 `done`이 오지 않았다. 마지막 `done`의 송신이 실패한 경우다).
- `route` hop은 질의를 전달하고 돌아오는 결과를 그대로 넘길 뿐 자기 `done`을 보내지 않는다. 기준 머신의 `done`(`final: true`)이 요청자에게 간다. 남은 시간에서 여유분을 빼면 0인 `route` hop은 전달하지 않고 사유 없는 `origin` `timed_out`과 `done`(`final: true`)으로 끝낸다.
- 분기 완료: 각 기기는 자기 조각을 다 보내고, 전달한 모든 이웃에게서 `done` 또는 `suppressed`를 받았거나 그 이웃을 시간 제한으로 포기했을 때 `net.topology.done`을 보낸다. 포기한 이웃은 `timed_out` 목록에 싣는다. 전체 완료는 요청자가 `final: true`인 `done`(기준 머신의 분기 완료, 또는 `route`를 중간에서 멈춘 hop의 종료)을 받았을 때, 또는 요청자 자신의 마감이 지났을 때다.
- 같은 노드에 대한 합치기(요청자): 그 노드가 직접 만든(`from` = 그 노드) 조각이 하나라도 있으면 그 조각의 자기 노드 상태(`online` 또는 `sharing_off`)를 쓴다. 직접 응답은 다른 경로의 실패 관측보다 우선한다. 다른 경로의 전달자가 그 노드를 `timed_out`으로 보고했어도 상태를 바꾸지 않는다. 그 경로로는 응답을 못 받았다는 뜻일 뿐이다. 어느 경로로도 직접 조각이 없을 때만 보고된 상태(`timed_out`·`auth_rejected`·`no_capability`)를 쓴다.
- 원격 ID를 확인할 수 없는 상태의 노드: `auth_rejected`·`timed_out`·`no_capability` 노드는 그 상대와 연결을 시도한 이웃이 만든다(`auth_rejected`는 위 "엣지 목록 입력"의 연결 실패 관측에서 나온다). 노드 식별자는 그 이웃이 아는 상대 기기 ID(링크의 `peer()`, 또는 TOFU 기록에 있는 기대값)다. 기기 ID를 모르면(인증 전에 거절돼 ID를 받지 못함) `device_id`가 비고 `reported_by`와 `peer_hint`로 구분한다. 요청자는 `device_id`가 있으면 그것으로, 없으면 `(reported_by, peer_hint)`로 합친다. `offline`은 정의 파일 방식에서 tasty만 만든다.
- 결과는 도착하는 대로 점진적으로 낸다. 노드에는 응답 시각을 붙인다.
- 탐색은 "이웃에게 질의를 보내는 함수"를 주입받는 상태 기계다. 그래서 전송 없이 가짜 이웃으로 시험한다. 크레이트에서는 기기마다 `Agent` 하나이고, 주입하는 함수는 `Outbox::send`다. `send`는 링크가 없으면 `SendError::NoLink`, 상대 `caps`에 `net.topology`가 없으면 보내지 않고 `SendError::NoCapability`를 돌려준다. 소비자는 받은 토폴로지 메시지를 `Agent::on_message`에 넘기고 `Agent::next_deadline` 때 `Agent::poll`을 부른다. 시험은 `crates/tasty-net-topology/tests/explore.rs`다.
- 링크로 나가는 조각은 누가 질의했든 언제나 `remote_answer`로 만든다(공개 꺼짐 말단 처리와 workspace 공개 범위를 적용). 요청자가 탐색 중 이웃으로서 질의를 받으면(기준 머신이 다른 기기일 때) 그 조각도 돌아오는 길의 다른 기기를 거치기 때문이다. 요청자 자기 화면에는 자기 `local_view`를 탐색 결과에 바로 합치고, 링크로 돌아온 자기 조각은 합치지 않는다(부모의 대기 추적을 위해 조각은 그대로 보낸다). 요청자가 기준 머신이면 자기 조각은 링크로 나가지 않으므로 공개 꺼짐이어도 자기 탐색을 이웃에게 전달한다.
- 링크 대조: 기준 머신은 링크 상대가 `route`의 마지막 hop(없으면 `requester`)인지, 기준 머신을 지난 기기는 링크 상대가 `path`의 마지막 기기이고 `path`가 `origin`에서 시작하고 같은 기기가 두 번 없고 자기를 담지 않는지 본다. 어긋난 질의는 답하지 않고 버린다. 응답(결과·억제·완료)은 링크 상대가 그 `request_id`에서 기다리는 상대(전달한 이웃, `route` hop이 전달한 다음 hop, 자기 탐색의 첫 hop)일 때만 중계·합치기·완료 처리를 하고, 아니면 버린다. 자기가 전달한 분기의 `branch`를 단 응답은 중계·합치기 전에 그 분기의 이웃이 그 이웃의 링크로 같은 `request_id`에 보낸 것인지 본다. 요청자는 `origin`이나 `route` hop이 보낸 `final`만, 그것도 끝이 올 수 있는 링크(자기 탐색의 첫 hop, 자기가 기준 머신이면 자기 자신)로 온 것만 탐색 끝으로 본다.
- 늦은 응답: 받은 메시지를 처리하기 전에 지난 마감을 먼저 적용한다(`Agent::on_message`가 `poll`을 먼저 부른다). 그래서 요청자 전체 마감, 분기 마감, 이웃 첫 응답 마감이 지난 뒤 온 응답은 합치거나 중계하지 않는다. `poll`은 요청자 전체 마감을 자기 분기 완료보다 먼저 처리한다.
- 알려진 한계: 받는 쪽 상한이 없다. 링크는 인증된 자기 기기 사이뿐이라는 전제로, 받는 `depth`, 한 기기가 동시에 기억하는 질의 수(`request_id`별 응답 기록·대기 분기), 요청자 탐색 하나가 모으는 노드·엣지 수에 상한을 두지 않는다. 메시지 하나의 크기(`MAX_MESSAGE_JSON`)와 시간(`MAX_QUERY_MS`)만 막는다. 상한 값은 tasty 배선과 실제 망 크기를 보고 정한다.

## 공개 범위

사용자 결정: 공개 범위는 기기 단위 설정 하나다. 설정이 없으면 제공한다. 끄면 질의에 자기 자신만 답하는 말단 노드가 된다.

- 원격 질의 응답: 공개 꺼짐이면 자기 노드 하나(`sharing_off`)만 답하고 전달하지 않는다. `route`의 중간 hop이어도 같다. 공개 꺼짐이 중계보다 우선하므로, 자기 노드를 답한 뒤 사유 `route_sharing_off`로 위 "탐색 규칙"처럼 끝낸다(`done`, `final: true`). 공개 켜짐이어도 `workspace` 메타데이터는 소비자가 정한 범위(이름·ID 중 무엇을 내보내는가)만 싣는다.
- 로컬 조회(자기 화면): 공개 설정과 관계없이 자기 엣지를 모두 보여 준다.
- 두 범위를 함수로 나눈다: `local_view`와 `remote_answer`(인자는 위 "엣지 목록 입력"). `Sharing`이 없으면(`Sharing::default()`) 공개 켜짐이고 workspace 필드는 하나도 싣지 않는다. 무엇을 실을지(`WorkspaceExposure`)는 tasty가 정한다([설계 목차](index.md) "남은 결정"). 설정을 저장하는 일은 tasty 몫이다.
- 응답 측 공통 함수 `answer_query`(`Agent::answer_query`)가 질의를 받아 자기 엣지를 답하고 다음 hop으로 전달한다. GUI·headless 배선은 tasty가 한다.
