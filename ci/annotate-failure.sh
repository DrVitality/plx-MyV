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
    part=$(grep -v 'host link: could not discover\| Compiling ' "$log" | grep -E -A8 '^(error|warning)(\[[A-Z0-9]+\])?:|panicked at|^test .* FAILED|^failures:|^FAIL|Error:|error:|^::error' | head -n 160)
    [ -n "$part" ] && out="$out$(basename "$log"):
$part
"
done
[ -n "$out" ] || exit 0
# Workflow-command escaping: %, CR and LF.
msg=$(printf '%s' "$out" | head -c 60000 | sed -e 's/%/%25/g' -e 's/\r/%0D/g' | awk 'BEGIN{ORS="%0A"} {print}')
echo "::error title=build errors::$msg"
