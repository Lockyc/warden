#!/bin/sh
# Claude Code hook (SessionStart, UserPromptSubmit, Stop): tells warden how to resume this agent
# and what the summary block shows for it. Does nothing outside a Rex terminal (`$REX_BLOCK`).
#
# - SessionStart writes the private APC
#     ESC _ warden1;resume=<base64 of "claude --resume <session_id>"> ESC \
#   which rides Rex's raw data stream to warden's relay, which strips it. Unless the session is
#   only compacting, it also clears the `warden` OSC 7501 records below: a new conversation
#   starts with empty rows, and a resumed one refills them from its transcript.
# - UserPromptSubmit and Stop write an OSC 7501 child record, `warden/asked` (the prompt) or
#   `warden/said` (the agent's reply): the first non-blank line, at most 200 characters. Rex keeps
#   the record on the block, and warden reads it from Rex. These need `jq`; without it they do
#   nothing.
#
# Prints nothing to stdout (Claude Code adds SessionStart's and UserPromptSubmit's stdout to the
# agent's context) and always exits 0, silently, so a hook failure never blocks the agent.
#
# Claude Code runs a hook in its own session with no controlling terminal, so `/dev/tty` can't be
# opened (and `[ -w /dev/tty ]` is true regardless, mode 666). The agent itself holds the
# terminal: walk up from the parent to the first process with a tty and write to that device.

input=$(cat)
[ -n "$REX_BLOCK" ] || exit 0

# A top-level string field holding no escapes (an event name, a session id, a source).
field() {
	printf '%s' "$input" | tr -d '\n' |
		sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p"
}

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
esc=$(printf '\033')

# One printf of the whole sequence: one write, so the agent's own output can't split it.
emit() {
	{ printf '%s' "$1" >"/dev/$tty"; } 2>/dev/null
}

# Record `$1` holding the first non-blank line of the hook input's `$2`, without control
# characters or markdown emphasis.
record() {
	command -v jq >/dev/null 2>&1 || return 0
	b64=$(printf '%s' "$input" | jq -r --arg k "$2" '
		(.[$k] // "") | split("\n")
		| map(gsub("[\\x00-\\x1f\\x7f\\x{80}-\\x{9f}]"; " ") | gsub("\\*\\*|__|`"; "")
			| gsub("\\s+"; " ") | sub("^[#> -]+"; "") | sub(" $"; ""))
		| map(select(length > 0)) | (first // "") | .[:200]
		| select(length > 0) | @base64' 2>/dev/null)
	[ -n "$b64" ] || return 0
	emit "${esc}]7501;id=$1:state=idle:msg=${b64}${esc}\\"
}

case $(field hook_event_name) in
SessionStart)
	[ "$(field source)" = compact ] || emit "${esc}]7501;id=warden:state=clear${esc}\\"
	id=$(field session_id)
	[ -n "$id" ] || exit 0
	b64=$(printf 'claude --resume %s' "$id" | base64 | tr -d '\n')
	apc="${esc}_warden1;resume=${b64}${esc}\\"
	emit "$apc"
	# Again ~2s later: warden types the agent when it creates the window and attaches the relay
	# after, so a first write that lands before the relay streams reaches only Rex's screen
	# state, which the relay's snapshot doesn't carry. A repeat is a no-op for warden. Detached
	# from every pipe Claude Code holds, so the hook returns now.
	(
		sleep 2
		printf '%s' "$apc" >"/dev/$tty"
	) </dev/null >/dev/null 2>&1 &
	# A resumed conversation (a woken agent: a new block, with no records) takes its rows from
	# its transcript: the last prompt typed, and the last reply's text.
	path=$(field transcript_path)
	if [ "$(field source)" = resume ] && [ -r "$path" ] && command -v jq >/dev/null 2>&1; then
		input=$(jq -cn 'reduce inputs as $l ({};
			($l.message.content // null) as $c
			| if $l.type == "user" and ($l.isMeta | not) and ($c | type) == "string"
				and ($c | startswith("<") | not) then .prompt = $c
			elif $l.type == "assistant" and ($c | type) == "array" then
				([$c[] | select(.type == "text") | .text] | join("\n")) as $t
				| if $t == "" then . else .last_assistant_message = $t end
			else . end)' "$path" 2>/dev/null) || exit 0
		record warden/asked prompt
		record warden/said last_assistant_message
	fi
	;;
UserPromptSubmit) record warden/asked prompt ;;
Stop) record warden/said last_assistant_message ;;
esac
exit 0
