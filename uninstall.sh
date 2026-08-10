#!/usr/bin/env bash
# uninstall.sh — safely remove iphone-use's per-user macOS installation.
#
# The default is deliberately local-only: it stops and removes product-owned
# LaunchAgents, PID-verified WDA processes, apps, logs, and ~/.iphone-use state.
# It never removes Xcode, Homebrew, iproxy, socat, or the WDA runner installed on
# the phone. Removing the phone runner requires explicit target identifiers.
set -u
umask 077

DRY_RUN=0
REMOVE_PHONE_RUNNER=0
PHONE_UDID=""
PHONE_BUNDLE_ID=""
FAILED=0

usage() {
    cat <<'EOF'
Usage:
  ./uninstall.sh [--dry-run]
  ./uninstall.sh --remove-phone-runner --udid <UDID> --bundle-id <bundle-id> [--dry-run]

Options:
  --dry-run               Show owned artifacts and actions without changing anything.
  --remove-phone-runner   Also uninstall one explicitly identified WDA runner app.
  --udid VALUE            Exact configured iPhone UDID for --remove-phone-runner.
  --bundle-id VALUE       Configured runner id or installed .xctrunner bundle id.
  -h, --help              Show this help.

The local uninstall never removes shared Xcode/Homebrew components, iproxy, or socat.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --remove-phone-runner)
            REMOVE_PHONE_RUNNER=1
            shift
            ;;
        --udid)
            [ "$#" -ge 2 ] || { printf 'ERROR: --udid requires a value\n' >&2; exit 2; }
            PHONE_UDID="$2"
            shift 2
            ;;
        --bundle-id)
            [ "$#" -ge 2 ] || { printf 'ERROR: --bundle-id requires a value\n' >&2; exit 2; }
            PHONE_BUNDLE_ID="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'ERROR: unknown argument: %s\n\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

valid_phone_udid() {
    case "$1" in
        ''|*[!0-9A-Fa-f-]*) return 1 ;;
        *) return 0 ;;
    esac
}

valid_bundle_id() {
    printf '%s\n' "$1" \
        | grep -Eq '^[A-Za-z0-9]+([A-Za-z0-9-]*[A-Za-z0-9])?(\.[A-Za-z0-9]+([A-Za-z0-9-]*[A-Za-z0-9])?)+$'
}

if [ "$REMOVE_PHONE_RUNNER" = "1" ]; then
    valid_phone_udid "$PHONE_UDID" || {
        printf 'ERROR: --remove-phone-runner requires an explicit hex/dash iPhone --udid.\n' >&2
        exit 2
    }
    valid_bundle_id "$PHONE_BUNDLE_ID" || {
        printf 'ERROR: --remove-phone-runner requires an explicit valid --bundle-id.\n' >&2
        exit 2
    }
elif [ -n "$PHONE_UDID" ] || [ -n "$PHONE_BUNDLE_ID" ]; then
    printf 'ERROR: --udid/--bundle-id are accepted only with --remove-phone-runner.\n' >&2
    exit 2
fi

