# 예제

`crates/tasty-net-demo`의 예제 바이너리로 두 프로세스 사이의 SSH 정방향 attach(연결·끊기·재연결)와 역할 반전 attach를 손으로 돌려 보고, 같은 흐름을 자동으로 확인하는 통합 시험을 돌리는 방법이다. 크레이트의 자리와 바이너리 이름은 [크레이트 구성](design/crates.md)이 정본이다.

## 바이너리

| 바이너리 | 하는 일 |
|---|---|
| `demo-server` | workspace 제공 쪽 흉내. 루프백 포트에서 handshake v2를 받고, `--record` 자리에 연결 레코드(포트·토큰·인스턴스 ID)를 발행한다. 정방향 링크마다 Control snapshot을 한 번 보내고 Data를 그대로 돌려준다. 역할 반전 제안은 `--proposals accept\|refuse`(기본 `refuse`)로 답하고, 수락한 링크에서는 holder가 되어 dialer의 descriptor를 받고 stdin의 `send <글>` 줄을 가장 최근 링크에 Data로 보내 돌아온 echo를 찍는다. attach 링크 아래의 bulk 채널은 받아 프레임을 그대로 돌려주고, 요청 채널에는 `system.info`에만 자기 인스턴스 ID로 답한다. stdin이 닫히거나 `quit`을 읽으면 레코드를 거두고 끝난다 |
| `demo-dialer` | SSH로 서버 기기의 레코드를 읽고(`cat`, 그다음 `type`) `-L` 터널을 열어 handshake한다. stdin의 `send <글>` 줄을 Data로 보내고 echo를 찍는다. 링크가 끊기면 transport의 `Redialer`(기본 설정값)로 backoff 뒤 레코드 발견부터 다시 한다. 다시 연결해도 결과가 같은 거절(틀린 토큰 등)이면 종료 코드 3으로, `Redialer`가 포기하거나(`redial.give_up_after`) 다시 걸 끊김이 아니면 종료 코드 4로 끝난다. `quit`이나 stdin 끝이면 Detach를 보내고 0으로 끝난다. `--holder acceptor`면 같은 `-L` 터널 위에서 역할 반전을 제안하고, 수락되면 workspace 제공 쪽이 되어 descriptor를 보낸 뒤 받은 Data를 돌려준다. 끊기면 다시 연결하는 쪽은 그대로 dialer다. `--channels`면 첫 링크가 올라올 때마다(다시 연결한 뒤 포함) 그 아래 bulk 채널과 같은 터널 위 요청 채널을 열고, `--also-holder dialer\|acceptor`면 같은 터널로 그 holder의 두 번째 attach 링크를 첫 링크가 올라올 때마다 연다. 두 번째 링크는 따로 다시 연결하지 않는다 |

옵션과 stdout에 찍는 사건 줄의 정본은 각 바이너리 소스의 머리 주석이다(`src/bin/demo-server.rs`, `src/bin/demo-dialer.rs`). 사건 줄은 `<누구> <사건> 키=값 ...` 형식이다.

두 바이너리 모두 stdin에 `table`을 주면 자기 연결 목록([전송 추상화](design/transport.md) "연결 목록")을 한 묶음으로 찍는다. `<누구> table device=<자기 기기 ID> entries=<n>`, 항목마다 `<누구> conn id=... path=... direction=... local_side=... state=... ...`, attach 링크마다 `<누구> link conn=<항목> link_id=... holder=... transport=...`, 끝에 `<누구> table-end`다. 값은 공백 없는 한 낱말로 퍼센트 인코딩하고 없는 값은 `-`다. 키 목록과 인코딩의 정본은 `src/table.rs`다.

## 손으로 돌리기

SSH로 로그인할 수 있는 기기(같은 기기의 `localhost`도 된다)가 하나 있으면 된다. 비밀번호 없이 로그인되게 키를 준비해 둔다.

```bash
cargo build -p tasty-net-demo --bins --locked

# 단말 1: SSH로 닿는 기기에서 서버를 띄운다
target/debug/demo-server --record "$HOME/tasty-net-demo/record.json"
# server ready pid=... port=... instance=demo-server-...

# 단말 2: 다른 기기(또는 같은 기기)에서 dialer를 띄운다. --record는 서버 기기 안의 경로다
target/debug/demo-dialer --destination me@server-host --record /home/me/tasty-net-demo/record.json
# dialer connected attempt=1 instance=demo-server-... tunnel_pid=12345
send hello
# dialer echo hello
```

