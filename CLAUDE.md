# tasty-network

[Tasty](https://github.com/zilhak/tasty)의 네트워크 라이브러리 레포다. 공개 레포다.

## 역할

- 스트림 프로토콜과 그 규약, 그 프로토콜을 나르는 전송(SSH 터널, P2P), relay 서버 패키지를 맡는다.
- Tasty와 다른 Tasty 서비스가 이 레포의 크레이트를 git 의존으로 가져다 쓴다.
- 설계의 정본은 [설계 문서](docs/design/index.md)다. 크레이트 이름·경로·의존 방향은 [크레이트 구성](docs/design/crates.md)의 표를 따르고, 바꾸려면 그 표를 먼저 고친다.

## 의존 규칙

- tasty 레포의 크레이트에 의존하지 않는다. 경로 의존과 git 의존 모두 안 된다. `bash scripts/check.sh no-tasty-deps`가 검사하고 CI도 돌린다([동작 수준 기준](docs/design/done-criteria.md) D7).
- 이 레포 안 크레이트 사이의 의존은 [크레이트 구성](docs/design/crates.md) "의존 방향"을 따른다.

## 빌드·검사

검사 명령은 `scripts/check.sh` 하나다. git 훅과 CI가 같은 단계를 부른다.

| 명령 | 하는 일 |
|---|---|
| `bash scripts/check.sh` | fmt, shellcheck, no-tasty-deps, p2p-isolation, clippy, test |
| `bash scripts/check.sh fmt` | `cargo fmt --all --check` |
| `bash scripts/check.sh lint` | `cargo clippy --workspace --all-targets --locked -- -D warnings` |
| `bash scripts/check.sh test` | `cargo test --workspace --locked` |
| `bash scripts/check.sh p2p-isolation` | P2P 크레이트의 workspace 안 의존 범위(자체 시험 포함) |
| `bash scripts/check.sh deny` | cargo deny의 licenses·bans·sources (`deny.toml`) |

단계 목록의 정본은 `bash scripts/check.sh help`다. `Cargo.lock`은 커밋한다. 의존을 해석하는 cargo 명령(clippy·test·check·metadata·deny·fetch)은 `--locked`로 돌린다. `cargo fmt`는 의존을 해석하지 않아 그 옵션이 없다.

## 버전·태그

- 모든 크레이트가 workspace 버전 하나를 쓰고 crates.io에 올리지 않는다(`publish = false`).
- 태그는 `v<semver>`이고 `main`의 버전 커밋에만 붙인다.
- 규칙의 정본은 [버전·호환](docs/design/versioning.md)이다.

## Git 규칙

- 커밋 형식과 단위는 tasty의 [커밋 컨벤션](https://github.com/zilhak/tasty/blob/develop/docs/dev-guide/commit-convention.md)을 따른다.
- 브랜치는 git-flow(`develop`·`release`·`main`)다. [브랜치](docs/dev-guide/branching.md)를 본다.
- 새 clone에서 `bash scripts/dev-setup.sh`를 한 번 실행해 훅을 켠다. 훅별 검사는 [Git 훅](docs/dev-guide/git-hooks.md), CI 잡은 [CI](docs/dev-guide/ci.md)에 있다.
- `--no-verify`를 쓰지 않는다. `core.hooksPath`를 바꾸는 등 훅을 끄는 다른 방법도 같다. 훅이 막으면 우회하지 말고 고치거나 보고한다.
- 에이전트가 쓰는 로컬 설정·작업 폴더(`.gitignore`가 제외한다)는 git에 넣지 않는다. 로컬 경로와 로컬 작업 문서를 이 레포에 적지 않는다.
