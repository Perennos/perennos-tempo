# Room rehearsal

A full run of Perennos Tempo Room before a release. It uses the real `tempo-room` and
`tempo-local-agent` programs, a mock Ollama, an owner and three guests (Mia, Sam and Lee) in two
rooms, plus two teammates: Sage (an engine, model `mock:4b`) and Coder (an agent). The unit tests check each piece alone. This checks that they work together, the way people
will use them.

## Run it (Linux or macOS)

From the repository root (each crate builds into its own `target/`):

```bash
cargo build --release --manifest-path perennos-tempo-room/Cargo.toml
cargo build --release --manifest-path perennos-tempo-local-agent/Cargo.toml
mkdir -p /tmp/rehearsal-bin /tmp/rehearsal-run
cp perennos-tempo-room/target/release/tempo-room perennos-tempo-local-agent/target/release/tempo-local-agent /tmp/rehearsal-bin/
python3 -I perennos-tempo-room/rehearsal/rehearse.py /tmp/rehearsal-bin /tmp/rehearsal-run
```

- It needs Python 3 (standard library only) and the free ports 18970 to 18972 on 127.0.0.1.
- It takes about two minutes. One minute of that is waiting out a posting limit on purpose.
- It prints one line per check, then `N passed, M failed`. It also writes `results.json` and every
  program's log into the run folder.
- Use a new, empty run folder each time. Nothing outside it is written, and nothing is deleted.

## What it checks (87 checks)

- **Signing in:**
  - the one-time owner link works once;
  - each guest's one-time code works;
  - one name carries across two rooms;
  - members see only their own rooms.
- **Devices:**
  - a member cannot make a sign-in link; the owner can;
  - Mia's second device signs in through the link with no name, sees the same rooms, and the
    link works only once;
  - Mia signs out on device 1: device 1 becomes a guest (even with its old cookie), device 2 keeps
    working and her record stays.
- **Talking:**
  - posts reach everyone live;
  - `@tempo` answers in the thread through the real agent;
  - two rooms are answered at the same time;
  - "Tempo is thinking" shows;
  - `tldr` and `remember` work.
- **Guest safety:**
  - the agent offers the model no tools;
  - each request starts with the room line and carries the other-people rule;
  - a model that tries to run a command runs nothing;
  - hidden reasoning never reaches the room;
  - long answers are capped;
  - keys in answers and in pasted messages are redacted before they are stored or shown.
- **Teammates:**
  - a member cannot add a teammate, list the team or see the installed models;
  - the owner adds the engine teammate `sage` and the command line adds the agent teammate `coder`
    through the running server; `team list` shows both; a model Ollama does not have and the name
    `tempo` are refused; nobody can join as a teammate;
  - Mia asks `@sage` and gets the mock Ollama's answer under Sage's name; the engine call is a
    direct chat (`stream` false, no tools), its system message is the role plus the room rule and
    the request is the room line;
  - a guest's `@sage` request that makes the model try a tool runs nothing, every engine call
    carried no tools, and a key in an answer is redacted;
  - `@coder` is answered by the real agent; Tempo does not answer too; one message naming two
    teammates asks each;
  - a one-on-one room (`always`, exactly one teammate) answers every message with `@coder` and Tempo
    stays out of it;
  - removing a teammate moves its record to `trash/`; teammates survive a restart.
- **Owner powers:**
  - a request that came through a proxy is never the owner;
  - members cannot make rooms;
  - mute and unmute work;
  - posting too fast is limited;
  - Remove stops the person's cookie at once and their live stream within 5 seconds, and their
    record goes to `trash/`.
- **Restarts and outages:**
  - messages and sessions survive a room restart;
  - with the agent down, a ping says so and a question gets a polite note while the room keeps
    working;
  - when the agent comes back with a new key, the room reads the new key and answers again.

## What it does not cover

- Real people on real phones and browsers.
- Tailscale or another proxy in front.
- The Discord, Telegram and WhatsApp bridges.
- A real model. The mock answers by keyword: `THINKTEST`, `LONGTEST`, `KEYTEST`, `SLOWTEST`,
  `TOOLTEST`. It answers a call with `"stream": false` (an engine teammate) with one JSON reply and
  streams for the agent.
- The agent's teammate roles: the agent binary may be older and not take a role and model
  (`"roles": true` in its health); the run prints which. Both ways are covered by the unit tests
  with a fake agent.
- Ask limits: the run raises them in its own config so its many questions are not told to wait.
