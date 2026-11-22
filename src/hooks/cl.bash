# cl shell integration for bash 4.4 or later. Load it with: eval "$(cl init bash)"
#
# Inside `cl shell`, these hooks print invisible markers around each
# command so cl can tell which output belongs to it. Outside, they do nothing.
#
# bash has no preexec hook, so the command line comes from the history list,
# read from PS0 just before the command runs. When bash keeps a command out
# of the history list, cl can't know what it was, so it marks the command
# as not recorded and keeps none of its output.

if ((BASH_VERSINFO[0] * 100 + BASH_VERSINFO[1] < 404)); then
  if [[ -n ${CL_TOKEN-} ]]; then
    printf 'cl: bash %s cannot record commands. cl needs bash 4.4 or later.\n' "$BASH_VERSION" >&2
    if [[ $OSTYPE == darwin* ]]; then
      printf 'cl: on macOS, run: brew install bash\n' >&2
    fi
  fi
else

_cl_mark() {
  printf '\e]7777;%s;%s\a' "$CL_TOKEN" "$1"
}

# Split the newest history entry ("%5d%c %s") into _cl_num and _cl_line.
_cl_newest() {
  local l
  l=$(HISTTIMEFORMAT='' builtin history 1)
  l=${l#"${l%%[0-9]*}"} # leading spaces
  _cl_num=${l%%[!0-9]*}
  l=${l#"$_cl_num"}
  _cl_line=${l:2} # "  ", or "* " for an edited entry
}

# Runs from PS0, in a subshell, just before each command.
_cl_preexec() {
  [[ -n ${CL_TOKEN-} ]] || return
  _cl_newest
  if [[ ! -o history || -z $_cl_num || -z ${_cl_prev_num-} ]]; then
    _cl_mark S
    return
  fi
  if [[ $_cl_num != "$_cl_prev_num" && $_cl_num != $((_cl_prev_num + 1)) ]]; then
    # The history list changed in other ways too (history -n, history -c),
    # so the newest entry may not be this command.
    _cl_mark S
    return
  fi
  if [[ $_cl_num == "$_cl_prev_num" ]]; then
    # bash kept the command out of history. With only ignoredups or
    # erasedups, it repeats the newest entry. With ignorespace or
    # HISTIGNORE, it could be anything, so don't record it.
    local hc=":${HISTCONTROL-}:"
    if [[ $hc == *:ignorespace:* || $hc == *:ignoreboth:* || -n ${HISTIGNORE-} ]]; then
      _cl_mark S
      return
    fi
  fi
  # Commands that start with a space are not recorded.
  if [[ $_cl_line == [[:space:]]* ]]; then
    _cl_mark S
    return
  fi
  local c=${_cl_line//$'\a'/}
  _cl_mark "C;${c//$'\e'/}"
}

# First in PROMPT_COMMAND, so it sees the command's exit status.
_cl_precmd() {
  local s=$?
  if [[ -n ${CL_TOKEN-} ]]; then
    _cl_mark "D;$s"
    _cl_order
  fi
  return $s
}

# Last in PROMPT_COMMAND, after anything that edits the history list (for
# example `history -a; history -n`): note the newest entry's number.
_cl_snap() {
  local s=$?
  _cl_newest
  _cl_prev_num=${_cl_num:-0} # 0 when the history is empty
  return $s
}

# Put _cl_precmd first and _cl_snap last in PROMPT_COMMAND, once each.
# Other tools may have added commands around them since the last prompt.
_cl_order() {
  local nl=$'\n' pc e
  local -a rest=()
  if [[ ${PROMPT_COMMAND@a} == *a* ]]; then
    for e in "${PROMPT_COMMAND[@]:1}"; do
      [[ $e == _cl_snap ]] || rest+=("$e")
    done
  fi
  pc=${PROMPT_COMMAND-}
  pc=${pc//_cl_precmd$nl/}
  pc=${pc//_cl_precmd;/}
  pc=${pc//_cl_precmd/}
  pc=${pc//$nl'_cl_snap'/}
  pc=${pc//;_cl_snap/}
  pc=${pc//_cl_snap/}
  # Drop separators left at either end, as in ";history -a".
  while [[ $pc == [$nl\;\ ]* ]]; do pc=${pc:1}; done
  while [[ $pc == *[$nl\;\ ] ]]; do pc=${pc%?}; done
  if ((${#rest[@]})); then
    PROMPT_COMMAND=("_cl_precmd${pc:+$nl$pc}" "${rest[@]}" _cl_snap)
  else
    # shellcheck disable=SC2178 # a string unless the user made it an array
    PROMPT_COMMAND="_cl_precmd$nl${pc:+$pc$nl}_cl_snap"
  fi
}

# shellcheck disable=SC2016 # bash expands PS0 before each command
[[ ${PS0-} == *_cl_preexec* ]] || PS0='$(_cl_preexec)'"${PS0-}"
_cl_order
if [[ -n ${CL_TOKEN-} ]]; then _cl_mark H; fi

fi
