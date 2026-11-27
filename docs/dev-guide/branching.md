# 브랜치

이 레포는 tasty와 같은 git-flow(`develop`·`release`·`main`)를 쓴다. 브랜치의 뜻, 작업 브랜치 반영(`git merge --ff-only`), release 승격, main과 태그, hotfix, merge 커밋을 허용하는 경우, CI 트리거는 tasty의 [브랜치 전략](https://github.com/zilhak/tasty/blob/develop/docs/dev-guide/branching.md)이 정본이고 이 레포에도 그대로 적용한다. 이 문서는 이 레포에서 다른 점만 적는다.

## tasty와 다른 점

- 태그는 workspace 버전(`Cargo.toml`의 `[workspace.package] version`)으로 `v<semver>`를 쓴다(예: `v0.1.0`). 모든 크레이트가 이 버전 하나를 같이 쓴다([버전·호환](../design/versioning.md)).
- 플러그인 버전 규칙이 없다. 크레이트마다 따로 버전을 올리지 않는다.
- 사이트 승격(tasty-page)과 README 배지가 없다.
- 태그를 브랜치와 한 번에 push하지 않는다. 일반 릴리스와 hotfix 모두 버전 커밋을 먼저 push하고(일반 릴리스는 `release`, hotfix는 `main`), 그 커밋의 CI(모바일 cross check 포함)가 모두 success인 것을 본 뒤에 태그를 push한다. tasty는 `release`·`main`과 태그를, hotfix는 `main`과 태그를 `--atomic` 한 번으로 push하지만 이 레포는 그렇게 하지 않는다. 정본은 [버전·호환](../design/versioning.md#태그와-브랜치)의 태그 push 순서다(director 2946).
- 태그 push로 도는 릴리스 워크플로가 아직 없다. tasty는 태그 커밋을 git 의존의 `rev`로 고정해 쓴다([버전·호환](../design/versioning.md) "tasty가 쓰는 방식").
- 훅은 tasty보다 검사가 적다. merge 커밋 판정은 tasty와 같은 파일(`scripts/lib/merge-policy.sh`)이다([Git 훅](git-hooks.md)).
- CI 잡 구성은 [CI](ci.md)에 있다.
