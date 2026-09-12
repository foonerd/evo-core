#!/usr/bin/env bash
#
# ui-release-gate.test.sh — /opt/evo/ui/current never points at an
# empty release.
#
# Three layers, all against the shipped script, never a copy of it:
#
#   1. The predicate. `ui_release_is_populated` is extracted from
#      ui-deploy-release.sh and driven directly, so the rules the
#      deploy enforces are the rules under test.
#
#   2. The path. ui-deploy-release.sh is run end to end against a
#      simulated target: ssh, scp, rsync, cargo and npm are stubs on
#      PATH, and the target's filesystem is a directory tree. The
#      deploy's own `ln -sfn` really runs in that tree, so "current
#      did not move" is read off the symlink itself rather than
#      asserted about the source.
#
#   3. Privilege. The same simulated target is run with and without
#      passwordless sudo, and with and without a terminal, so the
#      phase 0 decision is exercised rather than described.
#
# Usage:
#   scripts/install/tests/ui-release-gate.test.sh
#
# Exits 0 only when every case passes. Touches nothing outside its
# own temporary directory and never contacts a real host.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DEPLOY="${REPO_ROOT}/scripts/install/ui-deploy-release.sh"

PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }
check() { if [[ "$1" == "$2" ]]; then ok "$3"; else bad "$3" "expected '$2', got '$1'"; fi; }

if [[ ! -f "${DEPLOY}" ]]; then
    echo "FAIL: ${DEPLOY} not found" >&2
    exit 1
fi

# ---------------------------------------------------------------
# Layer 1: the predicate, extracted from the shipped script.
# ---------------------------------------------------------------
echo "[1/2] ui_release_is_populated (extracted from ui-deploy-release.sh)"

FN="$(awk '/^ui_release_is_populated\(\) \{$/,/^\}$/' "${DEPLOY}")"
if [[ -z "${FN}" ]]; then
    echo "  FAIL: ui_release_is_populated not found in ${DEPLOY}." >&2
    echo "        The deploy has no populated-release gate, or it was renamed." >&2
    exit 1
fi
eval "${FN}"

# index_bytes  remote_files  local_files  expected_rc  name
while read -r ib rf lf want name; do
    [[ -z "${ib}" || "${ib}" == \#* ]] && continue
    ui_release_is_populated "${ib}" "${rf}" "${lf}"
    check "$?" "${want}" "${name}"
done <<'CASES'
4096 27 27 0 a_complete_copy_is_populated
1    1  1  0 a_single_byte_index_alone_is_populated
0    0  27 1 an_empty_release_is_refused
0    26 27 1 a_release_without_index_html_is_refused
4096 0  27 1 index_reported_but_no_files_is_refused
4096 26 27 1 a_short_copy_is_refused
4096 28 27 1 more_files_than_were_sent_is_refused
0    0  0  1 nothing_sent_and_nothing_landed_is_refused
CASES

# A non-numeric fact (an ssh that returned noise) must refuse, not admit.
ui_release_is_populated "" "" "" 2>/dev/null
check "$?" "1" "empty_facts_are_refused"

# The gate must be reached before the symlink, in file order.
VERIFY_LINE="$(grep -n 'if ! ui_release_is_populated' "${DEPLOY}" | head -1 | cut -d: -f1)"
LN_LINE="$(grep -n 'ln -sfn' "${DEPLOY}" | head -1 | cut -d: -f1)"
if [[ -n "${VERIFY_LINE}" && -n "${LN_LINE}" && "${VERIFY_LINE}" -lt "${LN_LINE}" ]]; then
    ok "the_verify_gate_precedes_ln_sfn (line ${VERIFY_LINE} < ${LN_LINE})"
else
    bad "the_verify_gate_precedes_ln_sfn" "verify at '${VERIFY_LINE}', ln -sfn at '${LN_LINE}'"
fi

# rsync must be checked for on the target, not assumed.
if grep -q "command -v rsync" "${DEPLOY}"; then
    ok "the_target_is_checked_for_rsync"
else
    bad "the_target_is_checked_for_rsync" "no 'command -v rsync' probe in ${DEPLOY}"
fi

echo

# ---------------------------------------------------------------
# Layer 2: the whole script, against a simulated target.
# ---------------------------------------------------------------
echo "[2/2] ui-deploy-release.sh end to end (stubbed target)"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

BIN="${WORK}/bin"
mkdir -p "${BIN}"

# --- stub: ssh. Runs the remote command against ${FAKE_ROOT}. -----
cat > "${BIN}/ssh" <<'STUB'
#!/usr/bin/env bash
tty_flag="no-tty"
while [[ $# -gt 0 ]]; do
    case "$1" in
        -o) shift 2 ;;
        -t|-tt) tty_flag="tty"; shift ;;
        -q|-n|-T) shift ;;
        -*) shift ;;
        *) break ;;
    esac
