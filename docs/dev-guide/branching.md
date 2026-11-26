# 브랜치

이 레포는 tasty와 같은 세 브랜치(`develop`·`release`·`main`)를 쓴다. 규칙의 정본은 tasty의 [브랜치 전략](https://github.com/zilhak/tasty/blob/develop/docs/dev-guide/branching.md)이다. 이 문서는 그 규칙을 요약하고 이 레포에서 다른 점만 적는다.

| 브랜치 | 뜻 | 들어오는 것 |
|---|---|---|
| `develop` | 개발 중인 버전. 통합 브랜치이며 GitHub 기본 브랜치다 | CI를 통과한 작업 브랜치(fast-forward). `release`·`main`의 역병합 |
| `release` | 릴리스 후보 | `develop`에서 고른 시점. 검증, 릴리스 버그 수정, 버전 커밋. `main`의 역병합 |
| `main` | 배포된 버전. 릴리스 태그는 `main`의 커밋에만 붙인다 | 릴리스 때 `release`. hotfix |

## 작업 브랜치

- 작업 브랜치는 `develop`에서 갈라 `develop` 위로 rebase한다.
- 반영은 `git merge --ff-only <작업-브랜치>`로 한다. 작업 브랜치를 merge 커밋으로 합치지 않는다.
- PR의 base는 `develop`이다.

## release 승격

1. `release`를 `develop`에서 고른 커밋으로 올린다. `release`가 그 커밋의 조상이면 `git merge --ff-only <커밋>`, 아니면(릴리스 버그 수정이 아직 `develop`에 없을 때) merge 커밋으로 합친다.
2. `release`에서 검증하고, 버전을 올리는 버전 커밋을 만든다.
3. `release`에서 고친 버그와 버전 커밋을 `develop`에 역병합한다. `develop`이 앞서 있으면 merge 커밋이 된다.

`release`에는 새 기능을 넣지 않는다.

## main과 태그

1. `main`을 `release`로 fast-forward한다(`git merge --ff-only release`). 태그는 merge 커밋이 아니라 버전 커밋에 붙인다. `main`에 `release`에 없는 hotfix가 있으면 먼저 `main`을 `release`에 역병합하고 그 위에서 버전 커밋을 만든다.
2. 버전 커밋에 태그를 붙이고 `release`·`main`과 태그를 한 번에 push한다.

   ```bash
   git tag -a vX.Y.Z -m vX.Y.Z
   git push --atomic origin release main vX.Y.Z
   ```

## hotfix

1. `main`에서 갈라 고치고 `main`에 반영한다.
2. `main`에서 버전 커밋을 만들고 태그를 붙여 `main`과 태그만 push한다(`git push --atomic origin main vX.Y.Z`).
3. `main`을 `release`와 `develop`에 역병합한다. 대상이 앞서 있으면 merge 커밋이 된다.

역병합을 cherry-pick으로 하지 않는다. 해시가 달라져 다음 `release` → `main` 승격이 fast-forward로 되지 않는다.

## merge 커밋을 허용하는 경우

| 현재 브랜치 | 출처 브랜치 | 종류 |
|---|---|---|
| `release` | `main` | 역병합 |
| `develop` | `main` | 역병합 |
| `develop` | `release` | 역병합 |
| `release` | `develop` | 승격 |
| `main` | `release` | 승격 |

그 밖의 merge 커밋은 훅이 막는다. 판정은 [Git 훅](git-hooks.md#merge-커밋-판정)에 있다. 이 판정은 로컬 훅만 하고 CI는 merge 커밋을 보지 않는다.

## tasty와 다른 점

- 태그는 workspace 버전(`Cargo.toml`의 `[workspace.package] version`)으로 `v<semver>`를 쓴다(예: `v0.1.0`). 모든 크레이트가 이 버전 하나를 같이 쓴다([버전·호환](../design/versioning.md)).
- 플러그인 버전 규칙이 없다. 크레이트마다 따로 버전을 올리지 않는다.
- 사이트 승격(tasty-page)과 README 배지가 없다.
- 태그 push로 도는 릴리스 워크플로가 아직 없다. tasty는 태그 커밋을 git 의존의 `rev`로 고정해 쓴다([버전·호환](../design/versioning.md) "tasty가 쓰는 방식").
- 훅은 tasty보다 검사가 적다([Git 훅](git-hooks.md)).

## CI

`develop`·`release`·`main` push와 이 세 브랜치를 base로 한 PR에서 같은 CI가 돈다([CI](ci.md)).
