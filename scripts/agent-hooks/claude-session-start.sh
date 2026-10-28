#!/bin/sh
# Claude Code SessionStart hook: reports this agent's resume command to warden.
#
# Reads the hook's JSON on stdin, takes `session_id`, and — only inside a Rex terminal
# (`$REX_BLOCK` set) — writes the private APC
#   ESC _ warden1;resume=<base64 of "claude --resume <session_id>"> ESC \
# to the agent's terminal. It rides Rex's raw data stream to warden's relay, which strips it.
# Prints nothing to stdout (Claude Code adds a SessionStart hook's stdout to the agent's context)
# and always exits 0, silently, so a hook failure never blocks the agent starting.
#
# Claude Code runs a hook in its own session with no controlling terminal, so `/dev/tty` can't be
# opened (and `[ -w /dev/tty ]` is true regardless, mode 666). The agent itself holds the
# terminal: walk up from the parent to the first process with a tty and write to that device.

input=$(cat)
[ -n "$REX_BLOCK" ] || exit 0

id=$(printf '%s' "$input" | tr -d '\n' |
	sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
[ -n "$id" ] || exit 0

tty=
pid=$PPID
hops=0
while [ "$hops" -lt 8 ] && [ -n "$pid" ] && [ "$pid" -gt 1 ] 2>/dev/null; do
	t=$(ps -o tty= -p "$pid" 2>/dev/null | tr -d ' ')
	case $t in
	'' | '??' | '?') ;;
	*)
		tty=$t
		break
		;;
	esac
	pid=$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')
	hops=$((hops + 1))
done
[ -n "$tty" ] || exit 0

b64=$(printf 'claude --resume %s' "$id" | base64 | tr -d '\n')
esc=$(printf '\033')
# One printf of the whole sequence: one write, so the agent's own output can't split it.
{ printf '%s' "${esc}_warden1;resume=${b64}${esc}\\" >"/dev/$tty"; } 2>/dev/null
exit 0
