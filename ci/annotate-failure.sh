#!/bin/sh
# Turn the error lines of one or more build logs into a single GitHub annotation.
#
# A job's raw log is only readable through the Actions log API; its annotations are readable
# through the plain check-runs API too. This fork is developed from a session that can reach the
# second and not the first, so when a cargo or gate step fails, the job ends with this step and the
# errors that matter arrive where they can be read. Absent or empty logs are skipped.
set -u
out=""
for log in "$@"; do
    [ -s "$log" ] || continue
    # Every failing test's NAME first (one line each, so none is cut off by a long panic above
    # it), then each panic with its assertion, then compiler and gate errors with context.
    clean=$(grep -v 'host link: could not discover\| Compiling ' "$log")
    part=$( {
        printf '%s\n' "$clean" | grep -E '^test .* FAILED' | head -n 80
        printf '%s\n' "$clean" | grep -E -A5 'panicked at' | head -n 240
        printf '%s\n' "$clean" | grep -E -A8 '^(error|warning)(\[[A-Z0-9]+\])?:|^FAIL|Error:|error:|^::error' | head -n 160
    } )
    [ -n "$part" ] && out="$out$(basename "$log"):
$part
"
done
[ -n "$out" ] || exit 0
# Workflow-command escaping: %, CR and LF.
msg=$(printf '%s' "$out" | head -c 60000 | sed -e 's/%/%25/g' -e 's/\r/%0D/g' | awk 'BEGIN{ORS="%0A"} {print}')
echo "::error title=build errors::$msg"

# The whole of every failing test's panic (an assertion's `left:` can be a long golden text the
# summary above cuts off), in pieces small enough for one annotation each — warnings, then notices,
# ten of each per step. Only `left:` (what the test GOT) is kept of an assert_eq: the expectation
# is already in the tree.
for log in "$@"; do
    [ -s "$log" ] || continue
    grep -q 'panicked at' "$log" || continue
    full=$(awk '/panicked at/{p=1} p&&/^(note: run with|thread .* panicked at)/&&!/panicked at/{p=0} p' "$log" \
        | grep -v '^ *right: \|^note: run with')
    printf '%s' "$full" | fold -b -w 3900 | awk -v n=0 '
        { chunk = chunk $0 "\n"; if (length(chunk) > 3800) { emit(); } }
        END { if (length(chunk)) emit(); }
        function emit(   msg, kind) {
            n++; if (n > 20) return;
            kind = (n <= 10) ? "warning" : "notice";
            msg = chunk; gsub(/%/, "%25", msg); gsub(/\r/, "%0D", msg); gsub(/\n/, "%0A", msg);
            printf "::%s title=panic part %d::%s\n", kind, n, msg; chunk = "";
        }'
done
