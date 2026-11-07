# One Perennos Tempo folder per machine

Every Perennos Tempo program on a machine now keeps its files in one folder, the Tempo home. The launcher, the agent, Room, the Field, the command line and the workflows all find it the same way. Before this, the launcher, Room and the agent's packs used `~/.tempo`, while the command line, the Field and the workflows used a second folder. They only met if you set `TEMPO_HOME`.

Nothing moves by itself. If a machine already has files in an older place, they stay there until you merge them (below).

## Where it is

Each program picks the first of these that applies:

| Step | What | Windows | Linux and macOS |
| --- | --- | --- | --- |
| 1 | You said so: the `TEMPO_HOME` variable, if it is set and not empty. `PERENNOS_TEMPO_DATA` is the older name for the same thing and is checked second. | your folder | your folder |
| 2 | An older Tempo folder, but only if it already exists | the folder named by `TEMPO_LEGACY_HOME`, if set | `TEMPO_LEGACY_HOME` if set, else `$XDG_STATE_HOME/perennos-tempo`, or `~/.local/state/perennos-tempo` |
| 3 | The default | `%LOCALAPPDATA%\Perennos Tempo` | `~/.tempo` |

Why each step is there:

1. **You said so.** If you pin a folder, it always wins, on every program, so you can put it on another drive or share it between accounts.
2. **An older folder.** If an earlier Perennos Tempo kept its data in a folder of its own, point `TEMPO_LEGACY_HOME` at it (receipts and `engines.json` live there). On Linux and macOS an earlier version kept it in the state folder. Using those when they exist means nobody loses sight of their data on the day they update. The check is "does the folder exist"; an empty folder counts, so make sure an old folder you no longer want is gone (move it to the trash) before you update.
3. **The default.** A new machine gets one clean place, in the usual spot for the operating system.

## What lives in it

| Folder or file | What it is |
| --- | --- |
| `agent/` | the agent's conversations and memory (unless `TEMPO_AGENT_DATA` is set). Also `agent/control.key`, the agent's key (below). |
| `room/` | Tempo Room's data (unless `TEMPO_ROOM_DATA` or `--data` is set) |
| `connectors/`, `packs/` | the agent's connectors and packs |
| `workflows/`, `fleet/` | workflow templates and runs; the fleet snapshot and `machines.json` |
| `logs/`, `run/` | what each app wrote, and the records `tempo-launch stop` uses to end only what it started |
| `trash/` | anything Tempo moves aside (it never deletes), in `trash/<date>/` |
| `remote.json` | the apps on other machines that `tempo-launch` can open |
| `engines.json`, `connections.json` | your engines and your connections |

Every environment override that existed still works and still wins for its own folder: `TEMPO_AGENT_DATA`, `TEMPO_ROOM_DATA`, `--data`, `TEMPO_DATA`, `TEMPO_AGENT_RUNS` and the rest.

## See which one a machine uses

```
tempo-launch home
tempo-launch home --json
```

It prints the folder, which step chose it, and any other Tempo folder that exists and holds files (from the places in the table). If it lists one, that is data a program from before this change may have written.

The launcher also tells every app it starts: `TEMPO_HOME` is set to its own home, so they cannot disagree.

## Merge two folders

Say `tempo-launch home` shows your home and, below it, an older folder with files in it. To bring those files over:

```
tempo-launch stop
tempo-launch home --merge "C:\path\to\the\older\folder"
```

What it does:

- A file that is **not** in the home yet is moved in, in the same sub-folder it came from.
- A file that **is** already in the home is **never overwritten**. It stays where it was and is listed as skipped, so you can compare the two and decide.
- If a file cannot simply be moved (the older folder is on another drive), it is copied, and the original goes to `<home>/trash/<date>/merged-from-<name>/`. If even that is not possible, the original stays and is listed ("left where it was"). Nothing is deleted.
- When nothing is left in the older folder except empty folders, the folder itself moves to `<home>/trash/<date>/emptied-<name>`. If anything was skipped or left, the folder is kept so you can look at it.
- At the end it prints what moved and what was skipped because it already exists.

Run it while the apps are stopped, so nothing writes a file halfway. Links in the older folder are never followed or moved; they are listed as skipped.

## Pin one with `TEMPO_HOME`

Set `TEMPO_HOME` once and every program uses it, whatever is on the disk.

- Windows, for your user: `setx TEMPO_HOME "D:\Tempo"`, then sign out and in.
- Linux, in `~/.profile`: `export TEMPO_HOME="$HOME/.tempo"`
- macOS, in `~/.zprofile`: the same line.

Use this when a machine has two old folders and you want neither to win by accident, or when you want to point several accounts at one folder. After setting it, `tempo-launch home` should say `TEMPO_HOME is set`.

## The Windows installer

The installer works out the same folder with the same three steps and sets `TEMPO_AGENT_DATA` to `<home>\agent` for you, so the agent and `tempo-launch` agree even on a machine with the older folder. It only sets it if you have not set `TEMPO_AGENT_DATA` yourself. An older build set it to `%LOCALAPPDATA%\Perennos Tempo\agent`; that value is kept, so nothing changes under you. If you want the agent's files in the home, delete that variable and run the installer again, or merge as above.

## The agent key

The agent writes a fresh key to `<agent data>/control.key` every time it starts. Programs on the same machine read it from there. For an agent on another machine, or reached through a tunnel, set `TEMPO_AGENT_KEY` in the environment of the program that talks to it; that wins over the file. `tempo-launch agent` opens the agent's page as `http://127.0.0.1:<port>/#key=<key>`, so the page signs itself in; the key is in the part of the address after the `#`, which the browser never sends anywhere. Tempo Room sends the key on every agent call and reads the file again once if the agent answers 401, because the agent may have restarted. The key is never written to a log or shown.