레코드는 예제 전용 하위 디렉터리(Windows는 `%USERPROFILE%\tasty-net-demo\record.json`)에 둔다. 레코드를 발행하면 그 부모 디렉터리가 없으면 만들고 권한을 소유자만(Unix 0700)으로 좁히며, 레코드를 거둬도 되돌리지 않는다. 홈 디렉터리 바로 아래 파일을 주면 홈 디렉터리 권한이 좁혀진다.

`--ssh-port`, `--identity <키 파일>`, `--option Key=Value`(여러 번)는 `ssh` 옵션으로 넘어간다. `--read-timeout-ms`는 링크의 읽기 timeout이다(기본 30000, 두 바이너리에 같은 값을 준다).

끊기와 재연결:

- 터널 끊기: `dialer ssh-spawned pid=<PID> role=tunnel` 줄의 PID를 끝낸다(`kill <PID>`, Windows `taskkill /F /PID <PID>`). dialer가 `dialer lost failure=peer_closed`를 찍고, backoff 뒤 새 터널로 `dialer connected ... tunnel_pid=<새 PID>`를 찍는다. 서버는 `server released client=1 live=0`, `server attached client=2 live=1`을 찍는다.
- 서버 재시작: 서버를 끝내고(Ctrl-C) 같은 `--record`로 다시 띄운다. 끝난 서버의 레코드가 남아 있는 동안 dialer는 `dialer retry`를 찍으며 다시 시도하고, 새 레코드를 읽으면 새 인스턴스 ID로 `dialer connected`를 찍는다.
- 틀린 토큰: 레코드 파일의 `token`을 다른 64자리 16진수로 바꾼 사본을 `--record`로 주면 `dialer rejected reason=token_invalid`를 찍고 다시 시도하지 않고 끝난다.

## 역할 반전 손으로 돌리기

역방향 attach는 [전송 추상화](design/transport.md) "역할"의 결정 (b)로, SSH를 띄운 쪽(dialer)이 그대로 `-L` 터널을 열고 handshake에서 `holder = "acceptor"`를 제안한다. 받은 쪽(서버)이 수락하면 서버가 holder, dialer가 workspace 제공 쪽이 된다. `-R` 역방향 터널은 쓰지 않는다.

```bash
# 단말 1: 제안을 수락하는 서버
target/debug/demo-server --record "$HOME/tasty-net-demo/record.json" --proposals accept
# server ready pid=... port=... instance=demo-server-...

# 단말 2: 역할 반전을 제안하는 dialer
target/debug/demo-dialer --destination me@server-host --record /home/me/tasty-net-demo/record.json --holder acceptor
# dialer connected attempt=1 instance=demo-server-... tunnel_pid=12345
# dialer roles local=dialer launcher=dialer dialer=dialer provider=dialer holder=acceptor redialer=dialer

# 단말 1에서: 서버가 보내고 dialer가 돌려준다
send hello
# (단말 2) dialer served hello
# (단말 1) server echo client=1 hello
```

- 서버는 `server proposal peer=<dialer 인스턴스> answer=accept`, 같은 역할 표의 `server roles client=1 local=acceptor ...`, dialer가 보낸 descriptor `server control client=1 {"snapshot":...}`를 찍는다.
- 터널을 끊으면 정방향과 같이 dialer가 다시 연결하고 다시 제안한다. 서버는 다시 걸지 않는다.
- 서버를 `--proposals accept` 없이 띄우면 제안을 거절한다. dialer는 `dialer rejected reason=role_refused`를 찍고 다시 시도하지 않고 종료 코드 3으로 끝난다.
- dialer에 `--instance-id <서버의 인스턴스 ID>`를 주면 자기 자신과의 연결로 보고 `dialer rejected reason=self_attach`로 끝난다.

## 통합 시험

`crates/tasty-net-demo/tests/ssh_attach.rs`가 위 흐름을 실제 `ssh`·sshd와 별도 OS 프로세스 두 개(`demo-server`, `demo-dialer`)로 자동 확인한다([동작 수준 기준](design/done-criteria.md) D2). 시험은 한 번에 하나씩 돈다.