done
shift            # the target
cmd="$*"

printf '%s\t%s\n' "${tty_flag}" "${cmd}" >> "${FAKE_LOG}"

# Facts the simulated target decides, before anything is executed.
if [[ "${cmd}" == *"command -v rsync"* ]]; then
    [[ "${FAKE_TARGET_HAS_RSYNC:-1}" == "1" ]] && exit 0 || exit 1
fi
if [[ "${cmd}" == "sudo -n true" ]]; then
    [[ "${FAKE_SUDO_N:-1}" == "1" ]] && exit 0 || exit 1
fi
if [[ "${cmd}" == "sudo -v" ]]; then
    # An interactive sudo is only answerable on a terminal.
    [[ "${tty_flag}" == "tty" ]] || exit 1
    [[ "${FAKE_SUDO_INTERACTIVE:-1}" == "1" ]] && exit 0 || exit 1
fi

# Order matters: ${FAKE_ROOT} itself sits under /tmp, so /tmp/ is
# rewritten first and the prefixes inserted after it are left alone.
cmd="${cmd//\/tmp\//${FAKE_ROOT}/tmp/}"
cmd="${cmd//\/etc\//${FAKE_ROOT}/etc/}"
cmd="${cmd//\/opt\/evo/${FAKE_ROOT}/opt/evo}"

exec bash -c '
    sudo() { while [[ $# -gt 0 ]]; do case "$1" in -n|-A|-H|-E) shift;; -u) shift 2;; *) break;; esac; done; "$@"; }
    install() { local a=(); while [[ $# -gt 0 ]]; do case "$1" in -o|-g) shift 2;; *) a+=("$1"); shift;; esac; done; command install "${a[@]}"; }
    chown() { :; }
    systemctl() { :; }
    curl() { :; }
    uname() { if [[ "${1:-}" == "-m" ]]; then echo "${FAKE_TARGET_ARCH:-x86_64}"; else command uname "$@"; fi; }
    '"${cmd}"'
'
STUB

# --- stub: rsync. Mode decides what the transfer does. ------------
cat > "${BIN}/rsync" <<'STUB'
#!/usr/bin/env bash
printf 'rsync\t%s\n' "$*" >> "${FAKE_LOG}"

# sudo cannot prompt through rsync's protocol stream: a target
# without passwordless sudo refuses --rsync-path='sudo rsync'.
if [[ "$*" == *"--rsync-path=sudo rsync"* && "${FAKE_SUDO_N:-1}" != "1" ]]; then
    echo "sudo: no tty present and no askpass program specified" >&2
    exit 12
fi

src=""; dst=""
for a in "$@"; do
    case "${a}" in -*) ;; *) src="${dst}"; dst="${a}" ;; esac
done
dst="${dst#*:}"
dst="${FAKE_ROOT}${dst}"

# A target with no rsync cannot be copied to at all, whatever the
# mode asks for: the remote end of the transfer does not exist.
if [[ "${FAKE_TARGET_HAS_RSYNC:-1}" != "1" ]]; then
    echo "sudo: rsync: command not found" >&2
    echo "rsync: connection unexpectedly closed" >&2
    exit 12
fi

case "${FAKE_RSYNC_MODE:-ok}" in
    ok)
        mkdir -p "${dst}"
        cp -a "${src}." "${dst}/"
        ;;
    short)
        # Everything but one asset arrives: a truncated transfer that
        # rsync itself did not report.
        mkdir -p "${dst}"
        cp -a "${src}." "${dst}/"
        rm -f "${dst}/assets/app.js"
        ;;
    empty)
        # Exits clean having moved nothing: the shape a no-op or
        # emptied source leaves behind.
        mkdir -p "${dst}"
        ;;
    fail)
        echo "rsync: connection unexpectedly closed" >&2
        exit 12
        ;;
