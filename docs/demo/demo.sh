#!/usr/bin/env bash
# Demo of rexroster for a screen recording. Run it from a Rex pane:
#
#   docs/demo/demo.sh
#
# It starts four pretend workers (docs/demo/worker.sh) that report status the
# way Claude Code does. The roster opens with the first one, and over about
# 25 seconds each worker works through a few steps and finishes. Press Enter
# here at the end to close the workers one by one.
#
# Set REXROSTER to use a binary other than the rexroster on PATH, for example
# REXROSTER=target/release/rexroster.

set -eu

: "${REX_SESSION:?run this from a Rex pane}"
: "${REX_BLOCK:?run this from a Rex pane}"

rr=${REXROSTER:-rexroster}
here=$(cd "$(dirname "$0")" && pwd)
worker=$here/worker.sh
blocks=

type_out() { # print a command as if typed
	printf '\033[1;32m❯\033[0m '
	s=$1
	for ((i = 0; i < ${#s}; i++)); do
		printf %s "${s:i:1}"
		sleep 0.005
	done
	printf '\n'
}

spawn() { # spawn <name> <task>
	type_out "rexroster spawn \$REX_BLOCK $1 --task \"$2\" -- claude …"
	out=$("$rr" spawn "$REX_BLOCK" "$1" --task "$2" --cwd "$PWD" -- "$worker" "$1")
	id=$(printf %s "$out" | sed -n 's/.*"block_id":"\([^"]*\)".*/\1/p')
	blocks="$blocks $id"
	printf '\033[2m%s\033[0m\n' "$out"
}

cleanup() {
	# shellcheck disable=SC2086
	[ -n "$blocks" ] && rex block close -s "$REX_SESSION" $blocks >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

clear
printf '\033[2m# a lead hands out four tasks\033[0m\n\n'
sleep 1
spawn fix-login "fix the login redirect"
sleep 0.8
spawn add-metrics "add a /metrics endpoint"
sleep 0.8
spawn bump-deps "update dependencies"
sleep 0.8
spawn write-docs "document the config file"

printf '\n\033[2m# workers run detached; the roster shows each one. Enter closes them.\033[0m\n'
read -r _

# Close the workers one by one; the roster closes itself after the last.
for id in $blocks; do
	rex block close -s "$REX_SESSION" "$id" >/dev/null 2>&1 || true
	sleep 0.8
done
blocks=