info() { printf '== %s\n' "$*"; }
ok() { printf 'OK: %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
fail() {
    printf 'ERROR: %s\n' "$*" >&2
    FAILED=1
}
plan() { printf 'DRY-RUN: %s\n' "$*"; }

case "${HOME:-}" in
    "")
        printf 'ERROR: HOME is not set; run as the logged-in desktop user.\n' >&2
        exit 1
        ;;
    /*) ;;
    *)
        printf 'ERROR: HOME must be an absolute path (got %s).\n' "$HOME" >&2
        exit 1
        ;;
esac
case "$HOME" in
    /|*$'\n'*|*$'\r'*)
        printf 'ERROR: refusing unsafe HOME value.\n' >&2
        exit 1
        ;;
esac
[ -d "$HOME" ] || { printf 'ERROR: HOME does not exist: %s\n' "$HOME" >&2; exit 1; }
HOME_PHYSICAL="$(cd -P "$HOME" 2>/dev/null && pwd)"
[ -n "$HOME_PHYSICAL" ] \
    || { printf 'ERROR: could not resolve HOME: %s\n' "$HOME" >&2; exit 1; }
if [ "$HOME_PHYSICAL" != "$HOME" ]; then
    printf 'ERROR: HOME must be its canonical physical path.\n' >&2
    printf '       supplied: %s\n       physical: %s\n' "$HOME" "$HOME_PHYSICAL" >&2
    exit 1
fi

UID_NUM="$(id -u)"
[ "$UID_NUM" != "0" ] \
    || { printf 'ERROR: do not run this per-user uninstaller as root.\n' >&2; exit 1; }
GUI_DOMAIN="gui/$UID_NUM"

LAUNCHCTL_BIN="${IPHONE_USE_LAUNCHCTL:-/bin/launchctl}"
PLISTBUDDY_BIN="${IPHONE_USE_PLISTBUDDY:-/usr/libexec/PlistBuddy}"
PS_BIN="${IPHONE_USE_PS:-/bin/ps}"
STAT_BIN="${IPHONE_USE_STAT:-/usr/bin/stat}"
LSOF_BIN="${IPHONE_USE_LSOF:-$(command -v lsof 2>/dev/null || true)}"
SHASUM_BIN="${IPHONE_USE_SHASUM:-$(command -v shasum 2>/dev/null || true)}"

[ -x "$LAUNCHCTL_BIN" ] \
    || { printf 'ERROR: launchctl is unavailable: %s\n' "$LAUNCHCTL_BIN" >&2; exit 1; }
[ -x "$PLISTBUDDY_BIN" ] \
    || { printf 'ERROR: PlistBuddy is unavailable: %s\n' "$PLISTBUDDY_BIN" >&2; exit 1; }
[ -x "$PS_BIN" ] \
    || { printf 'ERROR: ps is unavailable: %s\n' "$PS_BIN" >&2; exit 1; }
[ -x "$STAT_BIN" ] \
    || { printf 'ERROR: stat is unavailable: %s\n' "$STAT_BIN" >&2; exit 1; }

DAEMON_LABEL="com.leeguoo.iphone-use"
WDA_LABEL="com.leeguoo.iphone-use.wda"
LEGACY_LABEL="work.pwtk.iphone-remote"

LAUNCH_AGENTS_DIR="$HOME/Library/LaunchAgents"
DAEMON_PLIST="$LAUNCH_AGENTS_DIR/$DAEMON_LABEL.plist"
WDA_PLIST="$LAUNCH_AGENTS_DIR/$WDA_LABEL.plist"
LEGACY_PLIST="$LAUNCH_AGENTS_DIR/$LEGACY_LABEL.plist"
LEGACY_DISABLED_PLIST="$LEGACY_PLIST.disabled"

APP_PATH="$HOME/Applications/iPhoneUse.app"
APP_BINARY="$APP_PATH/Contents/MacOS/iphone-use"
LEGACY_APP_PATH="$HOME/Applications/iPhoneRemote.app"
LEGACY_APP_BINARY="$LEGACY_APP_PATH/Contents/MacOS/iphone-remote"

LOG_DIR="$HOME/Library/Logs/iPhoneUse"
LEGACY_LOG_DIR="$HOME/Library/Logs/iPhoneRemote"
STATE_DIR="$HOME/.iphone-use"
FIXED_SETUP="$STATE_DIR/setup-wda.sh"
WDA_CHECKOUT="$STATE_DIR/WebDriverAgent"
WDA_CHECKOUT_MARKER="$STATE_DIR/wda-checkout-owner.v1"
LAUNCH_AGENTS_SAFE=1
STATE_SAFE=1

path_exists() {
    [ -e "$1" ] || [ -L "$1" ]
}

read_uid_mode() {
    local path="$1"
    local metadata
    PATH_UID=""
    PATH_MODE=""
    metadata="$("$STAT_BIN" -f '%u|%Lp' "$path" 2>/dev/null)" || return 1
    case "$metadata" in
        *'|'*) ;;
        *) return 1 ;;
    esac
    PATH_UID="${metadata%%|*}"
    PATH_MODE="${metadata#*|}"
    case "$PATH_UID:$PATH_MODE" in
        *[!0-9:]*|:*) return 1 ;;
    esac
    return 0
}

namespace_dir_safe() {
    local path="$1"
    local policy="${2:-owned}"
    local physical
    path_exists "$path" || return 0
    if [ -L "$path" ] || [ ! -d "$path" ]; then
        fail "refusing unsafe product namespace (not a real directory): $path"
        return 1
    fi
    physical="$(cd -P "$path" 2>/dev/null && pwd)" || {
        fail "could not resolve product namespace: $path"
        return 1
    }
    if [ "$physical" != "$path" ]; then
        fail "refusing symlinked/unexpected product namespace: $path"
        return 1
    fi
    if ! read_uid_mode "$path"; then
        fail "could not verify namespace owner/mode: $path"
        return 1
    fi
    if [ "$PATH_UID" != "$UID_NUM" ]; then
        fail "refusing product namespace not owned by current uid $UID_NUM: $path"
        return 1
    fi
    case "$policy" in
        private)
            if [ "$PATH_MODE" != "700" ]; then
                fail "refusing private product namespace with mode $PATH_MODE (expected 700): $path"
                return 1
            fi
            ;;
        owned)
            case "$PATH_MODE" in
                [0-7][0-7][0-7]) ;;
                *)
                    fail "refusing namespace with an unreadable permission mode: $path"
                    return 1
                    ;;
            esac
            local group_mode="${PATH_MODE#?}"
            local other_mode="${group_mode#?}"
            group_mode="${group_mode%?}"
            case "$group_mode$other_mode" in
                *[2367]*)
                    fail "refusing namespace writable by group/others (mode $PATH_MODE): $path"
                    return 1
                    ;;
            esac
            ;;
        *)
            fail "internal namespace policy is invalid: $policy"
            return 1
            ;;
    esac
    return 0
}

namespace_dir_safe "$LAUNCH_AGENTS_DIR" owned || LAUNCH_AGENTS_SAFE=0
namespace_dir_safe "$STATE_DIR" private || STATE_SAFE=0

assert_exact_path() {
    local actual="$1"
    local expected="$2"
    [ "$actual" = "$expected" ] || {
        fail "internal path guard rejected '$actual' (expected '$expected')"
        return 1
    }
    case "$actual" in
        "$HOME"/*) return 0 ;;
        *)
            fail "path is outside the validated HOME: $actual"
            return 1
            ;;
    esac
}

assert_real_parent() {
    local path="$1"
    local expected_parent="$2"
    local parent
    parent="$(dirname "$path")"
    [ -d "$parent" ] || return 0
    parent="$(cd -P "$parent" 2>/dev/null && pwd)" || {
        fail "could not resolve parent directory for $path"
        return 1
    }
    if [ "$parent" != "$expected_parent" ]; then
        fail "refusing path through a symlinked/unexpected parent: $path"
        return 1
    fi
    return 0
}

plist_get() {
    local plist="$1"
    local key="$2"
    "$PLISTBUDDY_BIN" -c "Print :$key" "$plist" 2>/dev/null || printf ''
}

plist_owned() {
    local kind="$1"
    local plist="$2"
    local label arg0 arg1
    [ "$LAUNCH_AGENTS_SAFE" = "1" ] || return 1
    path_exists "$plist" || return 1
    [ ! -L "$plist" ] && [ -f "$plist" ] || return 1
    label="$(plist_get "$plist" Label)"
    arg0="$(plist_get "$plist" ProgramArguments:0)"
    arg1="$(plist_get "$plist" ProgramArguments:1)"
    case "$kind" in
        daemon)
            [ "$label" = "$DAEMON_LABEL" ] \
                && [ "$arg0" = "$APP_BINARY" ] \
                && [ "$arg1" = "serve" ]
            ;;
        wda)
            [ "$STATE_SAFE" = "1" ] \
                && [ "$label" = "$WDA_LABEL" ] \
                && [ "$arg0" = "/bin/bash" ] \
                && [ "$arg1" = "$FIXED_SETUP" ]
            ;;
        legacy)
            [ "$label" = "$LEGACY_LABEL" ] \
                && [ "$arg0" = "$LEGACY_APP_BINARY" ] \
                && [ "$arg1" = "serve" ]
            ;;
        *) return 1 ;;
    esac
}

launchctl_snapshot() {
    "$LAUNCHCTL_BIN" print "$GUI_DOMAIN/$1" 2>/dev/null
}

snapshot_has_exact_value() {
    local snapshot="$1"
    local expected="$2"
    printf '%s\n' "$snapshot" | awk -v expected="$expected" '
        {
            line = $0
            sub(/^[[:space:]]*/, "", line)
            sub(/[[:space:]]*$/, "", line)
            if (line == expected || line == "program = " expected) found = 1
        }
        END { exit(found ? 0 : 1) }
    '
}

loaded_job_owned() {
    local kind="$1"
    local snapshot="$2"
    case "$kind" in
        daemon)
            snapshot_has_exact_value "$snapshot" "$APP_BINARY" \
                && snapshot_has_exact_value "$snapshot" "serve"
            ;;
        wda)
            snapshot_has_exact_value "$snapshot" "/bin/bash" \
                && snapshot_has_exact_value "$snapshot" "$FIXED_SETUP"
            ;;
        legacy)
            snapshot_has_exact_value "$snapshot" "$LEGACY_APP_BINARY" \
                && snapshot_has_exact_value "$snapshot" "serve"
            ;;
        *) return 1 ;;
    esac
}

wait_job_gone() {
    local label="$1"
    local tries=0
    while [ "$tries" -lt 20 ]; do
        launchctl_snapshot "$label" >/dev/null 2>&1 || return 0
        tries=$((tries + 1))
        sleep 0.25
    done
    ! launchctl_snapshot "$label" >/dev/null 2>&1
}

stop_owned_job() {
    local kind="$1"
    local label="$2"
    local plist="$3"
    local snapshot=""
    local loaded=0
    local owned_definition=0
    local owned_loaded=0
    local safe=1

    if path_exists "$plist"; then
        if plist_owned "$kind" "$plist"; then
            owned_definition=1
        else
            fail "refusing to claim LaunchAgent plist with unexpected Label/ProgramArguments: $plist"
            safe=0
        fi
    fi

    if snapshot="$(launchctl_snapshot "$label")"; then
        loaded=1
        if loaded_job_owned "$kind" "$snapshot"; then
            owned_loaded=1
        else
            fail "loaded job $GUI_DOMAIN/$label does not expose the product's exact ProgramArguments"
            safe=0
        fi
    fi

    if [ "$loaded" = "1" ] && [ "$owned_loaded" = "1" ]; then
        if [ "$DRY_RUN" = "1" ]; then
            plan "launchctl bootout $GUI_DOMAIN/$label"
        else
            "$LAUNCHCTL_BIN" bootout "$GUI_DOMAIN/$label" 2>/dev/null || true
            if ! wait_job_gone "$label"; then
                fail "LaunchAgent did not stop: $GUI_DOMAIN/$label"
                return 1
            fi
            ok "stopped LaunchAgent $label"
        fi
    fi

    if [ "$owned_definition" = "1" ] || [ "$owned_loaded" = "1" ]; then
        if [ "$DRY_RUN" = "1" ]; then
            plan "launchctl disable $GUI_DOMAIN/$label"
        elif ! "$LAUNCHCTL_BIN" disable "$GUI_DOMAIN/$label" 2>/dev/null; then
            warn "could not persistently disable $GUI_DOMAIN/$label (the owned plist will still be removed)"
        fi
    fi
    [ "$safe" = "1" ]
}

