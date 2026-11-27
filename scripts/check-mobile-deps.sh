#!/usr/bin/env bash
# 모바일 의존 쪽 검사(docs/design/mobile.md "의존 쪽 검사"). 한 크레이트·feature 조합의 일반 의존
# 그래프(cargo tree -e normal, 모바일 target 하나)를 패키지마다 켜진 feature와 함께 읽고, 모바일
# 빌드에 들어오면 안 되는 것을 찾는다. 이 레포의 mobile-check 단계와 앱 작업 공간이 같은 파일을 쓴다.
# 이 파일 하나만 있으면 돈다(bash, cargo, awk, grep).
#
# 사용:
#   check-mobile-deps.sh [--manifest-path <Cargo.toml>] [--app] --target <t> [--target <t> …] \
#       -p <크레이트> [-- <feature 인자…>]
#   check-mobile-deps.sh --self-test
#
#   --manifest-path   검사할 Cargo 작업 공간(또는 패키지)의 Cargo.toml. 없으면 지금 폴더의 작업 공간.
#                     cargo는 그 Cargo.toml의 폴더에서 돈다(그 폴더의 .cargo/config.toml·툴체인 고정을 따른다)
#   --app             앱 그래프 기준. tokio의 process는 그 feature를 켠 경로에 tasty-net-* 크레이트가
#                     있을 때만 실패다(앱 자신은 켜도 된다). 없으면 이 레포 기준: tokio의 process는 실패
#   --target          모바일 target(aarch64-linux-android, x86_64-linux-android, aarch64-apple-ios,
#                     aarch64-apple-ios-sim). 여러 번 줄 수 있다
#   -p                그래프의 뿌리 패키지(앱이면 앱 cdylib 크레이트)
#   -- 뒤             cargo tree에 넘기는 feature 인자. --no-default-features, --all-features,
#                     --features <목록>(--features=<목록>), -F <목록>만 받는다. 배포 빌드와 같은 인자를 준다
#
# 실패(1): russh-config 패키지 줄이 있다, tasty-net-ssh의 feature에 system-ssh가 있다, tokio의 feature에
# process가 있다(--app이면 위 조건). 판정 불가(2): 인자 오류(모바일이 아닌 target, -- 뒤의 feature
# 인자가 아닌 것 포함), cargo tree 실패(종료 코드가 0이 아님), 출력에 뿌리 패키지 줄이 없음, 출력에 모양을
# 모르는 줄이 있음, 판정 실패. 통과는 0이다. cargo 명령은 --locked로 돌므로 Cargo.lock이 있어야 한다.
set -euo pipefail

TAG="[mobile-deps]"
SELF="$(cd "$(dirname "$0")" && pwd -P)/$(basename "$0")"

