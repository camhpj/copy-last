# cl

`cl` copies your last terminal commands and their output to the clipboard.
Paste them into a bug report, a chat, or an AI assistant, with no text to select by hand.

```console
$ ls missing
ls: missing: No such file or directory
$ cl
cl: copied 1 command (3 lines) with pbcopy
```

The clipboard now holds the command, its output, and its exit status:

```text
$ ls missing
ls: missing: No such file or directory
[exit 1]
```

> [!IMPORTANT]
> `cl` records all output of the commands that you run in a session, secrets included.
> Only you can read the file that holds it. `cl` deletes the file when the session ends.
> To keep a command out of the record, start it with a space.

## Requirements

- macOS or Linux.
- zsh, or bash 4.4 or later. macOS includes bash 3.2, which is too old. Run `brew install bash` to get a newer bash.
- Rust 1.85 or later, only to build `cl` from source.

## Install

```sh
curl -fsSL https://github.com/camhpj/copy-last/releases/latest/download/install.sh | sh
```

The script downloads the build for your system from the latest release, checks its SHA-256 checksum, and installs `cl` in `~/.local/bin`.
To install somewhere else, set `CL_INSTALL_DIR`. To install a specific release, set `CL_VERSION`, for example `CL_VERSION=v0.1.0`.

To build from source instead, use Cargo:

```sh
cargo install --git https://github.com/camhpj/copy-last
```

## Setup

1. Add the hook to your rc file. For zsh, add this line to `~/.zshrc`:

   ```sh
   eval "$(cl init zsh)"
   ```

   For bash, add this line to `~/.bashrc`:

   ```sh
   eval "$(cl init bash)"
   ```

2. Open a new terminal and run `cl shell`. This starts a session: `cl` records the commands that you run until you exit the shell.

To start a session in every terminal, add this line after the `eval` line:

```sh
[[ -z ${CL_SESSION+x} && -t 0 ]] && command -v cl >/dev/null && exec cl shell
```

If `cl shell` cannot start a session, it prints the reason and runs your shell without recording.

## Usage

```sh
cl                  # the last command and its output
cl -n 3             # the last 3 commands
cl -n 2 -s 1        # 2 commands, skipping the most recent one
cl -c -n 5          # only the command lines
cl -o               # only the output
cl -p               # print instead of copying
cl --osc52          # copy with the OSC 52 escape sequence instead of a clipboard tool
cl list             # recent commands, with the number to pass to -s
```

`cl` does not record its own runs. So `cl` and `cl list` do not change what `-s` counts.

## Clipboard

`cl` tries these clipboard tools in order: `pbcopy`, `wl-copy`, `xclip`, `xsel`.
If none of them works, `cl` sends the text to your terminal with the OSC 52 escape sequence.
Over SSH, `cl` uses OSC 52 first. A clipboard tool would copy to the clipboard of the remote machine.

Your terminal must support OSC 52 for this to work.
Under tmux, also add `set -g set-clipboard on` to your tmux configuration.
Some terminals ignore OSC 52 text above 100 KB. `cl` warns you when a copy is that large.

## How it works

A terminal does not record which output belongs to which command.
`cl shell` runs your shell inside a pseudo-terminal and reads everything the shell prints.
The hook prints an invisible marker before and after each command, and `cl shell` removes the markers from the screen.
Each marker carries a random token for the session. Text that a program prints does not contain the token, so a file or a web page cannot add fake entries.

`cl` keeps the last 100 commands of each session, and the last 1 MiB of output for each command.
It does not capture the output of full-screen programs such as `vim` or `less`.
`cl` stores the history in `~/.local/state/cl/` (or `$XDG_STATE_HOME/cl/`), in a file with mode 0600.
If a session ends without cleanup, for example after a crash, the next `cl shell` deletes its file.

## Troubleshooting

### ``cl: not inside a cl session. Run `cl shell` first.``

You ran `cl` in a terminal with no session. Run `cl shell` first.

### `cl: this shell does not report commands to cl`

The hook is not loaded in this shell. Make sure that your rc file has the `eval "$(cl init …)"` line. Then run `cl shell` again.
With bash, also make sure that `bash --version` shows 4.4 or later.

### ``cl: `cl shell` could not start recording in this terminal. Run `cl shell` to see why.``

`cl shell` failed before your shell started. It printed the reason when the terminal opened.
Run `cl shell` again to see the error.

### `cl: that command was not recorded`

The command started with a space, so `cl` did not record it.
`cl list` shows these commands as `[not recorded]`.

In bash, `cl` reads each command line from the bash history.
If bash keeps a command out of its history, `cl` usually cannot know what the command was. It then does not record the command.
This happens in these cases:

- `HISTIGNORE` is set, and bash ignores the command.
- `HISTCONTROL` contains `ignorespace` or `ignoreboth`, and bash ignores a repeated command because of `ignoredups` or `erasedups`.
- History is off (`set +o history`).
- A command in `PROMPT_COMMAND` changed the history list in an unusual way, for example `history -c`.

With only `ignoredups` or `erasedups` in `HISTCONTROL`, `cl` records repeated commands as usual.

### Nothing was copied after `cl: sent 1 command (3 lines) with OSC 52`

Your terminal does not accept OSC 52, or tmux blocks it. See [Clipboard](#clipboard). Use `cl -p` to print the text instead.

### Multi-line commands show on one line in bash

bash joins the lines of a multi-line command with `;` in its history, and `cl` shows the command as bash stores it.
To keep the line breaks, run `shopt -s lithist`.
If you turn off `cmdhist` (`shopt -u cmdhist`), bash stores each line separately, and `cl` does not record multi-line commands.

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT), at your option.