pid_exists() {
    case "$1" in
        ''|0|1|0*|*[!0-9]*) return 1 ;;
    esac
    "$PS_BIN" -p "$1" -o pid= >/dev/null 2>&1
}

valid_port() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$1" -ge 1 ] 2>/dev/null && [ "$1" -le 65535 ] 2>/dev/null
}

valid_team_id() {
    [ "${#1}" -eq 10 ] || return 1
    case "$1" in
        *[!ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789]*) return 1 ;;
        *) return 0 ;;
    esac
}

expected_role_valid() {
    local expected="$1"
    local role="$2"
    case "$role:$expected" in
        runner:runner:?*|runner:legacy-runner:?*) return 0 ;;
        relay:relay:?*|relay:legacy-relay:?*) return 0 ;;
        mjpeg:mjpeg:?*|mjpeg:legacy-mjpeg:?*) return 0 ;;
        *) return 1 ;;
    esac
}

# Keep the uninstaller's process identity contract in lockstep with
# scripts/setup-wda.sh. A PID record is evidence, not authority: the command it
# contains must still have one of the exact WDA/relay shapes that setup writes.
command_matches_expected() {
    local command="$1"
    local expected="$2"
    local signature rest target_udid team_id bundle_id
    local local_port device_port legacy_argv
    case "$expected" in
        runner:*)
            signature="${expected#*:}"
            printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^(/[^ ]*/)?xcodebuild -project WebDriverAgent\.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=[0-9A-Fa-f-]+ -allowProvisioningUpdates DEVELOPMENT_TEAM=[A-Z0-9]{10} PRODUCT_BUNDLE_IDENTIFIER=[A-Za-z0-9.-]+ test$' \
                || return 1
            [ "$command" = "$signature" ]
            ;;
        relay:*|mjpeg:*)
            signature="${expected#*:}"
            if printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^(/[^ ]*/)?iproxy -s 127\.0\.0\.1 [0-9]+:[0-9]+ -u [0-9A-Fa-f-]+$'; then
                :
            elif printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^(/[^ ]*/)?socat TCP-LISTEN:[0-9]+,fork,reuseaddr,bind=127\.0\.0\.1 TCP:[A-Za-z0-9.:%_-]+:[0-9]+$'; then
                :
            else
                return 1
            fi
            [ "$command" = "$signature" ]
            ;;
        legacy-runner:*)
            rest="${expected#legacy-runner:}"
            target_udid="${rest%%:*}"
            rest="${rest#*:}"
            team_id="${rest%%:*}"
            bundle_id="${rest#*:}"
            case "$target_udid" in
                ''|__missing_udid__|*[!0-9A-Fa-f-]*) return 1 ;;
            esac
            valid_team_id "$team_id" || return 1
            valid_bundle_id "$bundle_id" || return 1
            legacy_argv="xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=$target_udid -allowProvisioningUpdates DEVELOPMENT_TEAM=$team_id PRODUCT_BUNDLE_IDENTIFIER=$bundle_id test"
            case "$command" in
                "$legacy_argv"|*/"$legacy_argv") return 0 ;;
                *) return 1 ;;
            esac
            ;;
        legacy-relay:*|legacy-mjpeg:*)
            rest="${expected#*:}"
            local_port="${rest%%:*}"
            rest="${rest#*:}"
            device_port="${rest%%:*}"
            target_udid="${rest#*:}"
            valid_port "$local_port" || return 1
            valid_port "$device_port" || return 1
            case "$target_udid" in
                ''|__missing_udid__|*[!0-9A-Fa-f-]*) return 1 ;;
            esac
            case "$command" in
                "iproxy $local_port $device_port -u $target_udid"|\
                */"iproxy $local_port $device_port -u $target_udid"|\
                "iproxy -s 127.0.0.1 $local_port:$device_port -u $target_udid"|\
                */"iproxy -s 127.0.0.1 $local_port:$device_port -u $target_udid")
                    return 0
                    ;;
                *) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

expected_listener_port() {
    local expected="$1"
    local signature rest pair port
    case "$expected" in
        relay:*|mjpeg:*)
            signature="${expected#*:}"
            case "$signature" in
                *"iproxy -s 127.0.0.1 "*)
                    rest="${signature#*iproxy -s 127.0.0.1 }"
                    pair="${rest%% *}"
                    port="${pair%%:*}"
                    ;;
                *"socat TCP-LISTEN:"*)
                    rest="${signature#*socat TCP-LISTEN:}"
                    port="${rest%%,*}"
                    ;;
                *) return 1 ;;
            esac
            ;;
        legacy-relay:*|legacy-mjpeg:*)
            rest="${expected#*:}"
            port="${rest%%:*}"
            ;;
        *) return 1 ;;
    esac
    valid_port "$port" || return 1
    printf '%s\n' "$port"
}

verify_loopback_listener() {
    local pid="$1"
    local port="$2"
    local listener_data listener_pids listener_name_data listener_names unexpected
    [ -n "$LSOF_BIN" ] && [ -x "$LSOF_BIN" ] || return 1
    listener_data="$("$LSOF_BIN" -nP -a -iTCP:"$port" -sTCP:LISTEN -Fp 2>/dev/null)" \
        || return 1
    listener_pids="$(printf '%s\n' "$listener_data" | sed -n 's/^p//p' | sort -u)"
    [ -n "$listener_pids" ] || return 1
    unexpected="$(printf '%s\n' "$listener_pids" \
        | awk -v p="$pid" '$0 != p { print; exit }')"
    [ -z "$unexpected" ] || return 1
    listener_name_data="$("$LSOF_BIN" -nP -a -p "$pid" \
        -iTCP:"$port" -sTCP:LISTEN -Fn 2>/dev/null)" || return 1
    listener_names="$(printf '%s\n' "$listener_name_data" | sed -n 's/^n//p')"
    [ -n "$listener_names" ] || return 1
    unexpected="$(printf '%s\n' "$listener_names" \
        | awk -v n="127.0.0.1:$port" '$0 != n { print; exit }')"
    [ -z "$unexpected" ]
}

verify_runner_cwd() {
    local pid="$1"
    local configured="${CONFIGURED_WDA_DIR:-}"
    local expected_dir cwd_data process_cwd
    [ -n "$configured" ] || configured="$WDA_CHECKOUT"
    [ -n "$LSOF_BIN" ] && [ -x "$LSOF_BIN" ] || return 1
    [ ! -L "$configured" ] && [ -d "$configured" ] || return 1
    expected_dir="$(cd -P "$configured" 2>/dev/null && pwd)" || return 1
    cwd_data="$("$LSOF_BIN" -nP -a -p "$pid" -d cwd -Fn 2>/dev/null)" || return 1
    process_cwd="$(printf '%s\n' "$cwd_data" | sed -n 's/^n//p' | head -1)"
    [ -n "$process_cwd" ] && [ "$process_cwd" = "$expected_dir" ]
}

pid_record_secure() {
    local file="$1"
    if ! read_uid_mode "$file"; then
        fail "could not verify PID record owner/mode: $file"
        return 1
    fi
    if [ "$PATH_UID" != "$UID_NUM" ] || [ "$PATH_MODE" != "600" ]; then
        fail "refusing PID record not owned by uid $UID_NUM with mode 600: $file (uid=$PATH_UID mode=$PATH_MODE)"
        return 1
    fi
    return 0
}

