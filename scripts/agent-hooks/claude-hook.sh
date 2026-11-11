#!/bin/sh
# Claude Code hook (SessionStart, UserPromptSubmit, Stop): tells warden how to resume this agent
# and what the summary block shows for it. Does nothing outside a Rex terminal (`$REX_BLOCK`).
#
# - SessionStart writes the private APC
#     ESC _ warden1;resume=<base64 of "claude --resume <session_id>"> ESC \
#   which rides Rex's raw data stream to warden's relay, which strips it. Unless the session is
#   only compacting, it also clears the `warden` OSC 7501 records below: a new conversation
#   starts with empty rows, and a resumed one refills them from its transcript.
# - UserPromptSubmit and Stop write OSC 7501 child records of at most 200 characters. Rex keeps
#   each record on the block, and warden reads it from Rex. A prompt, whole, is `warden/asked`,
#   and clears `warden/answered`; a short one (at most three words, 20 characters: "b",
#   "yes, go") is `warden/answered` instead, beside the ask it answers; one that opens with `<`
#   is Claude Code's own (a background task's `<task-notification>`, a command's output) and
#   writes nothing. Either clears `warden/needs`. The reply's first line is `warden/said`, until
#   the turn's digest (`digest`, also run on a resume) rewrites it with `warden/topic`,
#   `warden/asked` and `warden/needs`. Stop also writes `warden/background`, what is still
#   running as the turn ends. These need `jq`; without it they do nothing.
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