esac
exit 0
STUB

# --- stub: scp, cargo, npm. ---------------------------------------
cat > "${BIN}/scp" <<'STUB'
#!/usr/bin/env bash
args=(); for a in "$@"; do [[ "${a}" == -* ]] || args+=("${a}"); done
dst="${args[-1]#*:}"
mkdir -p "$(dirname "${FAKE_ROOT}${dst}")"
cp -f "${args[-2]}" "${FAKE_ROOT}${dst}"
STUB

cat > "${BIN}/cargo" <<'STUB'
#!/usr/bin/env bash
triple=""
while [[ $# -gt 0 ]]; do
    [[ "$1" == "--target" ]] && triple="$2"
    shift
done
mkdir -p "target/${triple}/release"
printf '#!/bin/true\n' > "target/${triple}/release/evo-ui-runtime"
chmod 0755 "target/${triple}/release/evo-ui-runtime"
STUB

printf '#!/usr/bin/env bash\nexit 0\n' > "${BIN}/npm"
chmod 0755 "${BIN}"/*

# --- fixture: a UI repo with a built dist, and a target tree ------
UI_REPO="${WORK}/evo-ui-eng"
mkdir -p "${UI_REPO}/apps/evo-ui-runtime" "${UI_REPO}/apps/evo-ui-shell/dist/assets"
echo '<!doctype html><title>evo</title>' > "${UI_REPO}/apps/evo-ui-shell/dist/index.html"
echo 'console.log(0)'                   > "${UI_REPO}/apps/evo-ui-shell/dist/assets/app.js"
echo 'body{}'                           > "${UI_REPO}/apps/evo-ui-shell/dist/assets/app.css"

PREVIOUS_ID="19990101T000000Z"

FAKE_ROOT="${WORK}/target"
FAKE_LOG="${WORK}/ssh.log"
export FAKE_ROOT FAKE_LOG

reset_target() {
    rm -rf "${FAKE_ROOT}"
    mkdir -p "${FAKE_ROOT}/opt/evo/bin" \
             "${FAKE_ROOT}/opt/evo/ui/releases/${PREVIOUS_ID}" \
             "${FAKE_ROOT}/etc/systemd/system" \
             "${FAKE_ROOT}/tmp"
    echo '<!doctype html><title>previous</title>' \
        > "${FAKE_ROOT}/opt/evo/ui/releases/${PREVIOUS_ID}/index.html"
    ln -sfn "/opt/evo/ui/releases/${PREVIOUS_ID}" "${FAKE_ROOT}/opt/evo/ui/current"
    : > "${FAKE_ROOT}/etc/systemd/system/evo-ui.service"
    : > "${FAKE_LOG}"
    export FAKE_SUDO_N=1 FAKE_SUDO_INTERACTIVE=1
}

# The symlink the deploy wrote, as the target itself would see it:
# the stub rewrites absolute paths into the simulated root, so that
# prefix comes back off here.
current_points_at() {
    local link; link="$(readlink "${FAKE_ROOT}/opt/evo/ui/current")"
    printf '%s' "${link#${FAKE_ROOT}}"
}

# Run the shipped deploy against the simulated target.
# Run the shipped deploy against the simulated target. A third
# argument of 1 gives it a real pty, which is what distinguishes an
# operator at a terminal from a scripted run.
run_deploy() {
    export PATH="${BIN}:${PATH}"
    export UI_REPO_ROOT="${UI_REPO}"
    export FAKE_TARGET_ARCH="x86_64"
    export FAKE_TARGET_HAS_RSYNC="${1}"
    export FAKE_RSYNC_MODE="${2}"
    unset CARGO_TARGET_DIR

    if [[ "${3:-0}" == "1" ]]; then
        script -qec "bash '${DEPLOY}' ui-test-target evouser" /dev/null \
            >"${WORK}/out.txt" 2>&1
    else
        bash "${DEPLOY}" ui-test-target evouser </dev/null \
            >"${WORK}/out.txt" 2>&1
    fi
    echo "$?"
}

# has_rsync  rsync_mode  expect_rc  expect_current  name
run_case() {
    local has="$1" mode="$2" want_rc="$3" want_cur="$4" name="$5"
    reset_target
    local rc; rc="$(run_deploy "${has}" "${mode}" "${6:-0}")"
    local cur; cur="$(current_points_at)"

    if [[ "${want_rc}" == "0" ]]; then
        [[ "${rc}" == "0" ]] || { bad "${name}" "deploy exited ${rc}; $(tail -3 "${WORK}/out.txt")"; return; }
    else
        [[ "${rc}" != "0" ]] || { bad "${name}" "deploy exited 0, expected a refusal"; return; }
    fi

    if [[ "${want_cur}" == "previous" ]]; then
        if [[ "${cur}" == "/opt/evo/ui/releases/${PREVIOUS_ID}" ]]; then
            ok "${name} (rc=${rc}, current still ${PREVIOUS_ID})"
        else
            bad "${name}" "current moved to '${cur}'"
        fi
    else
        if [[ "${cur}" != "/opt/evo/ui/releases/${PREVIOUS_ID}" && -n "${cur}" ]]; then
            if [[ -s "${FAKE_ROOT}${cur}/index.html" ]]; then
                ok "${name} (current -> ${cur##*/}, index.html present)"
            else
                bad "${name}" "current moved to an index-less release '${cur}'"
            fi
        else
            bad "${name}" "current did not move (still '${cur}')"
        fi
    fi
}