stop_pid_record() {
    local file="$1"
    local role="$2"
    local record pid rest recorded_lstart expected command persisted_command
    local current_lstart process_uid listener_port
    local tries

    path_exists "$file" || return 0
    if [ -L "$file" ] || [ ! -f "$file" ]; then
        fail "refusing non-regular PID record: $file"
        return 1
    fi
    pid_record_secure "$file" || return 1
    record="$(sed -n '1p' "$file" 2>/dev/null || true)"
    [ "$(awk 'END { print NR }' "$file" 2>/dev/null)" = "1" ] || {
        fail "invalid multi-line PID record: $file"
        return 1
    }
    pid="${record%%|*}"
    case "$pid" in
        ''|0|1|0*|*[!0-9]*)
            fail "invalid PID in $file"
            return 1
            ;;
    esac

    if ! pid_exists "$pid"; then
        if [ "$DRY_RUN" = "1" ]; then
            plan "remove stale PID record $file"
        elif rm -f "$file"; then
            ok "removed stale PID record $(basename "$file")"
        else
            fail "could not remove stale PID record: $file"
            return 1
        fi
        return 0
    fi

    case "$record" in
        *'|'*)
            rest="${record#*|}"
            case "$rest" in
                *'|'*) ;;
                *)
                    fail "invalid PID metadata in $file; refusing to signal PID $pid"
                    return 1
                    ;;
            esac
            recorded_lstart="${rest%%|*}"
            expected="${rest#*|}"
            ;;
        *)
            fail "legacy PID-only record cannot prove process ownership; refusing to signal PID $pid"
            return 1
            ;;
    esac
    expected_role_valid "$expected" "$role" || {
        fail "PID record role mismatch in $file; refusing to signal PID $pid"
        return 1
    }
    case "$expected" in
        *'|'*|*$'\n'*|*$'\r'*)
            fail "unsafe command identity in $file"
            return 1
            ;;
    esac

    process_uid="$("$PS_BIN" -p "$pid" -o uid= 2>/dev/null | tr -d '[:space:]')"
    current_lstart="$(LC_ALL=C "$PS_BIN" -p "$pid" -o lstart= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ "$process_uid" = "$UID_NUM" ] \
        && [ -n "$recorded_lstart" ] \
        && [ "$current_lstart" = "$recorded_lstart" ] \
        || {
            fail "PID metadata no longer matches PID $pid; refusing to signal it"
            return 1
        }
    command="$(LC_ALL=C "$PS_BIN" -ww -p "$pid" -o command= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    case "$expected" in
        runner:*|relay:*|mjpeg:*)
            persisted_command="${expected#*:}"
            [ "$command" = "$persisted_command" ] || {
                fail "PID $pid command does not match the persisted $role identity; refusing to signal it"
                return 1
            }
            ;;
    esac
    command_matches_expected "$command" "$expected" || {
        fail "PID record contains a non-product $role command contract; refusing to signal PID $pid"
        return 1
    }
    case "$role" in
        runner)
            verify_runner_cwd "$pid" || {
                fail "PID $pid is not a WDA runner rooted in the configured checkout; refusing to signal it"
                return 1
            }
            ;;
        relay|mjpeg)
            listener_port="$(expected_listener_port "$expected")" || {
                fail "PID $pid has no valid loopback listener contract; refusing to signal it"
                return 1
            }
            verify_loopback_listener "$pid" "$listener_port" || {
                fail "PID $pid does not exclusively own 127.0.0.1:$listener_port; refusing to signal it"
                return 1
            }
            ;;
    esac

    if [ "$DRY_RUN" = "1" ]; then
        plan "send SIGTERM to PID-verified $role process $pid"
        plan "remove PID record $file after verified exit"
        return 0
    fi

    # Close the validation-to-signal window as far as portable macOS process
    # APIs allow: re-read all three identity fields immediately before kill.
    process_uid="$("$PS_BIN" -p "$pid" -o uid= 2>/dev/null | tr -d '[:space:]')"
    current_lstart="$(LC_ALL=C "$PS_BIN" -p "$pid" -o lstart= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ "$process_uid" = "$UID_NUM" ] \
        && [ "$current_lstart" = "$recorded_lstart" ] \
        && [ "$(LC_ALL=C "$PS_BIN" -ww -p "$pid" -o command= 2>/dev/null \
            | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')" = "$command" ] || {
            fail "PID $pid identity changed before signalling; refusing to kill it"
            return 1
        }
    command_matches_expected "$command" "$expected" || {
        fail "PID $pid command contract changed before signalling; refusing to kill it"
        return 1
    }
    case "$role" in
        runner)
            verify_runner_cwd "$pid" || {
                fail "PID $pid runner cwd changed before signalling; refusing to kill it"
                return 1
            }
            ;;
        relay|mjpeg)
            listener_port="$(expected_listener_port "$expected")" || return 1
            verify_loopback_listener "$pid" "$listener_port" || {
                fail "PID $pid listener ownership changed before signalling; refusing to kill it"
                return 1
            }
            ;;
    esac
    if ! kill -TERM "$pid" 2>/dev/null; then
        fail "could not signal PID-verified $role process $pid"
        return 1
    fi
    tries=0
    while pid_exists "$pid" && [ "$tries" -lt 20 ]; do
        tries=$((tries + 1))
        sleep 0.25
    done
    if pid_exists "$pid"; then
        fail "PID-verified $role process $pid did not stop after SIGTERM; it was not force-killed"
        return 1
    fi
    if rm -f "$file"; then
        ok "stopped PID-verified $role process $pid"
    else
        fail "stopped $role process but could not remove $file"
        return 1
    fi
    return 0
}

remove_file() {
    local path="$1"
    local expected="$2"
    assert_exact_path "$path" "$expected" || return 1
    path_exists "$path" || return 0
    [ ! -L "$path" ] || {
        fail "refusing to remove symlink at owned file path: $path"
        return 1
    }
    if [ "$DRY_RUN" = "1" ]; then
        plan "remove file $path"
    elif rm -f "$path"; then
        ok "removed $path"
    else
        fail "could not remove $path"
        return 1
    fi
    return 0
}

remove_tree() {
    local path="$1"
    local expected="$2"
    local expected_parent="$3"
    assert_exact_path "$path" "$expected" || return 1
    path_exists "$path" || return 0
    [ ! -L "$path" ] || {
        fail "refusing to recursively remove symlink: $path"
        return 1
    }
    assert_real_parent "$path" "$expected_parent" || return 1
    if [ "$DRY_RUN" = "1" ]; then
        plan "remove directory tree $path"
    elif rm -rf "$path" && ! path_exists "$path"; then
        ok "removed $path"
    else
        fail "could not completely remove $path"
        return 1
    fi
    return 0
}

remove_owned_plist() {
    local kind="$1"
    local path="$2"
    path_exists "$path" || return 0
    if ! plist_owned "$kind" "$path"; then
        fail "preserving unowned or malformed plist: $path"
        return 1
    fi
    remove_file "$path" "$path"
}

remove_owned_app() {
    local path="$1"
    local expected_path="$2"
    local binary="$3"
    local bundle_id="$4"
    local actual_bundle info_plist
    path_exists "$path" || return 0
    if [ -L "$path" ] || [ ! -d "$path" ]; then
        fail "preserving non-directory/symlink at app path: $path"
        return 1
    fi
    info_plist="$path/Contents/Info.plist"
    if [ -L "$info_plist" ] || [ ! -f "$info_plist" ]; then
        fail "preserving app with unsafe/missing Info.plist: $path"
        return 1
    fi
    actual_bundle="$(plist_get "$info_plist" CFBundleIdentifier)"
    if [ "$actual_bundle" != "$bundle_id" ] || [ ! -f "$binary" ]; then
        fail "preserving app whose bundle/executable does not match iphone-use ownership: $path"
        return 1
    fi
    remove_tree "$path" "$expected_path" "$HOME/Applications"
}

