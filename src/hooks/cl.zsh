# cl shell integration for zsh. Load it with: eval "$(cl init zsh)"
#
# Inside `cl shell`, these hooks print invisible markers around each
# command so cl can tell which output belongs to it. Outside, they do nothing.

_cl_mark() {
  printf '\e]7777;%s;%s\a' "$CL_TOKEN" "$1"
}

_cl_preexec() {
  [[ -n ${CL_TOKEN-} ]] || return
  # Commands that start with a space are not recorded.
  if [[ $1 == [[:space:]]* ]]; then
    _cl_mark S
    return
  fi
  local c=${1//$'\a'/}
  _cl_mark "C;${c//$'\e'/}"
}

_cl_precmd() {
  local s=$?
  if [[ -n ${CL_TOKEN-} ]]; then
    _cl_mark "D;$s"
    # Stay first, so the next prompt still sees the command's exit status.
    precmd_functions=(_cl_precmd ${precmd_functions:#_cl_precmd})
  fi
  return $s
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _cl_preexec
(( ${+precmd_functions} )) || precmd_functions=()
precmd_functions=(_cl_precmd ${precmd_functions:#_cl_precmd})
if [[ -n ${CL_TOKEN-} ]]; then _cl_mark H; fi
