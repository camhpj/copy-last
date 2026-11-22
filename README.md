# cl

Copy your last N terminal commands and their output to the clipboard.

## Install

    cargo install --path .

## Setup

Add the hook to your rc file (`~/.zshrc` or `~/.bashrc`):

    eval "$(cl init zsh)"     # or: cl init bash  (bash >= 4.4)

Then run `cl shell` to start a recording session. To always record,
add this *after* the eval line (test `cl shell` by hand first):

    [[ -z $CL_SESSION && -t 0 ]] && command -v cl >/dev/null && exec cl shell

## Usage

    cl                  # last command + output
    cl -n 3             # last 3
    cl -n 2 -o 1        # 2 commands, skipping the most recent one
    cl -c -n 5          # just the command lines
    cl -O               # just the output
    cl -p               # print instead of copy
    cl --osc52          # copy via terminal escape (SSH, no clipboard tool)
    cl list             # recent commands with their offsets

Clipboard: pbcopy, wl-copy, xclip or xsel, falling back to OSC 52.
Under tmux, OSC 52 needs `set -g set-clipboard on`.

History lives in `~/.local/state/cl/<pid>.jsonl` (mode 0600, last 100
commands, 1 MiB tail per command) and is deleted when the session exits.