sha256_text() {
    local value="$1"
    local output
    [ -n "$SHASUM_BIN" ] && [ -x "$SHASUM_BIN" ] || return 1
    output="$(printf '%s' "$value" | "$SHASUM_BIN" -a 256 2>/dev/null)" || return 1
    output="${output%% *}"
    [ "${#output}" -eq 64 ] || return 1
    case "$output" in
        *[!0-9a-f]*) return 1 ;;
    esac
    printf '%s\n' "$output"
}

git_metadata_digests() {
    local checkout="$1"
    local refs reflog worktrees objects
    refs="$(GIT_OPTIONAL_LOCKS=0 git -C "$checkout" \
        for-each-ref --format='%(refname) %(objectname)' 2>/dev/null)" || return 1
    reflog="$(GIT_OPTIONAL_LOCKS=0 git -C "$checkout" \
        reflog show --all --format='%H %gD %gs' 2>/dev/null)" || return 1
    worktrees="$(GIT_OPTIONAL_LOCKS=0 git -C "$checkout" \
        worktree list --porcelain 2>/dev/null)" || return 1
    objects="$(GIT_OPTIONAL_LOCKS=0 git -C "$checkout" \
        cat-file --batch-all-objects --batch-check='%(objectname)' 2>/dev/null)" \
        || return 1
    objects="$(printf '%s\n' "$objects" | LC_ALL=C sort)" || return 1
    GIT_REFS_DIGEST="$(sha256_text "$refs")" || return 1
    GIT_REFLOG_DIGEST="$(sha256_text "$reflog")" || return 1
    GIT_WORKTREES_DIGEST="$(sha256_text "$worktrees")" || return 1
    GIT_OBJECTS_DIGEST="$(sha256_text "$objects")" || return 1
    GIT_WORKTREES_SNAPSHOT="$worktrees"
    return 0
}

read_wda_checkout_marker() {
    local marker="$WDA_CHECKOUT_MARKER"
    local count
    if [ -L "$marker" ] || [ ! -f "$marker" ]; then
        WDA_CHECKOUT_REASON="dedicated setup ownership marker is missing or unsafe"
        return 1
    fi
    if ! read_uid_mode "$marker" \
        || [ "$PATH_UID" != "$UID_NUM" ] \
        || [ "$PATH_MODE" != "600" ]; then
        WDA_CHECKOUT_REASON="ownership marker must be a current-uid regular file with mode 600"
        return 1
    fi
    count="$(awk 'END { print NR }' "$marker" 2>/dev/null)" || return 1
    if [ "$count" != "9" ]; then
        WDA_CHECKOUT_REASON="ownership marker has an unexpected schema"
        return 1
    fi
    MARKER_VERSION="$(sed -n '1s/^version=//p' "$marker")"
    MARKER_PATH="$(sed -n '2s/^path=//p' "$marker")"
    MARKER_ORIGIN="$(sed -n '3s/^origin=//p' "$marker")"
    MARKER_HEAD="$(sed -n '4s/^head=//p' "$marker")"
    MARKER_UDID="$(sed -n '5s/^udid=//p' "$marker")"
    MARKER_REFS_DIGEST="$(sed -n '6s/^refs_sha256=//p' "$marker")"
    MARKER_REFLOG_DIGEST="$(sed -n '7s/^reflog_sha256=//p' "$marker")"
    MARKER_WORKTREES_DIGEST="$(sed -n '8s/^worktrees_sha256=//p' "$marker")"
    MARKER_OBJECTS_DIGEST="$(sed -n '9s/^objects_sha256=//p' "$marker")"
    [ "$MARKER_VERSION" = "1" ] \
        && [ -n "$MARKER_PATH" ] \
        && [ -n "$MARKER_ORIGIN" ] \
        && [ "${#MARKER_HEAD}" -eq 40 ] \
        && [ "${#MARKER_REFS_DIGEST}" -eq 64 ] \
        && [ "${#MARKER_REFLOG_DIGEST}" -eq 64 ] \
        && [ "${#MARKER_WORKTREES_DIGEST}" -eq 64 ] \
        && [ "${#MARKER_OBJECTS_DIGEST}" -eq 64 ] \
        || {
            WDA_CHECKOUT_REASON="ownership marker contains incomplete identity fields"
            return 1
        }
    case "$MARKER_HEAD$MARKER_REFS_DIGEST$MARKER_REFLOG_DIGEST$MARKER_WORKTREES_DIGEST$MARKER_OBJECTS_DIGEST" in
        *[!0-9A-Fa-f]*)
            WDA_CHECKOUT_REASON="ownership marker contains malformed Git identity digests"
            return 1
            ;;
    esac
    case "$MARKER_UDID" in
        ''|*[!0-9A-Fa-f-]*)
            WDA_CHECKOUT_REASON="ownership marker contains a malformed device UDID"
            return 1
            ;;
    esac
    return 0
}