# Record `$1` holding the text on stdin, without control characters or markdown emphasis, at
# most 200 characters: its first non-blank line, or with `$2` = `all`, every line joined. Empty
# text clears the record.
put() {
	b64=$(jq -Rsr --arg all "${2:-}" '
		split("\n")
		| map(gsub("[\\x00-\\x1f\\x7f\\x{80}-\\x{9f}]"; " ") | gsub("\\*\\*|__|`"; "")
			| gsub("\\s+"; " ") | sub("^[#> -]+"; "") | sub(" $"; ""))
		| map(select(length > 0))
		| (if $all == "all" then join(" ") else (first // "") end) | .[:200]
		| @base64' 2>/dev/null)
	if [ -n "$b64" ]; then
		emit "${esc}]7501;id=$1:state=idle:msg=${b64}${esc}\\"
	else
		emit "${esc}]7501;id=$1:state=clear${esc}\\"
	fi
}

# Record `$1` holding the hook input's `$2`, as `put` takes it with `$3`; nothing when that is
# empty.
record() {
	command -v jq >/dev/null 2>&1 || return 0
	text=$(printf '%s' "$input" | jq -r --arg k "$2" '.[$k] // ""' 2>/dev/null)
	[ -n "$text" ] || return 0
	printf '%s' "$text" | put "$1" "${3:-}"
}

# The digest of the turn just ended: a background Haiku call over the transcript's last turns
# (and, at Stop, the reply that may not be in the file yet) that rewrites `warden/topic`,
# `warden/asked` (a short reply read against the question it answers, so `warden/answered`
# clears), `warden/said` and `warden/needs`. A prompt sent before it lands replaces its token,
# and the stale digest is dropped. Needs `claude` and `jq`; `WARDEN_DIGEST=0` turns it off.
DIGEST_PROMPT='You write the status lines a terminal multiplexer shows under a coding agent'"'"'s pane, for a user glancing across many agents. From the conversation excerpt, fill:
topic: what the whole conversation is working on, 2 to 6 words, no trailing period.
asked: what the user'"'"'s last message asks for, at most 90 characters, in their terms. When it is a short reply (a letter, "yes", "do it"), it answers the question in the Agent message just before it, never one in a later message: say what that choice meant.
said: what the agent did or found in its last reply, at most 110 characters, outcome first.
needs: the decision or input the agent is waiting on from the user, at most 90 characters, naming the options it offered; an empty string when it needs nothing.
Plain text, no markdown, no quotes around values.'
DIGEST_SCHEMA='{"type":"object","properties":{"topic":{"type":"string"},"asked":{"type":"string"},"said":{"type":"string"},"needs":{"type":"string"}},"required":["topic","asked","said","needs"]}'
digest_dir=${TMPDIR:-/tmp}/warden-digest

digest() {
	[ "${WARDEN_DIGEST:-1}" != 0 ] || return 0
	command -v jq >/dev/null 2>&1 && command -v claude >/dev/null 2>&1 || return 0
	sid=$(field session_id)
	path=$(field transcript_path)
	[ -n "$sid" ] && [ -r "$path" ] || return 0
	mkdir -p "$digest_dir" 2>/dev/null || return 0
	token="$$.$(date +%s)"
	printf '%s' "$token" >"$digest_dir/$sid" 2>/dev/null || return 0
	last=$(printf '%s' "$input" | jq -r '.last_assistant_message // ""' 2>/dev/null)
	excerpt=$(jq -rn --arg last "$last" "$KIND"' [inputs
			| (.message.content // null) as $c
			| if .type == "user" and (.isMeta | not) and ($c | type) == "string"
				and ($c | kind) != "skip" then {who: "User", text: $c}
			elif .type == "assistant" and ($c | type) == "array" then
				([$c[] | select(.type == "text") | .text] | join("\n")) as $t
				| if $t == "" then empty else {who: "Agent", text: $t} end
			else empty end] as $m
		| ([$m[] | select(.who == "User")] | first) as $first
		| (if $last == "" then $m
			else ($m | (map(.who) | rindex("User")) as $u
				| (if $u == null then [] else .[:$u + 1] end) + [{who: "Agent", text: $last}])
			end)
		| .[-8:]
		| (if $first then "The conversation opened with: \($first.text[:400])\n\n" else "" end)
			+ (map("\(.who): \(.text | if length > 1500 then .[:700] + " … " + .[-700:] else . end)")
				| join("\n\n"))' "$path" 2>/dev/null)
	[ -n "$excerpt" ] || return 0
	(
		cd / || exit 0
		out=$(printf '%s' "$excerpt" | env -u REX_BLOCK CLAUDE_CODE_DISABLE_CLAUDE_MDS=1 \
			claude -p --model haiku --no-session-persistence --tools "" --strict-mcp-config \
			--setting-sources "" --settings '{"disableAllHooks":true}' \
			--system-prompt "$DIGEST_PROMPT" --output-format json --json-schema "$DIGEST_SCHEMA" \
			2>/dev/null) || exit 0
		[ "$(cat "$digest_dir/$sid" 2>/dev/null)" = "$token" ] || exit 0
		d=$(printf '%s' "$out" | jq -c '.structured_output // empty' 2>/dev/null)
		[ -n "$d" ] || exit 0
		for k in topic asked said needs; do
			printf '%s' "$d" | jq -r --arg k "$k" '.[$k] // ""' | put "warden/$k"
		done
		emit "${esc}]7501;id=warden/answered:state=clear${esc}\\"
	) </dev/null >/dev/null 2>&1 &
}

# Record `warden/background`: the hook input's `background_tasks` (what is still running as the
# turn ends) counted by kind, "2 shells · 1 agent"; none clears it.
background() {
	command -v jq >/dev/null 2>&1 || return 0
	printf '%s' "$input" | jq -r '[.background_tasks[]? | .type
		| {shell: "shell", subagent: "agent", monitor: "monitor", workflow: "workflow",
			teammate: "teammate"}[.] // "task"]
		| group_by(.) | map("\(length) \(.[0])\(if length > 1 then "s" else "" end)")
		| join(" · ")' 2>/dev/null | put warden/background
}

# What a prompt is: `skip` (Claude Code's own, or blank), `answer` (short) or `ask`.
KIND='def kind: (gsub("\\s+"; " ") | ltrimstr(" ") | rtrimstr(" ")) as $p
	| if $p == "" or ($p | startswith("<")) then "skip"
	elif ($p | length) <= 20 and ($p | split(" ") | length) <= 3 then "answer"
	else "ask" end;'

# Record the hook input's `prompt` as an ask or an answer, as `kind` sorts it.
prompt() {
	command -v jq >/dev/null 2>&1 || return 0
	kind=$(printf '%s' "$input" | jq -r "$KIND"' .prompt // "" | kind' 2>/dev/null)
	[ "$kind" = skip ] && return 0
	# Whatever the agent waited on is answered, and a digest still running is stale.
	rm -f "$digest_dir/$(field session_id)" 2>/dev/null
	emit "${esc}]7501;id=warden/needs:state=clear${esc}\\"
	case $kind in
	ask)
		record warden/asked prompt all
		emit "${esc}]7501;id=warden/answered:state=clear${esc}\\"
		;;
	answer) record warden/answered prompt ;;
	esac
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
	# its transcript: the last ask, any answer after it, and the last reply's text.
	path=$(field transcript_path)
	if [ "$(field source)" = resume ] && [ -r "$path" ] && command -v jq >/dev/null 2>&1; then
		hook_input=$input
		input=$(jq -cn "$KIND"' reduce inputs as $l ({};
			($l.message.content // null) as $c
			| if $l.type == "user" and ($l.isMeta | not) and ($c | type) == "string" then
				($c | kind) as $k
				| if $k == "ask" then .prompt = $c | del(.answer)
				elif $k == "answer" then .answer = $c
				else . end
			elif $l.type == "assistant" and ($c | type) == "array" then
				([$c[] | select(.type == "text") | .text] | join("\n")) as $t
				| if $t == "" then . else .last_assistant_message = $t end
			else . end)' "$path" 2>/dev/null) || exit 0
		record warden/asked prompt all
		record warden/answered answer
		record warden/said last_assistant_message
		input=$hook_input
		digest
	fi
	;;
UserPromptSubmit) prompt ;;
Stop)
	record warden/said last_assistant_message
	background
	digest
	;;
esac
exit 0