run_case 0 ok    1 previous  a_target_without_rsync_leaves_current_on_the_previous_release
run_case 1 fail  1 previous  a_failed_transfer_leaves_current_on_the_previous_release
run_case 1 empty 1 previous  a_clean_rsync_that_copied_nothing_leaves_current_on_the_previous_release
run_case 1 short 1 previous  a_short_copy_leaves_current_on_the_previous_release
run_case 1 ok    0 new       a_good_copy_verifies_then_repoints_current

# The refusal has to say where the target stands, not just that it failed.
reset_target
run_deploy 0 ok >/dev/null
if grep -q 'has NOT been moved' "${WORK}/out.txt"; then
    ok "the_refusal_states_current_was_not_moved"
else
    bad "the_refusal_states_current_was_not_moved" "$(tail -3 "${WORK}/out.txt")"
fi

echo

# ---------------------------------------------------------------
# Layer 3: phase 0 privilege mode.
# ---------------------------------------------------------------
echo "[3/3] phase 0: a target without passwordless sudo still deploys"

reached_phase_1() { grep -q '\[1/8\]' "${WORK}/out.txt"; }
asked_sudo_n()    { grep -q $'\tsudo -n true$' "${FAKE_LOG}"; }
used_a_terminal() { grep -q $'^tty\t' "${FAKE_LOG}"; }
rsync_argv()      { grep $'^rsync\t' "${FAKE_LOG}" | head -1; }

# A box that still has sudo -n: nothing about the run changes.
reset_target
rc="$(run_deploy 1 ok 0)"
if [[ "${rc}" == "0" ]] && ! used_a_terminal; then
    ok "a_box_with_sudo_n_never_allocates_a_terminal"
else
    bad "a_box_with_sudo_n_never_allocates_a_terminal" "rc=${rc}, tty used: $(used_a_terminal && echo yes || echo no)"
fi
if [[ "$(rsync_argv)" == *"--rsync-path=sudo rsync"* ]]; then
    ok "a_box_with_sudo_n_transfers_exactly_as_before"
else
    bad "a_box_with_sudo_n_transfers_exactly_as_before" "$(rsync_argv)"
fi

# No passwordless sudo, operator at a terminal: the deploy continues.
reset_target
export FAKE_SUDO_N=0
rc="$(run_deploy 1 ok 1)"
cur="$(current_points_at)"
if [[ "${rc}" == "0" ]] && reached_phase_1; then
    ok "no_sudo_n_with_a_terminal_reaches_phase_1_and_completes"
else
    bad "no_sudo_n_with_a_terminal_reaches_phase_1_and_completes" "rc=${rc}; $(tail -3 "${WORK}/out.txt")"
fi
if used_a_terminal; then
    ok "no_sudo_n_runs_privileged_steps_over_a_terminal"
else
    bad "no_sudo_n_runs_privileged_steps_over_a_terminal" "no ssh -t recorded"
fi
if [[ "$(rsync_argv)" != *"--rsync-path"* ]]; then
    ok "no_sudo_n_transfers_without_asking_rsync_to_sudo"
else
    bad "no_sudo_n_transfers_without_asking_rsync_to_sudo" "$(rsync_argv)"