wda_checkout_owned() {
    local origin status head top git_dir configured_dir worktree_count worktree_path
    WDA_CHECKOUT_REASON=""
    [ "$STATE_SAFE" = "1" ] || {
        WDA_CHECKOUT_REASON="state namespace ownership/mode was not verified"
        return 1
    }
    read_wda_checkout_marker || return 1
    if [ -L "$WDA_CHECKOUT/.git" ] || [ ! -d "$WDA_CHECKOUT/.git" ]; then
        WDA_CHECKOUT_REASON="missing or symlinked .git ownership metadata"
        return 1
    fi
    if ! command -v git >/dev/null 2>&1; then
        WDA_CHECKOUT_REASON="git is unavailable for ownership/status verification"
        return 1
    fi
    configured_dir="${CONFIGURED_WDA_DIR:-}"
    if [ -z "$configured_dir" ] || [ -L "$configured_dir" ] || [ ! -d "$configured_dir" ]; then
        WDA_CHECKOUT_REASON="owned supervisor does not configure a real WDA_DIR"
        return 1
    fi
    configured_dir="$(cd -P "$configured_dir" 2>/dev/null && pwd)" || {
        WDA_CHECKOUT_REASON="configured WDA_DIR cannot be resolved"
        return 1
    }
    if [ "$configured_dir" != "$WDA_CHECKOUT" ] || [ "$MARKER_PATH" != "$WDA_CHECKOUT" ]; then
        WDA_CHECKOUT_REASON="marker/supervisor path does not bind the fixed managed checkout"
        return 1
    fi
    top="$(GIT_OPTIONAL_LOCKS=0 git -C "$WDA_CHECKOUT" \
        rev-parse --show-toplevel 2>/dev/null)" || {
        WDA_CHECKOUT_REASON="Git top-level cannot be resolved"
        return 1
    }
    git_dir="$(GIT_OPTIONAL_LOCKS=0 git -C "$WDA_CHECKOUT" \
        rev-parse --absolute-git-dir 2>/dev/null)" || {
        WDA_CHECKOUT_REASON="Git common directory cannot be resolved"
        return 1
    }
    if [ "$top" != "$WDA_CHECKOUT" ] || [ "$git_dir" != "$WDA_CHECKOUT/.git" ]; then
        WDA_CHECKOUT_REASON="Git top-level/common-dir escapes the managed checkout"
        return 1
    fi
    origin="$(git -C "$WDA_CHECKOUT" config --get remote.origin.url 2>/dev/null || true)"
    case "$origin" in
        https://github.com/appium/WebDriverAgent|\
        https://github.com/appium/WebDriverAgent.git|\
        git@github.com:appium/WebDriverAgent.git|\
        ssh://git@github.com/appium/WebDriverAgent.git)
            ;;
        *)
            WDA_CHECKOUT_REASON="remote.origin is not the official appium/WebDriverAgent repository"
            return 1
            ;;
    esac
    if [ "$origin" != "$MARKER_ORIGIN" ]; then
        WDA_CHECKOUT_REASON="remote.origin differs from the setup ownership marker"
        return 1
    fi
    if ! status="$(GIT_OPTIONAL_LOCKS=0 git -c core.fsmonitor=false -C "$WDA_CHECKOUT" \
        status --porcelain=v1 --untracked-files=all --ignored=matching \
        --ignore-submodules=none 2>/dev/null)"; then
        WDA_CHECKOUT_REASON="Git status (including ignored files/submodules) failed"
        return 1
    fi
    if [ -n "$status" ]; then
        WDA_CHECKOUT_REASON="checkout has tracked, untracked, ignored, or submodule data"
        return 1
    fi
    head="$(git -C "$WDA_CHECKOUT" rev-parse HEAD 2>/dev/null || true)"
    if [ -z "${CONFIGURED_WDA_REF:-}" ] \
        || [ "$(printf '%s' "$head" | tr 'A-F' 'a-f')" \
            != "$(printf '%s' "$CONFIGURED_WDA_REF" | tr 'A-F' 'a-f')" ] \
        || [ "$(printf '%s' "$head" | tr 'A-F' 'a-f')" \
            != "$(printf '%s' "$MARKER_HEAD" | tr 'A-F' 'a-f')" ]; then
        WDA_CHECKOUT_REASON="HEAD, supervisor WDA_REF, and ownership marker do not match"
        return 1
    fi
    if [ -z "${CONFIGURED_WDA_UDID:-}" ] \
        || [ "$CONFIGURED_WDA_UDID" != "$MARKER_UDID" ]; then
        WDA_CHECKOUT_REASON="supervisor device and ownership-marker device do not match"
        return 1
    fi
    if ! git_metadata_digests "$WDA_CHECKOUT"; then
        WDA_CHECKOUT_REASON="could not fingerprint Git refs/reflog/worktrees"
        return 1
    fi
    if [ "$GIT_REFS_DIGEST" != "$MARKER_REFS_DIGEST" ]; then
        WDA_CHECKOUT_REASON="Git refs changed after setup (local commits/branches/stashes are preserved)"
        return 1
    fi
    if [ "$GIT_REFLOG_DIGEST" != "$MARKER_REFLOG_DIGEST" ]; then
        WDA_CHECKOUT_REASON="Git reflog changed after setup (local history is preserved)"
        return 1
    fi
    if [ "$GIT_WORKTREES_DIGEST" != "$MARKER_WORKTREES_DIGEST" ]; then
        WDA_CHECKOUT_REASON="Git worktree topology changed after setup"
        return 1
    fi
    if [ "$GIT_OBJECTS_DIGEST" != "$MARKER_OBJECTS_DIGEST" ]; then
        WDA_CHECKOUT_REASON="Git object database changed after setup (unpublished objects are preserved)"
        return 1
    fi
    worktree_count="$(printf '%s\n' "$GIT_WORKTREES_SNAPSHOT" \
        | awk '$1 == "worktree" { count++ } END { print count + 0 }')"
    worktree_path="$(printf '%s\n' "$GIT_WORKTREES_SNAPSHOT" \
        | awk '$1 == "worktree" { sub(/^worktree /, ""); print; exit }')"
    if [ "$worktree_count" != "1" ] || [ "$worktree_path" != "$WDA_CHECKOUT" ]; then
        WDA_CHECKOUT_REASON="linked or non-canonical Git worktrees exist"
        return 1
    fi
    return 0
}

remove_state_transients() {
    local transient
    [ "$STATE_SAFE" = "1" ] && [ -d "$STATE_DIR" ] || return 0
    while IFS= read -r -d '' transient; do
        remove_file "$transient" "$transient"
        [ "$FAILED" = "0" ] || break
    done < <(
        find "$STATE_DIR" -mindepth 1 -maxdepth 1 -type f \
            \( -name 'setup-wda.sh.new.*' \
            -o -name 'setup-wda.sh.backup.*' \
            -o -name 'setup-wda.rollback.*.sh' \
            -o -name 'setup-wda.install.*' \
            -o -name 'setup-wda.restore.*' \
            -o -name 'setup-wda.sh.restore.*' \
            -o -name 'wda-supervisor.rollback.*.plist' \
            -o -name 'wda-supervisor.restore.*' \
            -o -name 'daemon.rollback.*.plist' \
            -o -name 'daemon.restore.*' \
            -o -name 'uninstall.sh.new.*' \
            -o -name 'uninstall.sh.backup.*' \
            -o -name 'uninstall.sh.restore.*' \
            -o -name 'wda-checkout-owner.v1.new.*' \
            -o -name 'wda-checkout-owner.v1.restore.*' \
            -o -name '.lsof-error.*' \
            -o -name '.wda-mjpeg-probe.*' \) \
            -print0 2>/dev/null
    )
}

state_unknown_entries() {
    local entry name
    [ "$STATE_SAFE" = "1" ] && [ -d "$STATE_DIR" ] || return 1
    while IFS= read -r -d '' entry; do
        name="${entry##*/}"
        case "$name" in
            WebDriverAgent)
                if [ -L "$entry" ] || [ ! -d "$entry" ]; then
                    printf '%s\n' "$entry"
                fi
                ;;
            uninstall.sh|\
            setup-wda.sh|\
            setup-wda.rollback.sh|\
            setup-wda.sh.new.*|\
            setup-wda.sh.backup.*|\
            setup-wda.rollback.*.sh|\
            setup-wda.install.*|\
            setup-wda.restore.*|\
            setup-wda.sh.restore.*|\
            wda-supervisor.rollback.plist|\
            wda-supervisor.rollback.*.plist|\
            wda-supervisor.restore.*|\
            daemon.rollback.*.plist|\
            daemon.restore.*|\
            uninstall.sh.new.*|\
            uninstall.sh.backup.*|\
            uninstall.sh.restore.*|\
            wda-checkout-owner.v1|\
            wda-checkout-owner.v1.new.*|\
            wda-checkout-owner.v1.restore.*|\
            wda-runner.log|wda-runner.pid|\
            wda-relay.log|wda-relay.pid|\
            wda-mjpeg-relay.log|wda-mjpeg-relay.pid|\
            wda-agent.log|wda-setup-status.json|\
            .lsof-error.*|.wda-mjpeg-probe.*)
                if [ -L "$entry" ] || [ ! -f "$entry" ]; then
                    printf '%s\n' "$entry"
                fi
                ;;
            *)
                printf '%s\n' "$entry"
                ;;
        esac
    done < <(find "$STATE_DIR" -mindepth 1 -maxdepth 1 -print0 2>/dev/null)
}

run_with_deadline() {
    local seconds="$1"
    shift
    command -v python3 >/dev/null 2>&1 || return 127
    python3 - "$seconds" "$@" <<'PY'
import os
import signal
import subprocess
import sys

deadline = float(sys.argv[1])
command = sys.argv[2:]
process = subprocess.Popen(command, start_new_session=True)
try:
    raise SystemExit(process.wait(timeout=deadline))
except subprocess.TimeoutExpired:
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    raise SystemExit(124)
PY
}

