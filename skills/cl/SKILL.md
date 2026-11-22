---
name: cl
description: Read the commands the user ran in their terminal, and their output, with the `cl` (copy-last) CLI instead of asking them to paste it. Use this whenever the user refers to something they ran or saw in their terminal, such as "the error I just got", "my last command", "why did that build fail", "look at the test output", or "what did that print", even if they don't mention cl. Also use it to set up or troubleshoot cl itself (`cl shell`, `cl init`, "this shell does not report commands to cl").
allowed-tools: Bash(cl list *) Bash(cl -p *)
---

# cl: read the user's recent terminal commands

`cl` records each command that the user runs inside `cl shell`, together with its output and exit status. Claude Code inherits the session from the terminal it was started in, so you can read that history from your Bash tool. That beats asking the user to copy and paste: you see the exact command, the full output and the exit status.

## Reading the history

1. Run `cl list` to see the recent commands. The first column is the number to pass to `-s`. `0` is the most recent command.

   ```console
   $ cl list
     0  cargo test                                          exit 101  48 lines
     1  git pull                                            exit 0    3 lines
   ```

2. Print the entries you need with `-p`:

   ```sh
   cl -p              # the most recent command and its output
   cl -p -s 1         # the one before it
   cl -p -n 3         # the last 3 commands
   cl -p -o           # only the output
   ```

Always pass `-p`. Without it, `cl` copies to the user's clipboard and overwrites whatever they had there.

Pick the entry from `cl list` instead of assuming it is number 0. While you run, the user's terminal is busy with Claude Code itself, so the newest entry is the last command that finished before Claude Code started, or before the user returned to it. If an entry's command is `claude`, skip it. It holds the output of an earlier Claude Code session, not the error the user means.

## What the output looks like

```text
$ cargo test
[… earlier output truncated …]
error[E0308]: mismatched types
[exit 101]
```

- `[exit N]` appears when the command failed.
- `[… earlier output truncated …]` means that `cl` kept only the last 1 MiB of output.
- `[full-screen program: output not captured]` appears for programs such as `vim` or `less`.
- `[not recorded]` in `cl list` means that the user kept the command out of the record, for example by starting it with a space. Respect that. Do not try to find out what the command was.

## When cl is not available

| What you see | What it means | What to do |
|---|---|---|
| `cl: command not found` | cl is not installed. | Ask the user to paste the output. Mention cl only if they want to avoid pasting in future. |
| `cl: not inside a cl session` | Claude Code was not started inside `cl shell`. | Ask the user to paste the output. To use cl next time, they run `cl shell` and then start Claude Code in it. |
| `cl: this shell does not report commands to cl` | The shell hook is not in the rc file. | See the setup below. |
| The command is not in `cl list` | The user ran it in another terminal, or after Claude Code started. | Ask the user to paste it, or to run it in the session that Claude Code was started from. |

Commands that you run with your own Bash tool are not recorded. You already have their output.

## Setting up cl

If the user wants to set up cl:

1. Install it: `curl -fsSL https://github.com/camhpj/copy-last/releases/latest/download/install.sh | sh`
2. Add the hook to `~/.zshrc` (`eval "$(cl init zsh)"`) or to `~/.bashrc` (`eval "$(cl init bash)"`). bash must be 4.4 or later. macOS includes 3.2, so `brew install bash` is needed there.
3. Open a new terminal, run `cl shell`, and start Claude Code inside it.

Ask before you edit the user's rc file. For more troubleshooting, see https://github.com/camhpj/copy-last#troubleshooting.

## Privacy

The history holds everything that the commands printed, which can include tokens, passwords or customer data. Read only the entries that the task needs, and do not quote secrets back to the user or put them in files.
