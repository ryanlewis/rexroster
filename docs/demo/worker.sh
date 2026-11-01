#!/bin/sh
# A pretend worker for the demo. It prints a short log and sends Rex
# program-status records (OSC 7501) the way Claude Code does, so the roster
# and the watcher react to it as they would to a real agent.
#
# Usage: worker.sh <scenario>   (fix-login | add-metrics | bump-deps | write-docs)

set -u

status() { # status <state> <message> [extra pairs]
	msg=$(printf %s "$2" | base64 | tr -d '\n')
	printf '\033]7501;state=%s:app=demo:msg=%s%s\033\\' "$1" "$msg" "${3:-}" >/dev/tty
}

step() { # step <seconds> <message>
	status working "$2"
	printf '\033[2m●\033[0m %s\n' "$2"
	sleep "$1"
}

finish() { # finish <state> <message>
	status "$1" "$2"
	case $1 in
	done) printf '\n\033[32m✓\033[0m %s\n' "$2" ;;
	error) printf '\n\033[31m✗\033[0m %s\n' "$2" ;;
	esac
	# Stay alive so the roster keeps the worker; the demo closes it.
	while :; do sleep 3600; done
}

status idle "Starting"
sleep 1

case ${1:-} in
fix-login)
	step 3 "Reading src/auth/redirect.rs"
	step 4 "Editing src/auth/redirect.rs"
	step 5 "Running cargo test auth::"
	finish "done" "Fixed the login redirect; 42 tests pass"
	;;
add-metrics)
	step 3 "Reading src/server.rs"
	step 3 "Adding prometheus 0.14"
	step 4 "Wiring request counters into the router"
	step 3 "Running cargo test"
	finish "done" "Added /metrics with request and latency counters"
	;;
bump-deps)
	step 3 "Running cargo update"
	step 4 "Running cargo deny check"
	step 3 "Running cargo test"
	finish "done" "Updated 14 crates; all checks pass"
	;;
write-docs)
	step 4 "Reading README.md"
	step 5 "Drafting docs/config.md"
	step 5 "Checking every example in docs/config.md"
	finish "done" "Wrote docs/config.md"
	;;
*)
	echo "unknown scenario: ${1:-}" >&2
	exit 2
	;;
esac