remove_phone_runner() {
    local devicectl_bin status
    if [ "$REMOVE_PHONE_RUNNER" != "1" ]; then
        info "phone WDA runner kept (default)"
        printf '%s\n' \
            "   To remove it later, pass --remove-phone-runner with the exact --udid and installed --bundle-id."
        return 0
    fi
    if [ "$DRY_RUN" = "1" ]; then
        plan "verify the exact device with devicectl: $PHONE_UDID"
        plan "uninstall exact phone bundle $PHONE_BUNDLE_ID from $PHONE_UDID (20-second devicectl timeout)"
        return 0
    fi

    if [ -n "${IPHONE_USE_DEVICECTL:-}" ]; then
        devicectl_bin="$IPHONE_USE_DEVICECTL"
    else
        command -v xcrun >/dev/null 2>&1 || {
            fail "xcrun/devicectl is unavailable; the local uninstall will continue"
            printf 'Run later: xcrun devicectl device uninstall app --device %s %s --timeout 20\n' \
                "$PHONE_UDID" "$PHONE_BUNDLE_ID" >&2
            return 1
        }
        devicectl_bin="$(xcrun -f devicectl 2>/dev/null || true)"
    fi
    [ -x "$devicectl_bin" ] || {
        fail "devicectl is unavailable: $devicectl_bin"
        return 1
    }

    info "verifying explicitly selected iPhone $PHONE_UDID"
    if run_with_deadline 15 "$devicectl_bin" device info details \
        --device "$PHONE_UDID" --timeout 10 >/dev/null; then
        :
    else
        status=$?
        fail "devicectl could not uniquely resolve $PHONE_UDID within the deadline (exit $status)"
        printf 'Run later: xcrun devicectl device uninstall app --device %s %s --timeout 20\n' \
            "$PHONE_UDID" "$PHONE_BUNDLE_ID" >&2
        return 1
    fi

    info "removing exact phone bundle $PHONE_BUNDLE_ID (deadline: 20 seconds)"
    if run_with_deadline 25 "$devicectl_bin" device uninstall app \
        --device "$PHONE_UDID" "$PHONE_BUNDLE_ID" --timeout 20; then
        ok "removed phone runner $PHONE_BUNDLE_ID from $PHONE_UDID"
    else
        status=$?
        fail "phone runner removal did not complete within the deadline (exit $status)"
        printf 'Verify the phone, then run: xcrun devicectl device uninstall app --device %s %s --timeout 20\n' \
            "$PHONE_UDID" "$PHONE_BUNDLE_ID" >&2
        return 1
    fi
}

preflight_phone_target() {
    local daemon_udid="" wda_udid="" daemon_bundle="" wda_bundle=""
    local canonical_udid="" canonical_bundle=""
    [ "$REMOVE_PHONE_RUNNER" = "1" ] || return 0

    if plist_owned daemon "$DAEMON_PLIST"; then
        daemon_udid="$(plist_get "$DAEMON_PLIST" EnvironmentVariables:PHONE_REMOTE_UDID)"
        daemon_bundle="$(plist_get "$DAEMON_PLIST" EnvironmentVariables:WDA_BUNDLE_ID)"
    fi
    if plist_owned wda "$WDA_PLIST"; then
        wda_udid="$(plist_get "$WDA_PLIST" EnvironmentVariables:WDA_UDID)"
        wda_bundle="$(plist_get "$WDA_PLIST" EnvironmentVariables:WDA_BUNDLE_ID)"
    fi

    if [ -n "$daemon_udid" ] && [ -n "$wda_udid" ] \
        && [ "$daemon_udid" != "$wda_udid" ]; then
        printf 'ERROR: daemon and WDA supervisor disagree on the canonical iPhone target; no phone app was removed.\n' >&2
        return 1
    fi
    canonical_udid="${daemon_udid:-$wda_udid}"
    if [ -z "$canonical_udid" ]; then
        printf 'ERROR: no ownership-verified canonical iPhone UDID is configured; refusing phone removal.\n' >&2
        printf '       Configure PHONE_REMOTE_UDID/WDA_UDID first, or use devicectl manually after verification.\n' >&2
        return 1
    fi
    if [ "$PHONE_UDID" != "$canonical_udid" ]; then
        printf 'ERROR: requested phone UDID does not match the canonical iphone-use target.\n' >&2
        printf '       requested: %s\n       configured: %s\n' "$PHONE_UDID" "$canonical_udid" >&2
        return 1
    fi

    if [ -n "$daemon_bundle" ] && [ -n "$wda_bundle" ] \
        && [ "$daemon_bundle" != "$wda_bundle" ]; then
        printf 'ERROR: daemon and WDA supervisor disagree on the configured WDA bundle id; no phone app was removed.\n' >&2
        return 1
    fi
    canonical_bundle="${daemon_bundle:-$wda_bundle}"
    if [ -z "$canonical_bundle" ]; then
        printf 'ERROR: no ownership-verified WDA bundle id is configured; refusing phone removal.\n' >&2
        return 1
    fi
    case "$PHONE_BUNDLE_ID" in
        "$canonical_bundle"|"$canonical_bundle.xctrunner") ;;
        *)
            printf 'ERROR: requested runner bundle does not match the configured WDA identity.\n' >&2
            printf '       requested: %s\n       configured base: %s\n' \
                "$PHONE_BUNDLE_ID" "$canonical_bundle" >&2
            return 1
            ;;
    esac
    return 0
}

preflight_phone_target || exit 2

CONFIGURED_WDA_REF=""
CONFIGURED_WDA_DIR=""
CONFIGURED_WDA_UDID=""
if plist_owned wda "$WDA_PLIST"; then
    CONFIGURED_WDA_REF="$(plist_get "$WDA_PLIST" EnvironmentVariables:WDA_REF)"
    CONFIGURED_WDA_DIR="$(plist_get "$WDA_PLIST" EnvironmentVariables:WDA_DIR)"
    CONFIGURED_WDA_UDID="$(plist_get "$WDA_PLIST" EnvironmentVariables:WDA_UDID)"
    if [ -n "$CONFIGURED_WDA_REF" ] \
        && { [ "${#CONFIGURED_WDA_REF}" -ne 40 ] \
            || printf '%s' "$CONFIGURED_WDA_REF" | grep -Eq '[^0-9A-Fa-f]'; }; then
        fail "configured WDA_REF is malformed; the checkout will be preserved"
    fi
    case "$CONFIGURED_WDA_UDID" in
        ''|*[!0-9A-Fa-f-]*)
            fail "configured WDA_UDID is malformed; the checkout will be preserved"
            ;;
    esac
fi
WDA_CHECKOUT_REASON=""
WDA_CHECKOUT_PRESENT=0
WDA_CHECKOUT_REMOVABLE=1
if path_exists "$WDA_CHECKOUT"; then
    WDA_CHECKOUT_PRESENT=1
    if [ -L "$WDA_CHECKOUT" ]; then
        WDA_CHECKOUT_REASON="checkout path is a symlink"
        WDA_CHECKOUT_REMOVABLE=0
    elif ! wda_checkout_owned; then
        WDA_CHECKOUT_REMOVABLE=0
    fi
fi

printf '\n=== iphone-use safe uninstall ===\n\n'
[ "$DRY_RUN" = "0" ] || info "dry-run: no process, file, app, or phone will be changed"

info "1/5 stopping the iphone-use daemon"
DAEMON_RUNTIME_CLEAN=1
LEGACY_RUNTIME_CLEAN=1
stop_owned_job daemon "$DAEMON_LABEL" "$DAEMON_PLIST" || DAEMON_RUNTIME_CLEAN=0
stop_owned_job legacy "$LEGACY_LABEL" "$LEGACY_PLIST" || LEGACY_RUNTIME_CLEAN=0

info "2/5 stopping the product-owned WDA supervisor and PID-verified processes"
WDA_RUNTIME_CLEAN=1
stop_owned_job wda "$WDA_LABEL" "$WDA_PLIST" || WDA_RUNTIME_CLEAN=0
if [ "$STATE_SAFE" = "1" ]; then
    stop_pid_record "$STATE_DIR/wda-runner.pid" runner || WDA_RUNTIME_CLEAN=0
    stop_pid_record "$STATE_DIR/wda-relay.pid" relay || WDA_RUNTIME_CLEAN=0
    stop_pid_record "$STATE_DIR/wda-mjpeg-relay.pid" mjpeg || WDA_RUNTIME_CLEAN=0
else
    warn "skipping WDA PID records because $STATE_DIR failed the namespace guard"
    WDA_RUNTIME_CLEAN=0
