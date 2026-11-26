# pi

tam runs [pi](https://pi.dev) as well as Claude Code: install it (`pi` on your `PATH`, logged in) and say which agent you want in the config.

```toml
# ~/.config/tam/config.toml
[agent]
program = "pi"
```

`tam new`, `tam run`, `tam chat` and the dashboard start pi from then on. A running dashboard picks the change up at the next start; the agents and chats already running keep going. A chat keeps the agent it was made with: its session, its name and its history are that agent's (see [Chats](chats.md)).

## The extension

pi has no hooks. tam writes an extension to `$XDG_DATA_HOME/tam/agents/pi.js` (by default `~/.local/share/tam/agents/pi.js`) and loads it at every start with `pi --extension <file>`: nothing is added to your pi settings or extensions, and the file is tam's (it is written again when it changes). It runs `tam hook-notify` for each of pi's events, which is how the daemon learns an agent's state, its session, its task and its peers, and it runs `tam inbox-wait` to hand it the messages waiting for it.

State comes from pi's own events, so it is exact while pi runs: `working` during a turn, `ready` at its prompt, `error` when a turn fails (an API error, say). A turn that ends without an event (an interrupt from the terminal) is covered the way Claude's are: a working agent silent for 10s is taken as back at the prompt.

## Environment

A pi agent runs in the environment of what started it, as a Claude agent does (see [the environment](claude-code.md#environment)), and the same things are left out: your pane and terminal size, and the calling session's own variables — a calling pi session's `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL` and `PI_REASONING_LEVEL`, so the new agent reports its own session to its tools. `PI_CODING_AGENT_DIR` and `PI_CODING_AGENT_SESSION_DIR` pass, as `CLAUDE_CONFIG_DIR` does. `[spawn] wrapper` applies to pi too.

## What differs from Claude Code

- **No permission requests, no plan mode, no question tool.** pi runs its tools with your user's permissions and asks nothing, so an agent never shows as blocked or asking, and the dashboard's `a` has nothing to answer: type into the agent, or `tam msg` it.
- **No extra directories, no edit deny.** Claude's `TaskAccess` (the task's directory and the peers' worktrees, read only) has no pi equivalent: the agent can already reach everything your user can, so the note it gets at its session start claims no more.
- **No context use.** pi's session file has the tokens but not the model's limit, so the CTX column stays empty.
- **No generated titles.** pi names no session of its own: a chat or a session shows the name you gave it (`/name`), else its id.
- **Sessions are found where pi keeps them.** `tam run` checks that a member's session still exists under `PI_CODING_AGENT_SESSION_DIR`, else `sessions` under `PI_CODING_AGENT_DIR` (by default `~/.pi/agent`), under the daemon's `HOME`: a `sessionDir` set in pi's settings is not looked at, and whatever tam cannot find is not resumed (as for Claude's transcripts).
- **One message, one process.** Each handover of messages is a `tam inbox-wait` process under pi (Claude's inbox hook is one too). It ends with pi, or when its daemon does.

Trust works as for Claude Code: pi may ask whether to trust a new worktree, and a detached agent waits on that question (`ready`, with no result). Attach once to answer it — or return here with `tam run` again after answering.

The same goes for a session pi cannot open where it runs: it then asks whether to fork it into the current directory. That only happens when the session's working directory is not the worktree's resolved path (a moved worktree, a symlinked worktree root such as `/tmp` on macOS), since tam runs every agent in its member's worktree.