usage() {
    sed -n '7,20p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

MANIFEST=""
MANIFEST_DIR=""
APP=0
PKG=""
TARGETS=()
FEATS=()
MOBILE_TARGETS=(aarch64-linux-android x86_64-linux-android aarch64-apple-ios aarch64-apple-ios-sim)

# -- 뒤의 인자는 feature 선택만 받는다. --prune·-e·--depth·-i·--invert처럼 cargo tree의 출력 범위를
# 바꾸는 인자는 금지 의존을 그래프에서 지울 수 있으므로 판정 불가다.
check_feature_args() {
    local a
    while [[ $# -gt 0 ]]; do
        a=$1
        case "$a" in
            --no-default-features | --all-features) shift ;;
            --features=?*) shift ;;
            --features | -F)
                if [[ $# -lt 2 || "$2" == -* || -z "$2" ]]; then
                    echo "$TAG 판정 불가: $a 뒤에 feature 목록이 없다" >&2
                    return 2
                fi
                shift 2
                ;;
            *)
                echo "$TAG 판정 불가: -- 뒤에는 feature 인자(--no-default-features, --all-features, --features <목록>, -F <목록>)만 준다: $a" >&2
                return 2
                ;;
        esac
    done
}

# cargo tree 호출. 자체 시험이 이 함수를 가짜로 바꿔 끼운다. --manifest-path가 있으면 그 폴더에서
# 돌린다. cargo는 지금 폴더에서 .cargo/config.toml을 찾고 rustup도 지금 폴더로 툴체인을 고르므로,
# 앱 작업 공간 밖에서 부르면 앱의 소스 대체·patch 설정과 고정 툴체인이 빠진다.
cargo_tree() {
    if [[ -n "$MANIFEST" ]]; then
        (cd "$MANIFEST_DIR" && cargo tree --locked --manifest-path "$MANIFEST" "$@")
    else
        cargo tree --locked "$@"
    fi
}

# 패키지마다 한 줄: "<이름> v<판> [(출처)]|<feature,…>" (뒤에 " (*)"가 붙을 수 있다).
feature_tree() {
    local t=$1
    cargo_tree --target "$t" -p "$PKG" ${FEATS[@]+"${FEATS[@]}"} -e normal -f '{p}|{f}' --prefix none
}

# tokio 한 판의 feature를 켠 쪽을 거꾸로 본 나무. 줄 앞의 숫자가 깊이다.
inverted_tokio() {
    local t=$1 ver=$2
    cargo_tree --target "$t" -p "$PKG" ${FEATS[@]+"${FEATS[@]}"} -e normal,features -i "tokio@$ver" \
        --no-dedupe --prefix depth -f '{p}'
}

# feature 나무 판정. 위반을 "위반: …" 줄로, 앱 기준에서 process가 켜진 tokio 판을 "tokio-process <판>"
# 줄로, 모양을 모르는 줄을 "unknown <줄 번호>: <줄>"로, 끝에 "packages <수>"를 찍는다. 빈 줄만 건너뛴다.
judge_features() {
    awk -v app="$APP" '
        {
            line = $0
            sub(/ \(\*\)$/, "", line)
            if (line == "") next
            # "<이름> v<판>[ (<출처>)…]|<feature,…>"가 아니면 모르는 줄이다(앞 공백, | 없음 등).
            if (line !~ /^[A-Za-z0-9_-]+ v[0-9][^ |]*( \([^()|]*\))*\|[A-Za-z0-9_,+.-]*$/) {
                print "unknown " NR ": " $0
                next
            }
            bar = index(line, "|")
            pkg = substr(line, 1, bar - 1)
            feats = "," substr(line, bar + 1) ","
            split(pkg, w, " ")
            name = w[1]
            ver = w[2]
            sub(/^v/, "", ver)
            if (!(pkg in seen)) { seen[pkg] = 1; n++ }
            if (name == "russh-config" && !(("rc " pkg) in told)) {
                told["rc " pkg] = 1
                print "위반: " pkg " 패키지가 그래프에 있다(ProxyCommand를 실행하는 OpenSSH 설정 해석)"
            }
            if (name == "tasty-net-ssh" && index(feats, ",system-ssh,") && !(("ss " pkg) in told)) {
                told["ss " pkg] = 1
                print "위반: " pkg "의 feature에 system-ssh가 있다"
            }
            if (name == "tokio" && index(feats, ",process,") && !(("tk " pkg) in told)) {
                told["tk " pkg] = 1
                if (app == 1) print "tokio-process " ver
                else print "위반: " pkg "의 feature에 process가 있다"
            }
        }
        END { print "packages " n + 0 }
    '
}

# 거꾸로 본 나무에서 tokio feature "process"(깊이 1) 아래에 tasty-net-* 패키지나 그 feature 노드가 있으면 그 경로를
# "위반: …" 줄로 찍는다. 그 노드가 없으면 "no-process-node"를 찍고, 모양을 모르는 줄(깊이 숫자로 시작하지
# 않음, 노드가 "<이름> v<판>…"이나 "<이름> feature \"<f>\"[ (<표시>)]"가 아님, 깊이가 한 번에 둘 이상 늘어남)은
# "unknown <줄 번호>: <줄>"로 찍고, 끝에 "end"를 찍는다. 빈 줄만 건너뛴다.
judge_inverted() {
    awk '
        BEGIN { prev = -1 }
        {
            if ($0 == "") next
            if (!match($0, /^[0-9]+/)) { print "unknown " NR ": " $0; next }
            d = substr($0, 1, RLENGTH) + 0
            text = substr($0, RLENGTH + 1)
            sub(/ \(\*\)$/, "", text)
            if (text !~ /^[A-Za-z0-9_-]+ (v[0-9][^ ]*( \([^()]*\))*|feature "[^"]*"( \([^()]*\))?)$/ || d > prev + 1) {
                print "unknown " NR ": " $0
                next
            }
            prev = d
            path[d] = text
            if (d == 1) { inside = (text == "tokio feature \"process\""); if (inside) found = 1; next }
            if (inside && d > 1) {
                split(text, w, " ")
                # 패키지 노드(<이름> v<판>)와 feature 노드(<이름> feature "<f>") 모두 그 크레이트를
                # 거친 경로다. 크레이트가 자기 feature로 tokio/process를 넘겨 켜면 feature 노드만 남는다.
                if (w[1] ~ /^tasty-net-/ && (w[2] ~ /^v/ || w[2] == "feature")) {
                    chain = path[1]
                    for (k = 2; k <= d; k++) chain = chain " <- " path[k]
                    if (!(chain in told)) { told[chain] = 1; print "위반: tokio의 process를 켠 경로에 " w[1] "가 있다: " chain }
                }
            }
        }
        END { if (!found) print "no-process-node"; print "end" }
    '
}

# 글자 $2에서 $1에 맞는 줄. 맞는 줄이 없으면 빈 출력과 0, grep이 실패하면(2 이상) 2.
pick_lines() {
    local out rc=0
    out=$(grep -e "$1" <<<"$2") || rc=$?
    [[ $rc -le 1 ]] || return 2
    printf '%s' "$out"
}

# target 하나의 판정. 통과 0, 위반 1, 판정 불가 2. 이 함수는 ||의 왼쪽에서 불리므로 errexit가
# 꺼져 있다. 그래서 명령마다 종료 코드와 필요한 출력 줄을 직접 본다.
check_target() {
    local t=$1 tree rc=0 verdict viol count vers v inv inv_rc unknown more
    tree=$(feature_tree "$t") || rc=$?
    if [[ $rc -ne 0 ]]; then
        echo "$TAG 판정 불가: cargo tree가 실패했다(target $t, 종료 코드 $rc)" >&2
        return 2
    fi
    if ! grep -q "^$PKG v[^|]*|" <<<"$tree"; then
        echo "$TAG 판정 불가: cargo tree 출력에 $PKG 줄이 없다(target $t)" >&2
        return 2
    fi
    if ! verdict=$(judge_features <<<"$tree"); then
        echo "$TAG 판정 불가: feature 나무 판정이 실패했다(target $t)" >&2
        return 2
    fi
    if ! unknown=$(pick_lines '^unknown ' "$verdict"); then
        echo "$TAG 판정 불가: feature 나무 판정의 출력을 읽지 못했다(target $t)" >&2
        return 2
    fi
    if [[ -n "$unknown" ]]; then
        printf '  %s\n' "$unknown" >&2
        echo "$TAG 판정 불가: feature 나무에 모양을 모르는 줄이 있다(target $t)" >&2
        return 2
    fi
    if ! count=$(sed -n 's/^packages \([1-9][0-9]*\)$/\1/p' <<<"$verdict") || [[ -z "$count" ]]; then
        echo "$TAG 판정 불가: feature 나무 판정이 패키지 줄을 하나도 읽지 못했다(target $t)" >&2
        return 2
    fi
    if ! viol=$(pick_lines '^위반: ' "$verdict") || ! vers=$(sed -n 's/^tokio-process //p' <<<"$verdict"); then
        echo "$TAG 판정 불가: feature 나무 판정의 출력을 읽지 못했다(target $t)" >&2
        return 2
    fi
    for v in $vers; do
        inv_rc=0
        inv=$(inverted_tokio "$t" "$v") || inv_rc=$?
        if [[ $inv_rc -ne 0 ]]; then
            echo "$TAG 판정 불가: tokio@$v를 거꾸로 본 cargo tree가 실패했다(target $t, 종료 코드 $inv_rc)" >&2
            return 2
        fi
        if ! inv=$(judge_inverted <<<"$inv") || ! grep -qx 'end' <<<"$inv"; then
            echo "$TAG 판정 불가: tokio@$v를 거꾸로 본 나무의 판정이 실패했다(target $t)" >&2
            return 2
        fi
        if ! unknown=$(pick_lines '^unknown \|^no-process-node$' "$inv") || [[ -n "$unknown" ]]; then
            printf '  %s\n' "$unknown" >&2
            echo "$TAG 판정 불가: tokio@$v를 거꾸로 본 나무에 tokio feature \"process\"가 없거나 모양을 모르는 줄이 있다(target $t)" >&2
            return 2
        fi
        if ! more=$(pick_lines '^위반: ' "$inv"); then
            echo "$TAG 판정 불가: tokio@$v를 거꾸로 본 나무 판정의 출력을 읽지 못했다(target $t)" >&2
            return 2
        fi
        viol+=$'\n'$more
    done
    viol=$(pick_lines . "$viol") || return 2
    local what="$t $PKG ${FEATS[*]:-(기본 feature)}" mode="이 레포 기준"
    [[ $APP -eq 1 ]] && mode="앱 기준, process를 켠 tokio ${vers:-없음}"
    if [[ -n "$viol" ]]; then
        printf '  %s\n' "$viol" >&2
        echo "$TAG 위반 — $what ($mode, 패키지 $count개)" >&2
        return 1
    fi
    echo "$TAG 통과 — $what ($mode, 패키지 $count개)" >&2
    return 0
}

# 자체 시험: 가짜 cargo tree(출력과 종료 코드)로 판정을 본다.
self_test() {
    local root='app v0.1.0 (/w/app)|default'
    local tk='tokio v1.54.1|bytes,io-util,net,rt,sync,time'
    local tkp='tokio v1.54.1|bytes,io-util,net,process,rt,sync,time'
    local ssh_e='tasty-net-ssh v0.1.0 (/w/crates/tasty-net-ssh)|embedded-ssh'
    local ssh_s='tasty-net-ssh v0.1.0 (/w/crates/tasty-net-ssh)|default,system-ssh (*)'
    local rc_line='russh-config v0.50.0|'
    local inv_app='0tokio v1.54.1;1tokio feature "process";2app v0.1.0 (/w/app);1tokio feature "rt";2tasty-net-p2p v0.1.0 (/w/p2p);3app v0.1.0 (/w/app)'
    local inv_net='0tokio v1.54.1;1tokio feature "process";2tokio feature "full";3tasty-net-transport v0.1.0 (/w/t);4app v0.1.0 (/w/app);1tokio feature "rt"'
    local inv_feat='0tokio v1.54.1;1tokio feature "process";2tasty-net-ssh feature "embedded-ssh";3app v0.1.0 (/w/app);4app feature "default" (command-line)'
    local inv_none='0tokio v1.54.1;1tokio feature "rt";2app v0.1.0 (/w/app)'
    local inv_space='0tokio v1.54.1;1tokio feature "process";2tokio feature "full"; 3tasty-net-transport v0.1.0 (/w/t);4app v0.1.0 (/w/app)'
    local root_nobar='app v0.1.0 (/w/app)'
    local failed=0 name app want st_tree st_trc st_inv st_irc st_awk got
    # 이름#--app(0·1)#기대 종료 코드#feature 나무#그 종료 코드#거꾸로 본 나무#그 종료 코드#도구 고장
    # (나무의 줄은 ;로 나눈다. 도구 고장: fail은 awk가 아무것도 찍지 않고 3으로 끝나고, empty는 0으로 끝나며
    # 아무것도 찍지 않는다. fail-inverted는 거꾸로 본 나무 판정이 3으로 끝난다. fail-sed는 tokio 판을 뽑는
    # sed가, fail-grep은 위반 줄을 찾는 grep이 3·2로 끝난다)
    local cases=(
        "정상: process 없는 tokio와 embedded-ssh#0#0#$root;$tk;$ssh_e#0##0"
        "tokio의 process(이 레포 기준)#0#1#$root;$tkp#0##0"
        "중복 표시(*)가 붙은 tasty-net-ssh의 system-ssh#0#1#$root;$tk;$ssh_s#0##0"
        "russh-config 줄#0#1#$root;$rc_line#0##0"
        "tokio 없는 그래프#0#0#$root;$ssh_e#0##0"
        "cargo tree가 줄을 찍고 101로 끝난다#0#2#$root;$tk#101##0"
        "출력에 뿌리 줄이 없다#0#2#$tk;$ssh_e#0##0"
        "앱이 직접 켠 tokio의 process(앱 기준)#1#0#$root;$tkp;$ssh_e#0#$inv_app#0"
        "tasty-net-transport를 거쳐 켠 tokio의 process(앱 기준)#1#1#$root;$tkp#0#$inv_net#0"
        "앱 기준에서도 system-ssh는 실패#1#1#$root;$tk;$ssh_s#0##0"
        "앱 기준에서도 russh-config는 실패#1#1#$root;$rc_line#0##0"
        "tasty-net-ssh의 feature 노드를 거쳐 켠 tokio의 process(앱 기준)#1#1#$root;$tkp#0#$inv_feat#0"
        "거꾸로 본 나무에 process 노드가 없다(앱 기준)#1#2#$root;$tkp#0#$inv_none#0"
        "거꾸로 본 cargo tree가 실패한다(앱 기준)#1#2#$root;$tkp#0#$inv_app#101"
        "feature 판정의 awk가 실패한다#0#2#$root;$tkp#0##0#fail"
        "feature 판정의 awk가 아무것도 찍지 않는다#0#2#$root;$tkp#0##0#empty"
        "거꾸로 본 나무 판정의 awk가 실패한다(앱 기준)#1#2#$root;$tkp#0#$inv_net#0#fail-inverted"
        "거꾸로 본 나무의 줄 앞에 공백이 있다(앱 기준)#1#2#$root;$tkp#0#$inv_space#0"
        "feature 나무의 뿌리 줄에 |가 없다#0#2#$root_nobar;$tk#0##0"
        "feature 나무에 모르는 모양의 줄이 있다#0#2#$root;$tk;  $ssh_s#0##0"
        "tokio 판을 뽑는 sed가 실패한다(앱 기준)#1#2#$root;$tkp#0#$inv_net#0#fail-sed"
        "위반 줄을 찾는 grep이 실패한다#0#2#$root;$tkp#0##0#fail-grep"
    )
    for c in "${cases[@]}"; do
        st_awk=""
        IFS='#' read -r name app want st_tree st_trc st_inv st_irc st_awk <<<"$c"
        got=0
        (
            # shellcheck disable=SC2329  # check_target이 부른다
            cargo_tree() {
                local out=$st_tree rc=$st_trc
                if [[ " $* " == *" -i "* ]]; then
                    out=$st_inv
                    rc=$st_irc
                fi
                [[ -n "$out" ]] && tr ';' '\n' <<<"$out"
                return "$rc"
            }
            case "$st_awk" in
                fail) awk() { return 3; } ;;
                empty) awk() { cat >/dev/null; } ;;
                fail-inverted)
                    # shellcheck disable=SC2329  # check_target이 부른다
                    judge_inverted() { return 3; }
                    ;;
                fail-sed)
                    # shellcheck disable=SC2329  # check_target이 부른다
                    sed() { [[ "$*" == *tokio-process* ]] && return 3; command sed "$@"; }
                    ;;
                fail-grep)
                    # shellcheck disable=SC2329  # check_target이 부른다
                    grep() { [[ "$*" == *'^위반: '* ]] && return 2; command grep "$@"; }
                    ;;
            esac
            PKG=app
            APP=$app
            check_target aarch64-linux-android
        ) >/dev/null 2>&1 || got=$?
        if [[ "$got" == "$want" ]]; then
            echo "$TAG 자체 시험 통과: $name" >&2
        else
            echo "$TAG 자체 시험 실패: $name (기대 $want, 결과 $got)" >&2
            failed=1
        fi
    done
    cli_self_test || failed=1
    return "$failed"
}