fi
if [ "$WDA_RUNTIME_CLEAN" != "1" ]; then
    warn "WDA shutdown was not fully ownership-verified; preserving its plist, PID records, logs, setup script, and checkout."
    warn "For each preserved *.pid, compare its first field with:"
    warn "  ps -ww -p <PID> -o uid=,lstart=,command="
    warn "Only after the UID/start-time/command identify this product, send 'kill -TERM <PID>' and rerun the uninstaller."
fi

info "3/5 handling the optional on-phone runner"
remove_phone_runner

info "4/5 removing verified LaunchAgents, apps, and logs"
if [ "$DAEMON_RUNTIME_CLEAN" = "1" ]; then
    DAEMON_ARTIFACTS_CLEAN=1
    remove_owned_app "$APP_PATH" "$APP_PATH" "$APP_BINARY" \
        "com.leeguoo.iphone-use" || DAEMON_ARTIFACTS_CLEAN=0
    remove_tree "$LOG_DIR" "$HOME/Library/Logs/iPhoneUse" \
        "$HOME/Library/Logs" || DAEMON_ARTIFACTS_CLEAN=0
    if [ "$DAEMON_ARTIFACTS_CLEAN" = "1" ]; then
        remove_owned_plist daemon "$DAEMON_PLIST" || DAEMON_ARTIFACTS_CLEAN=0
    fi
    [ "$DAEMON_ARTIFACTS_CLEAN" = "1" ] \
        || warn "preserved the current daemon plist when recovery metadata could still be needed"
else
    warn "preserved current daemon plist, app, and logs because its shutdown/ownership was not verified"
fi
if [ "$LEGACY_RUNTIME_CLEAN" = "1" ]; then
    LEGACY_ARTIFACTS_CLEAN=1
    remove_owned_app "$LEGACY_APP_PATH" "$LEGACY_APP_PATH" "$LEGACY_APP_BINARY" \
        "work.pwtk.iphone-remote" || LEGACY_ARTIFACTS_CLEAN=0
    remove_tree "$LEGACY_LOG_DIR" "$HOME/Library/Logs/iPhoneRemote" \
        "$HOME/Library/Logs" || LEGACY_ARTIFACTS_CLEAN=0
    if [ "$LEGACY_ARTIFACTS_CLEAN" = "1" ]; then
        remove_owned_plist legacy "$LEGACY_PLIST" || LEGACY_ARTIFACTS_CLEAN=0
        remove_owned_plist legacy "$LEGACY_DISABLED_PLIST" || LEGACY_ARTIFACTS_CLEAN=0
    fi
    [ "$LEGACY_ARTIFACTS_CLEAN" = "1" ] \
        || warn "preserved legacy daemon plist recovery metadata"
else
    warn "preserved legacy daemon plist, app, and logs because its shutdown/ownership was not verified"
fi
if [ "$WDA_RUNTIME_CLEAN" != "1" ] \
    || [ "$WDA_CHECKOUT_REMOVABLE" != "1" ] \
    || [ "$FAILED" != "0" ]; then
    warn "preserved WDA supervisor plist for recovery: $WDA_PLIST"
fi

info "5/5 removing iphone-use-owned WDA checkout and state"
STATE_TREE_REMOVABLE=1
if [ "$WDA_RUNTIME_CLEAN" != "1" ]; then
    warn "preserved $STATE_DIR because a WDA process/supervisor could still be active"
    STATE_TREE_REMOVABLE=0
elif [ "$STATE_SAFE" = "1" ] && [ "$WDA_CHECKOUT_PRESENT" = "1" ]; then
    if [ "$WDA_CHECKOUT_REMOVABLE" = "1" ]; then
        # The first proof happened before process shutdown and optional device
        # work. Re-run the full marker/Git proof immediately before rm -rf so a
        # checkout that became dirty or gained refs/worktrees is preserved.
        if wda_checkout_owned; then
            remove_tree "$WDA_CHECKOUT" "$HOME/.iphone-use/WebDriverAgent" "$STATE_DIR"
        else
            fail "preserving WDA checkout after final ownership recheck: ${WDA_CHECKOUT_REASON:-ownership changed}: $WDA_CHECKOUT"
            STATE_TREE_REMOVABLE=0
        fi
    else
        fail "preserving WDA checkout: ${WDA_CHECKOUT_REASON:-ownership could not be verified}: $WDA_CHECKOUT"
        STATE_TREE_REMOVABLE=0
    fi
fi

if [ "$FAILED" = "0" ] \
    && [ "$WDA_RUNTIME_CLEAN" = "1" ] \
    && [ "$STATE_SAFE" = "1" ]; then
    for state_name in \
        wda-runner.log \
        wda-runner.pid \
        wda-relay.log \
        wda-relay.pid \
        wda-mjpeg-relay.log \
        wda-mjpeg-relay.pid \
        wda-agent.log \
        wda-setup-status.json \
        wda-checkout-owner.v1 \
        setup-wda.rollback.sh \
        wda-supervisor.rollback.plist \
        setup-wda.sh
    do
        remove_file "$STATE_DIR/$state_name" "$HOME/.iphone-use/$state_name"
        [ "$FAILED" = "0" ] || break
    done
    [ "$FAILED" != "0" ] || remove_state_transients
fi

FINALIZE_STATE=0
STATE_UNKNOWN=""
if [ "$FAILED" = "0" ] \
    && [ "$WDA_RUNTIME_CLEAN" = "1" ] \
    && [ "$STATE_SAFE" = "1" ] \
    && [ "$STATE_TREE_REMOVABLE" = "1" ]; then
    if [ ! -d "$STATE_DIR" ]; then
        FINALIZE_STATE=1
    elif ! STATE_UNKNOWN="$(state_unknown_entries)"; then
        fail "could not inspect the remaining state directory; preserving it"
    elif [ -z "$STATE_UNKNOWN" ]; then
        FINALIZE_STATE=1
    else
        fail "unrecognized or unsafe entries remain in $STATE_DIR"
        printf '%s\n' "$STATE_UNKNOWN" | sed 's/^/    /' >&2
    fi
fi

if [ "$FINALIZE_STATE" = "1" ]; then
    # Keep both WDA configuration and the retry entrypoint until every earlier
    # process/file operation has succeeded.
    remove_owned_plist wda "$WDA_PLIST"
    if [ "$FAILED" = "0" ] && [ -d "$STATE_DIR" ]; then
        if [ "$DRY_RUN" = "1" ]; then
            plan "remove file $STATE_DIR/uninstall.sh last (if installed)"
            plan "remove empty directory $STATE_DIR"
        else
            remove_file "$STATE_DIR/uninstall.sh" "$HOME/.iphone-use/uninstall.sh"
            if [ "$FAILED" = "0" ] \
                && [ -z "$(find "$STATE_DIR" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]; then
                if rmdir "$STATE_DIR"; then
                    ok "removed empty $STATE_DIR"
                else
                    fail "could not remove empty state directory: $STATE_DIR"
                fi
            elif [ "$FAILED" = "0" ]; then
                fail "state directory gained or retained entries during final cleanup: $STATE_DIR"
            fi
        fi
    fi
elif [ -d "$STATE_DIR" ]; then
    warn "preserved retryable $STATE_DIR because cleanup was incomplete or unknown files remain"
fi

printf '\n'
if [ "$FAILED" = "0" ]; then
    if [ "$DRY_RUN" = "1" ]; then
        ok "dry-run complete; every listed destructive action was ownership-gated"
    else
        ok "local iphone-use uninstall complete"
    fi
    printf '%s\n' "Shared Xcode/Homebrew tools, iproxy, and socat were not changed."
    exit 0
fi

printf 'Uninstall finished with safety refusals/errors above; unverified artifacts were preserved.\n' >&2
printf 'Shared Xcode/Homebrew tools, iproxy, and socat were not changed.\n' >&2
exit 1
