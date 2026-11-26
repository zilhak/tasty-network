# CI

워크플로는 `.github/workflows/ci.yml`과 `.github/workflows/relay.yml` 두 개다. 둘 다 `develop`·`release`·`main` push, 이 세 브랜치를 base로 한 PR, 수동 실행(`workflow_dispatch`)에서 돈다. `ci.yml`의 검사는 `scripts/check.sh`의 단계를, `relay.yml`의 검사는 `relay/check-config.sh`를 부르므로 로컬과 훅에서 같은 명령으로 다시 돌릴 수 있다.

## 잡

| 잡 | 러너 | 단계 |
|---|---|---|
| `fmt` | `ubuntu-latest` | `scripts/check.sh fmt` |
| `clippy-test` | `ubuntu-latest`·`macos-latest`·`windows-latest` (`fail-fast: false`) | `scripts/check.sh lint`, `scripts/check.sh test --no-fail-fast` |
| `repo-checks` | `ubuntu-latest` | `shellcheck`(`scripts/check.sh shell`), `no-tasty-deps`(`scripts/check.sh no-tasty-deps`) |
| `deny` | `ubuntu-latest` | `scripts/check.sh deny` |
| `relay-config-check` (`relay.yml`) | `ubuntu-latest` | `bash relay/check-config.sh --self-test`, `bash relay/check-config.sh` |

- 러너는 GitHub 호스트 러너다([시험](../design/testing.md)). 잡마다 `timeout-minutes`가 있고 권한은 `contents: read`뿐이다.
- `relay.yml`은 `relay/` 밖만 바뀐 커밋에서도 돈다. `develop`의 어느 커밋이든 D5 (a)의 실행 ID를 남기기 위해서다. 러너의 docker로 고정 이미지 `n0computer/iroh-relay:v1.5.0`(digest 고정)을 받아 띄운다. 아래 concurrency 규칙은 두 워크플로가 같고 그룹 이름의 앞머리(`ci-`·`relay-`)만 다르다.
- push와 수동 실행은 브랜치와 커밋마다 다른 concurrency 그룹을 쓴다. 그래서 서로 다른 커밋의 push는 각자 실행이 남는다. 같은 ref·같은 커밋의 push와 수동 실행은 그룹을 함께 쓰므로, 그 그룹에서 대기 중인 앞 실행은 같은 ref·커밋의 새 요청에 밀려 취소될 수 있다.
- PR은 PR마다 그룹 하나를 쓰고, 새 커밋이 오면 앞 커밋의 실행을 취소한다.
- Windows 잡은 Git Bash(`shell: bash`)로 돈다.

## 툴체인

잡마다 `scripts/install-toolchain.sh`가 `rust-toolchain.toml`의 채널을 rustfmt·clippy와 함께 설치한다. `RUSTUP_AUTO_INSTALL=0`이라 그 밖의 툴체인을 몰래 받지 않는다.

## 동작 수준 기준과의 관계

- D1(세 OS CI)은 이 워크플로 실행 하나의 `fmt` 잡과 `clippy-test` 세 잡의 결론으로 본다.
- D7(tasty 의존 없음)은 `repo-checks` 잡의 `no-tasty-deps` 단계 결론으로 본다.
- D5 (a)(relay 설정 기동)는 `relay.yml`의 `relay-config-check` 잡 결론으로 본다.
- 기준과 증거 형식의 정본은 [동작 수준 기준](../design/done-criteria.md)이다.

## cargo deny

- `deny.toml`의 licenses·bans·sources를 본다. 라이선스 허용 목록은 tasty `deny.toml`과 같다. git 의존과 crates.io 밖의 레지스트리는 받지 않는다.
- advisories는 보지 않는다. 새 보안 공지는 커밋과 무관하게 나와 바뀐 것이 없는 커밋의 실행도 실패시키기 때문이다. 외부 의존(iroh 등)이 들어오는 작업에서 따로 잡을 둘지 정한다.
- cargo-deny는 공식 릴리스 아카이브를 받아 워크플로에 고정한 SHA-256과 대조한 뒤 쓴다.

## shellcheck

`scripts/install-shellcheck.sh`가 러너에 shellcheck가 없을 때만 고정 판(SHA-256 대조)을 잡의 임시 폴더에 받는다.