fi
if [[ "${cur}" != "/opt/evo/ui/releases/${PREVIOUS_ID}" && -n "${cur}" ]]; then
    ok "no_sudo_n_still_repoints_current_on_a_good_copy"
else
    bad "no_sudo_n_still_repoints_current_on_a_good_copy" "current '${cur}'"
fi

# The release gate is not weakened by the interactive path.
reset_target
export FAKE_SUDO_N=0
rc="$(run_deploy 1 empty 1)"
if [[ "${rc}" != "0" && "$(current_points_at)" == "/opt/evo/ui/releases/${PREVIOUS_ID}" ]]; then
    ok "the_release_gate_still_holds_on_the_interactive_path"
else
    bad "the_release_gate_still_holds_on_the_interactive_path" "rc=${rc}, current $(current_points_at)"
fi

# No passwordless sudo and no terminal: refuse at phase 0, before the build.
reset_target
export FAKE_SUDO_N=0
rc="$(run_deploy 1 ok 0)"
if [[ "${rc}" != "0" ]] && ! reached_phase_1; then
    ok "no_sudo_n_and_no_terminal_refuses_before_the_build"
else
    bad "no_sudo_n_and_no_terminal_refuses_before_the_build" "rc=${rc}, reached phase 1: $(reached_phase_1 && echo yes || echo no)"
fi
if grep -q 'no terminal' "${WORK}/out.txt"; then
    ok "the_no_terminal_refusal_names_the_remedy"
else
    bad "the_no_terminal_refusal_names_the_remedy" "$(tail -3 "${WORK}/out.txt")"
fi

# sudo itself refused: still a refusal, and still not a NOPASSWD demand.
reset_target
export FAKE_SUDO_N=0 FAKE_SUDO_INTERACTIVE=0
rc="$(run_deploy 1 ok 1)"
if [[ "${rc}" != "0" ]] && ! reached_phase_1; then
    ok "sudo_refused_outright_stops_at_phase_0"
else
    bad "sudo_refused_outright_stops_at_phase_0" "rc=${rc}"
fi
if grep -q 'NOPASSWD is not required' "${WORK}/out.txt"; then
    ok "the_sudo_refusal_does_not_demand_nopasswd"
else
    bad "the_sudo_refusal_does_not_demand_nopasswd" "$(tail -3 "${WORK}/out.txt")"
fi

# The footprint checks still fail first: neither is a sudo problem.
reset_target
rm -f "${FAKE_ROOT}/etc/systemd/system/evo-ui.service"
rc="$(run_deploy 1 ok 0)"
if [[ "${rc}" != "0" ]] && ! asked_sudo_n && grep -q 'evo-ui.service unit not installed' "${WORK}/out.txt"; then
    ok "a_missing_unit_fails_before_the_privilege_check"
else
    bad "a_missing_unit_fails_before_the_privilege_check" "rc=${rc}, sudo probed: $(asked_sudo_n && echo yes || echo no)"
fi

reset_target
rm -rf "${FAKE_ROOT}/opt/evo/ui/releases"
rc="$(run_deploy 1 ok 0)"
if [[ "${rc}" != "0" ]] && ! asked_sudo_n && grep -q 'releases missing' "${WORK}/out.txt"; then
    ok "a_missing_releases_dir_fails_before_the_privilege_check"
else
    bad "a_missing_releases_dir_fails_before_the_privilege_check" "rc=${rc}, sudo probed: $(asked_sudo_n && echo yes || echo no)"
fi

# The script must not be the thing that widens sudoers. Match the
# grant itself - a sudoers path, visudo, or the "NOPASSWD:" rule
# syntax - not the word, which the refusal text legitimately uses.
SUDOERS_WRITE="$(grep -nE 'NOPASSWD:|/etc/sudoers|sudoers\.d|visudo' "${DEPLOY}" || true)"
if [[ -z "${SUDOERS_WRITE}" ]]; then
    ok "the_deploy_never_grants_passwordless_sudo"
else
    bad "the_deploy_never_grants_passwordless_sudo" "${SUDOERS_WRITE}"
fi

echo
printf '%s: %d passed, %d failed\n' "$(basename "${BASH_SOURCE[0]}")" "${PASS}" "${FAIL}"
[[ "${FAIL}" -eq 0 ]]
