#!/usr/bin/env bash
# 추적된 .sh 파일과 shell shebang 파일을 ShellCheck warning 수준으로 검사하고, ASCII 밖 글자가 바로 뒤에
# 붙은 $이름(아래 scan_var_nonascii)이 없는지 본다. 인자를 주면 그 목록에 같은 선택 규칙을 적용한다.
# --self-test는 그 판정만 시험한다. 통과 0, 검사 실패 1, 도구·대상 부재 2.

set -e -o pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
[ -n "$REPO_ROOT" ] || { echo "[shell-assets] 판정 불가: git 저장소가 아니다." >&2; exit 2; }
cd "$REPO_ROOT"

# macOS의 bash 3.2는 UTF-8 로케일에서 $이름 바로 뒤의 ASCII 밖 글자 바이트를 이름의 일부로 읽어, set -u면
# unbound variable로 끝나고 아니면 빈 값을 펼친다. 그래서 그 자리는 ${이름}으로 쓴다. 바이트로 읽으려고
# C 로케일에서 파일마다 모든 줄을 본다. 주석·따옴표·here-doc은 따라가지 않는다. 그래서 bash가
# 펼치지 않는 자리(주석, 작은따옴표 안의 awk·sed 코드, 인용된 here-doc 본문, \$)도 잡힌다. 그런 자리는
# ${이름}으로 쓰거나, 바로 윗줄에 이유와 함께 "# shell-assets: allow-var-nonascii"를 적는다.
# - 줄 이음: 홀수 개의 \로 끝나는 줄은 끝 \를 빼고 다음 줄과 이어 붙인다(여러 번 이어져도 같다). 따옴표는
#   보지 않으므로 작은따옴표 안에서도 이어 붙인다. 판정은 이어 붙인 논리 줄 전체에 하고, 찾으면 그 논리
#   줄의 첫 줄 번호와 이어 붙인 내용을 찍는다.
# - 표시: 물리 줄 하나로 정한다. 줄 전체 주석으로 "# shell-assets: allow-var-nonascii" 바로 뒤에 공백·탭이나
#   줄 끝이 오고(allow-var-nonasciiX는 표시가 아니다), 홀수 개의 \로 끝나지 않으며, 앞 줄에 이어진 줄이
#   아닐 때만 표시다. 이어 붙인 문자열에서는 표시를 찾지 않는다(bash는 주석 끝 \로 줄을 잇지 않는다).
#   표시는 바로 다음 논리 줄의 첫 물리 줄만 면제한다. 그 논리 줄이 이어졌으면 둘째 물리 줄부터 이어 붙인
#   나머지는 검사하고 그 첫 줄 번호를 찍는다. 표시 줄 자신도 검사한다.
# 찾은 줄을 "파일:줄: 내용"으로 찍고, awk가 실패하면 2로 끝난다.
scan_var_nonascii() {
    local f rc=0
    for f in "$@"; do
        # 파일 이름의 = 를 awk가 변수 대입으로 읽지 않게, 상대 경로는 ./로 시작하게 한다.
        case "$f" in /* | ./*) ;; *) f="./$f" ;; esac
        LC_ALL=C awk '
            # s가 홀수 개의 \로 끝나는가(줄 이음)
            function odd_bs(s,    k, len) {
                len = length(s)
                k = 0
                while (k < len && substr(s, len - k, 1) == "\\") k++
                return k % 2
            }
            # 이어 붙인 논리 줄 하나를 판정한다. 표시 아래(lallow)면 첫 물리 줄을 빼고 이어 붙인 나머지만 본다.
            function judge(    re) {
                re = "[$][A-Za-z_][A-Za-z0-9_]*[^\001-\177]"
                if (!lallow) { if (line ~ re) print FILENAME ":" start ": " line }
                else if (rstart && rest ~ re) print FILENAME ":" rstart ": " rest
            }
            # 물리 줄 s가 표시인가(이어지지 않는 한 줄)
            function marker(s) {
                return (s ~ /^[ \t]*# shell-assets: allow-var-nonascii$/ || s ~ /^[ \t]*# shell-assets: allow-var-nonascii[ \t]/)
            }
            BEGIN { allow = 0; joining = 0; line = ""; rest = ""; rstart = 0 }
            {
                if (!joining) { start = FNR; lallow = allow; line = ""; rest = ""; rstart = 0 }
                piece = odd_bs($0) ? substr($0, 1, length($0) - 1) : $0
                line = line piece
                if (FNR > start) { if (!rstart) rstart = FNR; rest = rest piece }
                if (odd_bs($0)) { joining = 1; next }
                joining = 0
                judge()
                allow = (FNR == start && marker($0))
            }
            END { if (joining) judge() }
        ' "$f" || rc=2
    done
    return "$rc"
}

# 자체 시험: 임시 파일마다 scan_var_nonascii가 찾은 줄 수를 기대값과 비교한다(awk가 실패하면 rc2).
self_test() {
    local tmp failed=0 c name want body got n=0 d='$'
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/shell-assets-test.XXXXXX") || { echo "[shell-assets] 판정 불가: 임시 폴더" >&2; return 2; }
    # 이름#찾을 줄 수#내용(\n은 줄바꿈, \t는 탭, printf %b로 쓴다). 사례의 $는 ${d}로 만들어
    # 이 파일 자신이 판정에 걸리지 않게 한다.
    local cases=(
        "한글 앞 ${d}이름#1#echo \"패키지 ${d}count개\""
        "가운뎃점 앞 ${d}이름#1#echo \"${d}ext·llvm-ar\""
        "한글 앞 ${d}{이름}#0#echo \"패키지 ${d}{count}개\""
        "공백 뒤 한글#0#echo \"${d}count 개\""
        "위치 인자 뒤 한글#0#echo \"${d}1개\""
        "줄 전체 주석도 본다#1#  # 패키지 ${d}count개"
        "윗줄 표시가 있는 작은따옴표 awk#0## shell-assets: allow-var-nonascii awk 필드다\nawk '{ print ${d}name개 }'"
        "표시 없는 작은따옴표 awk#1#awk '{ print ${d}name개 }'"
        "표시는 바로 다음 줄에만#1## shell-assets: allow-var-nonascii\necho ok\necho \"${d}v를\""
        "표시 줄 자신도 본다#1## shell-assets: allow-var-nonascii ${d}v를 때문\necho ok"
        "줄 끝 주석에 쓴 표시는 듣지 않는다#1#echo ok # shell-assets: allow-var-nonascii\necho \"${d}v를\""
        "here-doc 본문#1#cat <<EOF\n${d}v를 찾았다\nEOF"
        "마지막 줄에 LF가 없다#1#echo \"${d}dest가\""
        "ASCII만#0#echo \"${d}a${d}b_c ${d}d-e\""
        "인용하지 않은 here-doc의 주석 모양 줄#1#cat <<EOF\n# ${d}count개\nEOF"
        "여러 줄 큰따옴표 안의 주석 모양 줄#1#echo \"first\n# ${d}count개\n\""
        "인용된 here-doc 본문도 본다#2#cat <<'EOF'\n# ${d}count개\n${d}v를\nEOF"
        "큰따옴표로 인용된 here-doc 본문도 본다#1#cat <<\"EOF\"\n${d}v를\nEOF"
        "<<- 본문과 탭 들여쓴 구분자 뒤 줄#2#cat <<-EOF\n\t${d}v를\n\tEOF\necho \"${d}w가\""
        "here-doc이 끝난 뒤 주석도 본다#1#cat <<EOF\nok\nEOF\n# ${d}count개"
        "줄 끝 주석의 작은따옴표 뒤 주석도 본다#1#echo a # don't\n# ${d}count개"
        "<<< 뒤 주석도 본다#1#grep x <<<\"ok\"\n# ${d}count개"
        "닫히지 않은 큰따옴표 뒤 주석도 본다#1#echo \"${d}{a}\n# ${d}count개"
        "here-doc 본문 줄 이음 뒤 구분자 모양 줄#1#count=3\n: \"${d}count\"\ncat <<EOF\nprefix\\\\\nEOF\n# ${d}count개\nEOF"
        "줄 이음 뒤 주석 모양 줄#1#count=3\nprintf '%s\\\\n' abc\\\\\n# ${d}count개"
        "줄 이음으로 붙는 ${d}이름#1#echo \"${d}v\\\\\n개\""
        "줄 이음으로 붙는 ${d}이름(이스케이프된 달러도 잡는다)#1#echo \\\\${d}v\\\\\n개"
        "줄 이음 경계 뒤 ASCII#0#echo \"${d}v\\\\\nok\""
        "줄 이음 뒤로 이름이 이어진다#1#echo \"${d}v\\\\\nok개\""
        "줄 끝에 달러만 남은 줄 이음#1#echo \"${d}\\\\\nv개\""
        "줄 이음 두 번#1#echo \"${d}v\\\\\n\\\\\n개\""
        "빈 이음 줄을 낀 줄 이음#1#count=3\nprintf '%s\\\\n' \"${d}count\\\\\n\\\\\n개\""
        "작은따옴표 안에서도 이어 붙인다#1#awk '{ print ${d}v\\\\\n개 }'"
        "파일 끝에서 끝나지 않은 줄 이음#1#echo \"${d}v개\\\\"
        "오타 표시는 표시가 아니다#1## shell-assets: allow-var-nonasciiTypo\nprintf '%s\\\\n' \"${d}count개\""
        "탭이 뒤따르는 표시#0## shell-assets: allow-var-nonascii\t이유\nawk '{ print ${d}name개 }'"
        "표시는 아래 논리 줄의 첫 물리 줄에만 듣는다#1## shell-assets: allow-var-nonascii 이유\necho a\\\\\n\"${d}v개\""
        "표시 아래 첫 물리 줄은 이어져도 면제된다#0## shell-assets: allow-var-nonascii 이유\nawk '{ print ${d}v개 }' \\\\\n  file"
        "백슬래시로 끝나는 표시 줄은 표시가 아니다#1#v=1\n# shell-assets: allow-var-nonascii 이유 \\\\\necho ok\necho \"${d}v개\""
        "표시 아래 주석 끝 백슬래시 뒤의 명령은 본다#1#count=3\n# shell-assets: allow-var-nonascii 문서 예시\n# 예시: ${d}count개\\\\\nprintf '%s\\\\n' \"${d}count개\""
        "이어 붙여 만든 표시는 표시가 아니다#1#count=3\n# shell-assets: allow-var-nonasci\\\\\ni () { :; }\nprintf '%s\\\\n' \"${d}count개\""
        "앞 줄에 이어진 표시 모양 줄은 표시가 아니다#1#echo a \\\\\n# shell-assets: allow-var-nonascii\necho \"${d}v개\""
        "표시 아래 논리 줄 다음 줄에는 듣지 않는다#1## shell-assets: allow-var-nonascii 이유\necho a\\\\\nb\necho \"${d}v개\""
        "줄 끝 이스케이프된 백슬래시는 경계가 아니다#0#echo ${d}v\\\\\\\\\n개"
        "줄 이음 뒤에 시작하는 here-doc#1#cat <<EOF \\\\\n | tr a b\n# ${d}count개\nEOF"
        "<<- 본문 줄 이음 뒤 탭 구분자 모양 줄#1#cat <<-EOF\n\tA\\\\\n\tEOF\n\t# ${d}count개\n\tEOF"
        "작은따옴표 구분자 안의 백슬래시#1#count=3\nEOF() { :; }\ncat <<'E\\\\OF'\ntext\nE\\\\OF\nprintf '%s\\\\n' \"${d}count개\"\nEOF"
        "큰따옴표 구분자의 백슬래시 달러 본문도 본다#2#cat <<\"E\\\\${d}F\"\n${d}v를\nE${d}F\necho \"${d}w가\""
        "따옴표 밖 백슬래시 구분자 본문도 본다#1#cat <<E\\\\OF\n${d}v를\nEOF"
        "줄 이음 뒤 닫는 괄호 다음 주석 속 here-doc 표지#1#count=3\nEOF() { :; }\n: \"${d}count\"\n(:)\\\\\n# <<'EOF'\nprintf '%s\\\\n' \"${d}count개\"\nEOF"
        "닫는 괄호 바로 뒤 주석 속 here-doc 표지#1#count=3\nEOF() { :; }\n(:)# <<'EOF'\nprintf '%s\\\\n' \"${d}count개\"\nEOF"
        "이스케이프된 닫는 괄호 바로 뒤 주석 모양 글자#1#count=3\nprintf '%s\\\\n' \\\\)# \"\ncat <<'EOF'\n${d}count개\nEOF\n)#\""
        "이스케이프된 닫는 괄호와 줄 이음 뒤 주석 모양 글자#1#count=3\nprintf '%s\\\\n' \\\\)\\\\\n# \"\ncat <<'EOF'\n${d}count개\nEOF\n)#\""
        "이스케이프된 공백 바로 뒤 주석 모양 글자#1#printf '%s\\\\n' a\\\\ # \"\ncat <<'EOF'\n${d}v를\nEOF\n\""
        "닫은 따옴표 바로 뒤 주석 모양 글자#1#echo \"a\"# \"\ncat <<'EOF'\n${d}v를\nEOF\n\""
        "공백 뒤 줄 이음 다음 줄의 주석#1#printf x \\\\\n# <<'EOF'\n${d}v를\nEOF"
        "이스케이프된 < 뒤 <는 here-doc이 아니다#1#cat \\\\<<'EOF'\n${d}v를\nEOF"
        "<< 뒤 이스케이프된 < 본문도 본다#2#cat <<\\\\<X\n${d}v를\n<X\necho \"${d}w가\""
        "<< 뒤 이스케이프된 - 본문도 본다#2#cat <<\\\\-EOF\n\t${d}v를\n\t-EOF\n-EOF\necho \"${d}w가\""
        "구분자 안의 이스케이프된 닫는 괄호 본문도 본다#2#cat <<E\\\\)F\n${d}v를\nE)F\necho \"${d}w가\""
        "구분자 안의 이스케이프된 달러 본문도 본다#2#cat <<\\\\${d}X\n${d}v를\n${d}X\necho \"${d}w가\""
        "줄 끝 이스케이프된 백슬래시 뒤 주석도 본다#1#echo a\\\\\\\\\n# ${d}count개"
        "here-doc 본문 끝 이스케이프된 백슬래시 뒤 주석도 본다#1#cat <<EOF\na\\\\\\\\\nEOF\n# ${d}count개"
        "달러 작은따옴표 안의 이스케이프된 작은따옴표#1#echo ${d}'it\\\\'s' # <<'EOF'\n${d}v를\nEOF"
        "여러 줄 큰따옴표가 닫힌 바로 뒤 주석 모양 글자#1#count=3\necho \"a\nb\"# \"\ncat <<'EOF'\n${d}count개\nEOF\n)#\""
        "여러 줄 작은따옴표가 닫힌 바로 뒤 주석 모양 글자#1#count=3\necho 'a\nb'# \"\ncat <<'EOF'\n${d}count개\nEOF\n)#\""
        "다음 줄에서 닫힌 큰따옴표 바로 뒤 주석 모양 글자#1#count=3\nprintf '%s\\\\n' \"a\n\"# \"\ncat <<'EOF'\n${d}count개\nEOF\n)#\""
        "달러 달러 뒤 작은따옴표#1#count=3\nprintf '%s\\\\n' ${d}${d}'x\\\\'\nprintf '%s\\\\n' \"'\ncat <<'EOF'\n${d}count개\nEOF\n)#\""
    )
    for c in "${cases[@]}"; do
        IFS='#' read -r name want body <<<"$c"
        n=$((n + 1))
        printf '%b' "$body" >"$tmp/case$n.sh"
        if got=$(scan_var_nonascii "$tmp/case$n.sh"); then
            got=$(printf '%s' "$got" | grep -c . || true)
        else
            got="rc$?"
        fi
        if [ "$got" = "$want" ]; then
            echo "[shell-assets] 자체 시험 통과: $name" >&2
        else
            echo "[shell-assets] 자체 시험 실패: $name (기대 ${want}, 결과 ${got})" >&2
            failed=1
        fi
    done
    rm -rf "${tmp:?}"
    [ "$failed" -eq 0 ] && echo "[shell-assets] 자체 시험 통과 — 사례 ${n}개"
    return "$failed"
}

if [ "${1:-}" = "--self-test" ]; then
    [ "$#" -eq 1 ] || { echo "[shell-assets] 사용: check-shell-assets.sh [--self-test | <파일>…]" >&2; exit 2; }
    self_test
    exit $?
fi

if ! command -v shellcheck >/dev/null 2>&1; then
    echo "[shell-assets] 판정 불가: shellcheck 이 PATH 에 없다." >&2
    echo "  받는 법: bash scripts/install-shellcheck.sh   (배포판 패키지도 된다)" >&2
    exit 2
fi

is_shell_asset() {
    case "$1" in *.sh) return 0 ;; esac
    [ -f "$1" ] || return 1
    # grep -q가 producer를 조기 종료해 SIGPIPE를 일으키지 않도록 먼저 내용을 받는다.
    local first=""
    IFS= read -r first < "$1" || true
    grep -qE '^#!.*[ /](ba|z)?sh( |$)' <<<"$first"
}

select_shell_assets() {
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        if is_shell_asset "$f"; then printf '%s\n' "$f"; fi
    done
    return 0
}

collect_corpus() {
    {
        git ls-files -- '*.sh'
        # 후보가 없을 때 git grep의 rc 1을 허용한다.
        git grep -lI -E '^#!' -- . 2>/dev/null || true
    } | select_shell_assets
    return 0
}

if [ "$#" -gt 0 ]; then
    TARGETS=$(printf '%s\n' "$@" | select_shell_assets | sort -u)
else
    TARGETS=$(collect_corpus | sort -u)
fi

COUNT=$(printf '%s\n' "$TARGETS" | grep -c . || true)

# 전체 검사 대상이 비면 거부한다. 인자로 받은 staged 목록에 셸 파일이 없는 경우는 통과다.
if [ "$COUNT" -eq 0 ]; then
    if [ "$#" -gt 0 ]; then
        echo "[shell-assets] 대상 0 개 — 넘어온 $# 개 중 셸 자산이 없다."
        exit 0
    fi
    echo "[shell-assets] 판정 불가: 검사할 셸 파일을 찾지 못했다. Git 파일 수집과 선택 조건을 확인해라." >&2
    exit 2
fi

VERSION=$(shellcheck --version | sed -n 's/^version: //p')

set +e
# 이유: TARGETS의 경로 목록을 개별 인자로 넘기기 위해 단어 분리를 사용한다.
# shellcheck disable=SC2086
OUT=$(shellcheck --severity=warning --format=tty $TARGETS 2>&1)
RC=$?
set -e

set +e
# shellcheck disable=SC2086  # 위와 같은 이유로 경로 목록을 단어 분리한다
VAR_OUT=$(scan_var_nonascii $TARGETS)
VAR_RC=$?
set -e
if [ "$VAR_RC" -ne 0 ]; then
    echo "[shell-assets] 판정 불가: \$이름 뒤 ASCII 밖 글자 판정의 awk가 실패했다." >&2
    exit 2
fi
VAR_COUNT=$(printf '%s\n' "$VAR_OUT" | grep -c . || true)

if [ "$RC" -ne 0 ]; then
    printf '%s\n' "$OUT" >&2
    echo >&2
    echo "[shell-assets] 위반 — 셸 자산 $COUNT 개 · shellcheck $VERSION · 문턱 warning 이상 · 잔여 0 이 기준이다." >&2
    echo "  의도한 코드면 그 줄 바로 위에 \`# shellcheck disable=<코드>\` 와 **왜인지**를 같이 적어라." >&2
    echo "  근거 없는 억제는 다음 사람에게 '누군가 판단했다' 로 읽히고, 실제로 그렇지 않았던 자리가 있었다." >&2
    exit 1
fi

if [ "$VAR_COUNT" -ne 0 ]; then
    printf '%s\n' "$VAR_OUT" | sed 's/^/  /' >&2
    echo "[shell-assets] 위반 — ASCII 밖 글자가 바로 뒤에 붙은 \$이름 ${VAR_COUNT}줄. \${이름}으로 쓴다." >&2
    echo "  macOS bash 3.2는 UTF-8에서 그 바이트를 이름으로 읽는다. 주석 등 bash가 펼치지 않는 자리도 \${이름}으로" >&2
    echo "  쓰거나, 바로 윗줄에 '# shell-assets: allow-var-nonascii <이유>'를 적는다." >&2
    exit 1
fi

echo "[shell-assets] 통과 — 셸 자산 $COUNT 개 · shellcheck $VERSION · 문턱 warning 이상 · 위반 0 · ASCII 밖 글자 앞 \$이름 0"
