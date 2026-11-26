# Git 훅

훅은 `.githooks/`에 있다. 새 clone에서 `bash scripts/dev-setup.sh`를 한 번 실행하면 `core.hooksPath`가 `.githooks`로 정해진다. 이 설정은 clone의 공용 설정이라 그 clone의 모든 worktree가 함께 쓰고, 각 worktree는 자기 checkout의 `.githooks`를 돌린다. 훅이 켜져 있지 않으면 `scripts/check.sh`가 경고한다.

훅에는 끄는 스위치가 없다. `--no-verify`나 `core.hooksPath` 변경으로 훅을 건너뛰지 않는다. 훅이 막으면 원인을 고친다.

## pre-commit

빠른 검사만 한다. 검사 목록의 정본은 `.githooks/pre-commit`의 `CHECKS` 배열이다.

| 검사 | 하는 일 | 정본 |
|---|---|---|
| M.1 | 진행 중인 merge를 커밋하면 아래 merge 커밋 판정으로 허용 여부를 본다 | `scripts/lib/merge-policy.sh` |
| A.2 | 커밋 대상에 `.rs`가 있으면 `scripts/check.sh fmt` | `scripts/check.sh` |
| A.3 | 커밋 대상 중 셸 자산(`.sh`, 셸 shebang 파일)을 shellcheck로 검사한다. shellcheck가 없어 판정하지 못해도 커밋을 막는다 | `scripts/check-shell-assets.sh` |

## pre-merge-commit

`git merge`가 merge 커밋을 만들기 직전에 merge 커밋 판정을 한다. fast-forward merge에는 돌지 않는다. 충돌을 풀고 `git commit`으로 만드는 merge 커밋은 pre-commit의 M.1이 같은 판정으로 본다.

## merge 커밋 판정

허용하는 방향은 tasty [브랜치 전략](https://github.com/zilhak/tasty/blob/develop/docs/dev-guide/branching.md)의 "merge 커밋을 허용하는 경우"다. 판정 조건의 정본은 `scripts/lib/merge-policy.sh`의 `merge_commit_allowed`다(tasty와 같은 파일). CI는 merge 커밋을 보지 않는다.

## pre-push

push 전에 아래 단계를 `scripts/check.sh`로 돌린다. 앞 단계가 실패해도 뒤 단계를 모두 돌린다. 단계 목록의 정본은 `.githooks/pre-push`다.

| 단계 | 명령 |
|---|---|
| B.1 | `scripts/check.sh fmt` |
| B.2 | `scripts/check.sh shell` |
| B.3 | `scripts/check.sh no-tasty-deps` (Python 3이 PATH에 있어야 한다: `python3` 또는 `python`) |
| B.4 | `scripts/check.sh lint` |
| B.5 | `scripts/check.sh check` |

- 시험(`cargo test`)은 돌리지 않는다. 실제 `ssh`·포트를 쓰는 시험이 있어 환경에 따라 실패할 수 있다. 시험은 [CI](ci.md)가 세 OS에서 돌린다.
- 검사 명령에는 git이 훅에 넘기는 저장소 변수(`git rev-parse --local-env-vars`)를 지우고 넘긴다.

### 로그

단계별 출력은 `$(git rev-parse --git-path hook-logs)/pre-push.<임의>/` 아래 `<단계>.log`에 남는다. 실패한 단계는 마지막 60줄과 로그 경로를 화면에 보여 준다. `summary.log`에 단계별 결과와 걸린 시간이 있다.

## 알려진 한계

tasty 훅과 같은 동작이다.

- pre-commit의 A.2·A.3은 커밋 대상 파일 이름을 인덱스에서 가져오지만 검사는 작업 트리의 파일 내용으로 한다. 파일 일부만 스테이징하면 커밋될 내용이 아니라 작업 트리의 내용이 검사된다. 그래서 인덱스에 남은 오류를 놓치거나, 작업 트리에만 있는 오류로 커밋이 막힐 수 있다. 부분 스테이징을 했으면 커밋한 뒤 `bash scripts/check.sh fmt`·`bash scripts/check.sh shell`로 다시 본다.
- A.3은 커밋 대상 경로 목록을 공백으로 나눠 넘긴다. 공백이 든 경로의 셸 파일은 제대로 검사되지 않는다. 셸 파일 이름에 공백을 쓰지 않는다.
- pre-push는 git이 넘기는 push 대상 ref를 읽지 않고 지금 checkout된 작업 트리를 검사한다. 체크아웃하지 않은 다른 브랜치를 push하거나, 커밋하지 않은 수정이 작업 트리에 있으면 실제로 보내는 커밋과 다른 내용이 검사된다. push할 브랜치를 checkout하고 작업 트리가 깨끗한 상태에서 push한다. 보낸 커밋 자체는 CI가 검사한다.