# 인자 처리와 cargo를 부르는 자리의 자체 시험: PATH 앞에 둔 가짜 cargo로 이 스크립트를 따로 돌린다.
# 가짜 cargo는 FAKE_CARGO_PWD가 있으면 지금 폴더가 그 폴더일 때만 뿌리 줄을 찍고, 아니면 101로 끝난다.
cli_self_test() {
    local tmp failed=0 c name want pwd_mode args got want_pwd
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/mobile-deps-test.XXXXXX")
    mkdir -p "$tmp/bin" "$tmp/app" "$tmp/elsewhere"
    : >"$tmp/app/Cargo.toml"
    cat >"$tmp/bin/cargo" <<'FAKE'
#!/usr/bin/env bash
if [[ -n "${FAKE_CARGO_PWD:-}" && "$(pwd -P)" != "$FAKE_CARGO_PWD" ]]; then
    echo "fake cargo: 지금 폴더 $(pwd -P), 기대 $FAKE_CARGO_PWD" >&2
    exit 101
fi
echo 'app v0.1.0 (/w/app)|default'
FAKE
    chmod +x "$tmp/bin/cargo"
    # 이름#기대 종료 코드#cargo의 폴더 확인(app·없음)#인자(공백으로 나눈다, @APP@은 앱 Cargo.toml)
    local cases=(
        "다른 폴더에서 부른 --manifest-path는 그 폴더에서 cargo를 돌린다#0#app#--manifest-path @APP@ --app --target aarch64-linux-android -p app"
        "상대 경로 --manifest-path도 그 폴더에서 돈다#0#app#--manifest-path ../app/Cargo.toml --target aarch64-linux-android -p app"
        "없는 --manifest-path#2##--manifest-path ../none/Cargo.toml --target aarch64-linux-android -p app"
        "모바일 target 네 개#0##--target aarch64-linux-android --target x86_64-linux-android --target aarch64-apple-ios --target aarch64-apple-ios-sim -p app"
        "호스트 target#2##--target x86_64-unknown-linux-gnu -p app"
        "모바일 target 뒤의 호스트 target#2##--target aarch64-linux-android --target x86_64-apple-darwin -p app"
        "feature 인자들#0##--target aarch64-linux-android -p app -- --no-default-features --features a,b -F c --features=d --all-features"
        "-- 뒤의 --prune#2##--target aarch64-linux-android -p app -- --prune tokio"
        "-- 뒤의 -e#2##--target aarch64-linux-android -p app -- -e no-build"
        "-- 뒤의 --depth#2##--target aarch64-linux-android -p app -- --depth 1"
        "-- 뒤의 -i#2##--target aarch64-linux-android -p app -- -i tokio"
        "-- 뒤의 모르는 인자#2##--target aarch64-linux-android -p app -- --frobnicate"
        "feature 목록 없는 --features#2##--target aarch64-linux-android -p app -- --features"
        "feature 목록 대신 인자가 온 -F#2##--target aarch64-linux-android -p app -- -F --prune"
    )
    for c in "${cases[@]}"; do
        IFS='#' read -r name want pwd_mode args <<<"$c"
        args=${args//@APP@/$tmp/app/Cargo.toml}
        want_pwd=""
        [[ "$pwd_mode" == app ]] && want_pwd=$(cd "$tmp/app" && pwd -P)
        got=0
        # shellcheck disable=SC2086  # 인자는 공백으로 나눈다
        (cd "$tmp/elsewhere" && PATH="$tmp/bin:$PATH" FAKE_CARGO_PWD="$want_pwd" bash "$SELF" $args) \
            >/dev/null 2>&1 || got=$?
        if [[ "$got" == "$want" ]]; then
            echo "$TAG 자체 시험 통과: $name" >&2
        else
            echo "$TAG 자체 시험 실패: $name (기대 $want, 결과 $got)" >&2
            failed=1
        fi
    done
    rm -rf "${tmp:?}"
    return "$failed"
}

if [[ "${1:-}" == "--self-test" ]]; then
    [[ $# -eq 1 ]] || usage
    self_test
    exit $?
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --manifest-path)
            [[ $# -ge 2 ]] || usage
            MANIFEST=$2
            shift 2
            ;;
        --app) APP=1; shift ;;
        --target)
            [[ $# -ge 2 ]] || usage
            TARGETS+=("$2")
            shift 2
            ;;
        -p)
            [[ $# -ge 2 ]] || usage
            PKG=$2
            shift 2
            ;;
        --)
            shift
            FEATS=("$@")
            break
            ;;
        *)
            echo "$TAG 모르는 인자: $1" >&2
            usage
            ;;
    esac
done
[[ -n "$PKG" && ${#TARGETS[@]} -gt 0 ]] || usage
for t in "${TARGETS[@]}"; do
    known=0
    for k in "${MOBILE_TARGETS[@]}"; do [[ "$k" == "$t" ]] && known=1; done
    if [[ $known -eq 0 ]]; then
        echo "$TAG 판정 불가: 모바일 target이 아니다: $t (target: ${MOBILE_TARGETS[*]})" >&2
        exit 2
    fi
done
check_feature_args ${FEATS[@]+"${FEATS[@]}"} || exit 2
if [[ -n "$MANIFEST" ]]; then
    if [[ ! -f "$MANIFEST" ]]; then
        echo "$TAG 판정 불가: Cargo.toml이 없다: $MANIFEST" >&2
        exit 2
    fi
    MANIFEST_DIR=$(cd "$(dirname "$MANIFEST")" && pwd -P)
    MANIFEST="$MANIFEST_DIR/$(basename "$MANIFEST")"
fi

worst=0
for t in "${TARGETS[@]}"; do
    rc=0
    check_target "$t" || rc=$?
    [[ $rc -gt $worst ]] && worst=$rc
done
exit "$worst"