| 시험 | 확인하는 것(요약) |
|---|---|
| `connect_and_echo` | 연결, handshake, snapshot, echo. 두 프로세스의 PID가 다르다 |
| `reconnect_after_tunnel_kill` | 터널 ssh를 끝내면 dialer가 끊김을 분류하고 새 터널(다른 PID)로 다시 붙어 echo가 된다. 서버에 남은 링크는 하나다 |
| `reconnect_after_server_restart` | 서버를 죽이고 다시 띄우면 dialer가 새로 발견한 레코드(다른 인스턴스 ID)로 다시 붙는다 |
| `wrong_token_is_rejected_without_retry` | 틀린 토큰은 `token_invalid`로 거절되고 dialer는 시도 한 번으로 끝난다 |
| `connection_table_reports_ssh_forward` | attach·bulk·요청 채널을 나르는 터널 하나가 dialer에서는 항목 하나(구간 셋, `ssh` 경유, rendezvous 없음), 서버에서는 채널마다 `ssh_inbound` 항목(bulk는 `parent`)이다. 터널을 끊으면 옛 항목은 원인과 함께 `disconnected`, 새 항목은 새 ID로 옛 ID를 가리키고 `reconnecting`에서 `connected`가 된다 |

`crates/tasty-net-demo/tests/ssh_reverse_attach.rs`는 역할 반전을 같은 방식으로 확인한다(D3). 두 파일은 같은 시험 도우미(`tests/support/`)를 쓴다.

| 시험 | 확인하는 것(요약) |
|---|---|
| `reversed_holder_echo` | 서버가 제안을 수락하고 holder가 된다. 두 끝이 같은 역할 표(SSH를 띄운 쪽·dialer·제공 쪽·다시 거는 쪽은 dialer, holder는 서버)를 찍고, 첫 Control은 dialer의 descriptor다. 서버가 보낸 Data가 dialer를 거쳐 돌아온다 |
| `a_quoted_instance_id_survives_the_descriptor` | 따옴표·역슬래시가 든 dialer 인스턴스 ID가 descriptor(JSON 직렬화)를 거쳐 서버에 그대로 닿는다. 서버는 descriptor를 파싱하고, 파싱되지 않으면 holder가 되지 않고 `server control-invalid`를 찍는다 |
| `reconnect_after_tunnel_kill` | 터널 ssh를 끝내면 서버 쪽 링크가 끝나고 dialer가 새 터널(다른 PID)로 다시 붙어 다시 제안한다. 역할 표와 echo가 그대로다 |
| `refused_proposal_stops_without_retry` | 거절하는 서버에서 dialer는 `role_refused`로 시도 한 번 만에 종료 코드 3으로 끝난다 |
| `self_attach_is_rejected` | 서버의 인스턴스 ID를 쓰는 dialer는 `self_attach`로 끝나고 제안·attach가 일어나지 않는다 |
| `connection_table_reports_ssh_role_reversed` | `connection_table_reports_ssh_forward`와 같은 모양을 `ssh_role_reversed`·`role_reversed`로 본다 |
| `both_ends_produce_matching_local_links` | 두 끝의 연결 목록에서 만든 토폴로지 입력(링크 ID, dialer, acceptor, holder, 엣지 값)이 같다 |
| `mixed_holders_on_one_tunnel` | 한 터널 위 정방향 링크와 역할 반전 링크: SSH를 띄운 쪽 항목은 `mixed`, 링크마다 두 끝의 값이 같다(`ssh_forward` 하나, `ssh_role_reversed` 하나) |

정확한 조건은 시험 파일이 정본이다. 모든 시험은 끝에 sshd·ssh·예제 프로세스가 남지 않았는지 확인한다.

```bash
cargo test -p tasty-net-demo --test ssh_attach --locked -- --nocapture
cargo test -p tasty-net-demo --test ssh_reverse_attach --locked -- --nocapture
```

- SSH 서버: 시험이 임시 폴더에 키와 설정을 만들어 비특권 OpenSSH `sshd`를 띄운다. sshd가 없거나 설정 검사·기동·로그인·`-L` 확인이 실패하면 시험 전용 Rust SSH 서버(`russh`)로 돈다. 둘 다 안 되면 시험은 실패한다. 규칙은 [CI와 시험 환경](design/testing.md) "SSH 통합 시험의 sshd"다.
- `TASTY_NET_TEST_SSHD`: `auto`(기본), `openssh`(대체 없음), `russh`. `TASTY_NET_TEST_SSHD_PATH`: 쓸 `sshd` 경로.
- 시험이 통과해도 stderr에 남는 증거 줄(`[ssh_attach] ...`, `[ssh_reverse_attach] ...`): `sshd=openssh|russh|none`, `pids server=... dialer=... tunnel_ssh=...`, `tunnel ssh pid before=... after=...`, `processes left after the test: <n> of <m> tracked`. 역할 반전 시험은 두 끝의 역할 줄(`roles: ...`)도 남긴다.
- `bash scripts/prepare-test-sshd.sh`는 시험이 쓸 sshd를 찾아 설정을 `sshd -t`로 검사하고 결과를 찍는다. CI가 시험 전에 부른다([CI](dev-guide/ci.md)).
