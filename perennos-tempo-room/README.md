# Perennos Tempo Room

A private group chat for Perennos Tempo. People talk; anyone can write `@tempo` and Perennos
Tempo answers in the thread, the way people use @grok on X. Rooms for the family, the team, a
book club. You can also set up **teammates**: named AI members you talk to like colleagues (see
"Your team"). Runs on your machine next to the agent; Discord, Telegram and WhatsApp can be
bridged in, so the same Tempo answers there too.

One binary, `tempo-room`. The HTTP server and the app are std only (no build step). `ureq` (with
rustls) makes the outgoing calls: the agent on loopback and the bridges' HTTPS; `ring`, already
under rustls, gives secure random tokens, SHA-256 and the webhook signature check.

## Start in three commands

```bash
cargo build --release --manifest-path perennos-tempo-room/Cargo.toml
tempo-room serve          # prints the owner link once: open it on this machine
tempo-room invite family  # makes #family and prints a one-time link and code
```

`serve` listens on `http://127.0.0.1:8767/` and talks to the Perennos Tempo agent at
`http://127.0.0.1:8765` (start it with `tempo-local-agent serve`). If the agent is not running
the room still works; Tempo just says, politely, that it cannot answer yet.

The data folder is `--data DIR`, else `TEMPO_ROOM_DATA`, else `<Tempo home>/room`: the one Tempo home per machine (`TEMPO_HOME`, else an older Tempo folder if it exists, else `~/.tempo`; see `docs/TEMPO-HOME.md`).

## Rooms and invites

- **You are the owner** when you are at the machine: a loopback connection (with no proxy in
  front) carrying an owner session cookie. `serve` prints a one-time owner link at the first
  start; `tempo-room owner-link` makes a new one (the older link stops working). Opening a link
  signs you in once and trades it for a random session cookie (30 days; Sign out ends it on the
  server); the link itself is never the cookie, and only its hash is kept. The owner makes rooms,
  invites, mutes, removes and sets Tempo's tone.
- **Invites work once and expire** (48 hours unless `--hours N` or `invite_hours`). The code is
  the key to the room: the **Share this room** sheet in the app and `tempo-room invite ROOM` give
  a link and a code you can read out loud
  (`XXXXX-XXXXX`; O and 0, I and 1 are the same). Codes are kept only as hashes.
- **Joining** asks for a name and gives the device a session cookie (HttpOnly, SameSite=Strict).
  The same person invited to a second room keeps their name and gets the room added.
- **One person, several devices.** Each device has its own session, so Mia can use Room on her
  phone and her laptop at once. To add a device, the owner opens People, taps **New sign-in link**
  beside her name and sends her the link or reads out the code (the same `XXXXX-XXXXX` format,
  once only, and it expires like an invite: 48 hours unless `invite_hours`). A program can ask for
  the same with `POST /room/invite {"member": "<id>"}` (owner only). Opening it signs the new
  device in as Mia: it never asks for a name, never makes a new person and never changes her
  rooms. A sign-in link made for Mia can only sign in Mia, and it stops working if she is removed.
  Failed tries count against the same join limit as invite codes.
- **Sign out** by a member ends that device's session on the server (not only the cookie in the
  browser); her other devices keep working and her record stays. Say so plainly: it signs out
  this device only. The owner's sign-out works as before.
- **Sessions expire when idle.** A device not used for 180 days (`session_days` in `config.json`)
  stops working and needs a new sign-in link. Use keeps it alive; the time of last use is saved
  at most once an hour per device, so ordinary use does not keep rewriting the file.
- **Mute** (`/mute @Mia`, or People in the app) stops someone posting in that room; **Remove** takes
  them out of every room. Their record moves to `trash/` (nothing is deleted), every session of
  theirs stops working at once, their unused sign-in links stop working, and an open live stream
  ends within five seconds.

### For family or a team: Tailscale (or the home network)

The room only listens on this machine until you say otherwise. Three ways, easiest first:

1. **`tailscale serve` (Plan A, HTTPS, no firewall prompt on Windows).** Keep `serve` on loopback
   and let Tailscale put HTTPS in front:
   ```bash
   tailscale serve --bg 8767
   tempo-room serve --public-url https://laptop.<your-tailnet>.ts.net
   ```
   Invite links then use that address. Everyone comes in through loopback this way, which is
   why owner powers also need the owner session cookie, and why a request that carries a proxy's
   headers (`X-Forwarded-For` and the like) is never the owner: a family member cannot act as the
   owner.
2. **Bind the tailnet address (Plan B).** `tailscale ip -4` gives `100.x.y.z`; then
   `tempo-room serve --bind 100.x.y.z --allow-host laptop`. The owner still uses
   `http://127.0.0.1:8767` on the machine itself (a second, loopback-only listener).
   Windows may ask once to allow `tempo-room.exe` through its firewall; to avoid the prompt, add
   the rule yourself first (PowerShell as administrator):
   `New-NetFirewallRule -DisplayName "Perennos Tempo Room" -Direction Inbound -Program "<path>\tempo-room.exe" -Action Allow -Profile Private`
3. **The home network (Plan C).** `--bind 192.168.0.20` (or `0.0.0.0 --allow-host <name>`).

Whenever the room listens beyond loopback, `serve` prints a warning: the traffic is plain HTTP
there, so use it only on Tailscale (encrypted) or a network you trust, never the open internet.

## Your team

Teammates are named AI members you set up and then talk to like colleagues: write `@sage` in a
message and Sage answers in the thread, under its own name. A teammate has:

- a **handle** (`@sage`: 2 to 24 lowercase letters, digits or hyphens) and a **name** (Sage);
- a **role**: your instruction for it, in your own words, up to 2,000 characters ("You are a calm
  cooking coach. Keep answers to one sentence.");
- a **kind**:
  - **agent** answers through your Perennos Tempo agent, in guest mode, like `@tempo`, with a
    conversation of its own in each room. An agent that supports teammate roles uses the role and
    the model you pick; an older agent uses its own models, and Room puts the role in front of each
    request instead;
  - **engine** is a direct chat with one model in Ollama (it needs a model, like `qwen3:4b`). Room
    sends the role and the room rule as the system message and the same request Tempo would get.
    Ollama is found at `ollama_url` in `config.json`, else the `OLLAMA_URL` or `TEMPO_OLLAMA`
    environment variable, else `http://127.0.0.1:11434`. Only this machine, a home or office
    network, or your tailnet is ever called. When you add an engine teammate and Ollama answers,
    the model is checked against what is installed; if Ollama is down, the teammate is kept, with a
    note;
- optionally a **model** (the app offers the installed ones).

In the app, open **People** in a room (you are the owner): the **Team** section lists your
teammates, adds or edits one, removes one, and has an **In this room** switch per room. From the
command line:

```
tempo-room team list
tempo-room team add sage --kind engine --model qwen3:4b --role "You are a calm cooking coach." --room family
tempo-room team add coder --kind agent --name Coder --role "Review code. Be blunt."
tempo-room team remove coder
```

How they work in a room:

- **Ask** with `@handle` anywhere in a message (any case). Name several and each answers in turn,
  in the order you wrote them. Every ask counts against the same limits as `@tempo`.
- **A one-on-one chat**: a room set to answer every message (`/mode always`) with exactly one
  teammate in it is a DM. That teammate answers every message instead of Tempo; `@tempo` still asks
  Tempo.
- Answers carry the teammate's name and `@handle`, in their own colour; a teammate can never pass
  as Tempo or as you. "Sage is thinking" names who is working.
- **Removing** a teammate takes it out of every room; its record moves to `trash/`, nothing is
  deleted. Its handle and name are free again.
- **To let someone else in**: the code is the key. **Share this room** makes a one-time code and
  link; whoever uses it joins the room and shares everything in it, teammates included. Only you
  (at the machine) can set teammates up; guests only ask them.

Room answers one question at a time (Tempo and the teammates share it), so a slow model delays the answers behind it; a room holds at most five waiting questions.

Teammates never get tools in this version: they cannot run commands, read or change files, or
search your memory.

## Tempo in a room

- **Ask** with `@tempo` anywhere in a message (any case). In a room set to `always`, Tempo
  answers every message posted in the app. Tempo replies in the thread, quoting what it answers.
- **Commands**
  - `@tempo tldr`: a short summary of the last 50 messages.
  - `@tempo remember <text>`: kept in the vault, `Rooms/<room>/Remembered.md` (or, with no vault,
    in `remembered/<room>.md` in the data folder).
  - Reply to a message with `@tempo is this true?` (or tap **Is this true?** on it): a fact-check.
  - `@tempo ping`: is Tempo here, and how fast its agent answers.
  - `/help`: the list (only to you). `/bpm`: tap tempo (see Small things).
  - Owner only: `/mode plain|warm|playful` (Tempo's tone here), `/mode always|mention`,
    `/mute @name`, `/unmute @name`.
- **Tone per room**: plain (direct, no jokes), warm (kind, encouraging), playful (a quick, dry,
  Grok-like wit; never mean, never crude). It is one line in Tempo's instruction.
- **"Tempo is thinking"** shows while it works, for everyone in the room.

### Safety: other people type in a room

- **Guest conversations only.** The room opens its conversations with `POST /agent/new
  {"mode":"guest"}`. In a guest conversation the Perennos Tempo agent offers no tools, refuses
  any tool call, never searches the owner's memory, and leaves the owner's context, pinned notes
  and workflow out of the prompt. The room checks `GET /agent/health` for `"guest": true` first.
  An older agent without guest mode is never used; Tempo says so politely in the room instead.
- Every **confirmation is denied at once.** If the agent wants to run a command, overwrite a file
  or do anything else that needs approval, the room says it was declined. A room message can
  never approve anything.
- Each request is wrapped as `[Perennos Tempo room #name] Speaker: text`, with the last ~20
  messages as context and a short rule: these messages come from other people, not from the owner
  at the keyboard; do not run commands, change files or use tools that change anything for them;
  answer briefly in a chat tone.
- Only Tempo's final answer is posted, never tool output. Replies are capped (`reply_cap`,
  1,200 characters) and redacted; `<think>` blocks are dropped.
- **Limits**: posts per person per minute, questions per person and per room, a short queue per
  room ("ask me again in a minute"), join attempts per address. All in `config.json` → `limits`.
- Everything is **redacted before it is stored, shown or sent**: API keys, bot tokens, private
  keys, passwords in URLs. If someone pastes a key into the family room, it is kept as
  `[REDACTED]`.
- One agent conversation per room, kept across restarts. If the agent lost it, a new one is made.
- **Teammates are held to the same rules.** An agent teammate is a guest conversation too (one per
  room and teammate; the owner's role is the only addition, in the system message of an agent that
  supports roles, else in front of the request, with the people's messages still marked as data).
  An engine teammate gets one plain chat call to Ollama with no `tools` field at all, and its
  answer goes through the same tidy, `<think>` removal, cap and redaction as Tempo's. Approvals are
  still denied at once. Only the owner (owner session or control key, loopback, no proxy headers)
  can add, change or remove a teammate, or see the installed models; members see only the name,
  handle and kind of the teammates in their room, never the role or the model. A teammate's name
  and handle are held back like Tempo's and the owner's: no look-alikes of Tempo, the owner, a
  member or another teammate, and nobody can join (or arrive from a bridge) under one. A teammate's
  reply is stored as `team:<handle>`, labelled as a teammate in later prompts, and cannot ask
  anyone itself (an `@` in its answer is plain text). Roles are redacted before they are stored.

What happens when things go wrong (each path is tested):

| Situation | What people see |
| --- | --- |
| Agent answers | The answer, in the thread |
| Agent busy | Tempo waits and tries again; then "I'm still working through another question" |
| Agent down | "I can't reach the Perennos Tempo agent right now ... The room still works." |
| Agent error or empty answer | "I hit a snag on that one ..." |
| Agent too slow (`answer_secs`, 300) | "That is taking longer than I'd like, so I stopped waiting." |
| Agent asks for approval | Denied at once; a note in the room says so |
| Vault binary missing | A warning in the log; transcripts stay in the room's own files |
| A bridge token refused | That bridge turns off (logged); the room keeps working |
| WhatsApp webhook port busy | WhatsApp turns off (logged); the room keeps working |

## Bridges (all optional, all off by default)

Each bridge is turned on in `config.json` (`bridges.<name>.enabled: true`) or by its environment
variables. Keys (bot tokens, the WhatsApp access token, verify token and app secret) come only
from the environment, never from a file: each has its usual variable below, or `"token_env"`
(`"verify_token_env"`, `"app_secret_env"`) in `config.json` names another one. A key written in
`config.json` stops that bridge, with a message saying to move it into the environment. Keys are
never printed or logged, and are only ever sent to the platform's own API host. `tempo-room bridges check` shows what is on and what is missing;
`tempo-room bridges check --live` also asks each platform whether the token works. Restart
`serve` after a change. A bridged chat mirrors into a local room (made if it does not exist),
so it shows in the app and the vault. People in the app are not sent to the platforms: only
Tempo's replies go back, to the thread that asked.

### Discord

Free. REST polling every 4 seconds, no WebSocket.

1. <https://discord.com/developers/applications> → **New Application** (name it Perennos Tempo).
2. **Bot** → **Reset Token** → copy it. Store it as an environment variable, for example on
   Windows `setx TEMPO_DISCORD_TOKEN "<token>"` (new terminals see it). Never in `config.json`.
3. Same page, **Privileged Gateway Intents** → turn on **Message Content Intent**. (Without it,
   Discord hides the text of messages that do not mention the bot, so the room mirror stays empty;
   mentions still work.)
4. **OAuth2** → **URL Generator** → scope `bot`, permissions **View Channels**, **Send Messages**,
   **Read Message History** → open the URL → pick your server → **Authorize**.
5. Channel id: Discord **Settings** → **Advanced** → **Developer Mode** on; then right-click the
   channel → **Copy Channel ID**.
6. `config.json`:
   ```json
   {"bridges": {"discord": {"enabled": true, "channels": {"123456789012345678": "family"}}}}
   ```
7. `tempo-room bridges check --live`, then restart `tempo-room serve`.

Mention the bot (`@Perennos Tempo`), write "@tempo", or reply to one of Tempo's messages: Tempo
answers in-thread (a Discord reply) and never pings @everyone or roles. The first look at a
channel only notes where it is, so old history is never answered.

If the first reply fails with Discord error 40001, the bot account may never have been online:
Discord has long asked that a bot connect to its gateway once before posting. Do that once with
any Discord bot library, for example Python's `pip install discord.py` and a five-line script that
logs in and exits; Tempo Room does not need it again.

### Telegram

Free. Long polling (`getUpdates`), no public address needed.

1. In Telegram, open **@BotFather** → `/newbot` → a name (Perennos Tempo) and a username ending
   in `bot`. BotFather replies with the token: `setx TEMPO_TELEGRAM_TOKEN "<token>"`.
2. Add the bot to your group (group info → **Add members** → the bot's username).
3. Find the chat id: turn the bridge on with no chats (`{"bridges": {"telegram": {"enabled": true}}}`),
   start `serve`, then message the bot or mention it in the group. The log says
   `telegram: ignored a message from chat -1001234567890 (Family); add it to bridges.telegram.chats`.
   A direct chat with the bot has your own (positive) id.
4. `config.json`:
   ```json
   {"bridges": {"telegram": {"enabled": true, "chats": {"-1001234567890": "family", "123456789": "riley"}}}}
   ```
5. Restart `serve`. Only listed chats are let in; anything else is ignored.

In groups Tempo answers when mentioned (`@YourBot` or "@tempo"), when someone replies to Tempo,
or for a `/command`; in an allowed direct chat it answers every message. By default Telegram only
gives a group bot those messages (privacy mode), which is all Tempo needs. To mirror the whole
group into the local room, BotFather → `/setprivacy` → your bot → **Disable**, then remove and
re-add the bot to the group.

### WhatsApp (Untested against Meta)

Built and tested against a fake Graph API only. WhatsApp needs more than the others:

- A Meta developer account and a Meta Business account; an app (type Business) with the
  **WhatsApp** product added.
- A business phone number: Meta's test number works for development (up to five recipient
  numbers you verify); a real number cannot be in use by the WhatsApp app at the same time.
- A lasting access token: the API Setup token expires in 24 hours; for real use make a System
  User in Business Settings and generate a token with `whatsapp_business_messaging`.
  `TEMPO_WHATSAPP_TOKEN`.
- The **Phone number ID** (API Setup) → `phone_number_id`.
- The **App Secret** (App settings → Basic) → `TEMPO_WHATSAPP_APP_SECRET`. Every webhook call is
  checked against it (`X-Hub-Signature-256`); unsigned calls are refused.
- A **public HTTPS webhook.** The webhook has its own loopback port (8768) so only it is exposed,
  never the room: `tailscale funnel --bg 8768` (Funnel must be allowed in your tailnet's policy),
  then in Meta: **Configuration** → **Webhook** → Callback URL
  `https://laptop.<your-tailnet>.ts.net/room/whatsapp`, Verify token = the `verify_token` you chose
  (`TEMPO_WHATSAPP_VERIFY_TOKEN`), and subscribe to **messages**.
- `config.json`:
  ```json
  {"bridges": {"whatsapp": {"enabled": true, "phone_number_id": "1055...", "numbers": {"15551234567": "family"}, "public_host": "laptop.<your-tailnet>.ts.net"}}}
  ```

WhatsApp chats are one to one, so every text from an allowed number is for Tempo. Tempo only ever
replies to a message someone sent, inside WhatsApp's 24-hour window; Meta has not charged for
those replies (service messages), but check Meta's pricing page before relying on that. Tempo
never starts a conversation.

## Connect it to Perennos Tempo

**The agent can read and post** through MCP. Add to the agent's MCP servers:

```json
{"mcpServers":{"room":{"command":"<path>/tempo-room","args":["mcp"]}}}
```

Tools: `room_list`, `room_read {room, limit}`, `room_post {room, text}`. "Tell the family room
dinner is at 7" becomes a post from Tempo (marked "via agent" in the app), live for everyone when
the room server runs, and written to the room's file when it does not.

**The vault keeps the transcripts.** Set `vault` in `config.json` (or `TEMPO_VAULT`) to your
Perennos Tempo Vault folder. Each message becomes a line in `Rooms/<room>/<YYYY-MM-DD>.md` under an
`## HH:MM` heading, written in batches with
`tempo-vault append "Rooms/<room>/<date>.md" "<lines>" --heading "<HH:MM>" --vault <dir>`
(no console window on Windows). `tempo-vault` is found next to `tempo-room`, else on PATH, else
at `TEMPO_VAULT_BIN`. Text is redacted first. No `tempo-vault`, no problem: a warning in the log.

**Other tools can check on it**: `GET /room/health` answers
`{"ok":true,"app":"tempo-room","version":"0.1.0","rooms":2,"agent":"up"}` (`"agent":"down"` when
the agent does not answer).

## Command line

```
tempo-room serve [--port 8767] [--bind 127.0.0.1] [--allow-host H]... [--agent http://127.0.0.1:8765] [--public-url https://NAME]
tempo-room owner-link [--port N]
tempo-room rooms [add NAME]
tempo-room invite ROOM [--hours 48]
tempo-room post ROOM TEXT [--as-tempo]
tempo-room team list
tempo-room team add HANDLE --kind agent|engine [--name N] [--role TEXT] [--model M] [--room ROOM]...
tempo-room team remove HANDLE
tempo-room bridges check [--live]
tempo-room mcp
tempo-room version
```

All take `--data DIR`; add `--json` for machine-readable output. Commands that change a room go
through the running server (so everyone sees it live) and straight to the data folder when it is
not running.

## config.json

`config.example.json` lists every key. Copy the ones you need into `<data>/config.json` (the
hash of the owner link is added there; when Tempo Room rewrites the file it keeps the previous
copy as `config.json.prev`). An older `owner_token` in the file is removed on the next start,
and `serve` prints a new owner link.

| Key | Meaning |
| --- | --- |
| `owner_name` | Your name in rooms (also settable in the app) |
| `birthday` | `MM-DD`, empty by default (see Small things) |
| `agent` | The agent's URL (`--agent` wins) |
| `vault` | The Perennos Tempo Vault folder (`TEMPO_VAULT` when empty) |
| `public_url` | As `--public-url` |
| `utc_offset` | `-05:00` and the like, when the machine's clock zone is not yours (`TEMPO_UTC_OFFSET` wins) |
| `reply_cap`, `answer_secs`, `invite_hours` | Longest reply, longest wait for the agent (and for an engine teammate's model), invite (and sign-in link) lifetime |
| `ollama_url` | Where engine teammates find Ollama (`OLLAMA_URL` or `TEMPO_OLLAMA` when empty, else `http://127.0.0.1:11434`); only this machine, a private network or a tailnet |
| `session_days` | Days a member's device can sit unused before it must sign in again (180) |
| `limits` | `post_per_min`, `ask_per_member` per `ask_member_secs`, `ask_per_room` per `ask_room_secs`, `join_per_ip` |
| `bridges` | See Bridges |

Environment: `OLLAMA_URL`, `TEMPO_OLLAMA`, `TEMPO_ROOM_DATA`, `TEMPO_VAULT`, `TEMPO_VAULT_BIN`, `TEMPO_UTC_OFFSET`,
`TEMPO_DISCORD_TOKEN`, `TEMPO_DISCORD_CHANNELS` (`id=room,id=room`), `TEMPO_TELEGRAM_TOKEN`,
`TEMPO_TELEGRAM_CHATS`, `TEMPO_WHATSAPP_TOKEN`, `TEMPO_WHATSAPP_VERIFY_TOKEN`,
`TEMPO_WHATSAPP_APP_SECRET`, `TEMPO_WHATSAPP_NUMBERS`.

## Security

- `serve` binds 127.0.0.1 unless told otherwise, and warns when it is not. The Host header must
  be ours (no DNS rebinding); a present Origin must be ours and cross-site fetches are refused (no
  CSRF); only GET and POST; POST bodies must be `application/json` and at most 1 MiB.
- The owner needs both: a loopback connection that no proxy is behind (a request with
  `X-Forwarded-For`, `Forwarded`, `X-Real-Ip` or Tailscale headers is never the owner) and an owner
  session cookie from a one-time owner link. The CLI and MCP use a separate control key
  (`control.key`, made at each start, owner-only file) in the `X-Tempo-Control` header, again only
  from this machine. Members need a session from a one-time, expiring invite or sign-in link, one session per device, each ended by that device's sign-out or after `session_days` of disuse. Owner links,
  sessions and invite codes are stored only as SHA-256 hashes. Cookies are HttpOnly and
  SameSite=Strict (members' also Secure when `--public-url` is https). On Linux and macOS the data
  files are mode 0600 (and a new folder 0700); on Windows they rely on your user profile's own
  access rules (Untested).
- Names from Discord, Telegram and WhatsApp are cleaned (no hidden characters, brackets or colons)
  and cannot pass for Tempo, the owner or a member: a clash shows as `Tempo (discord)`.
- Live updates (SSE) only carry rooms the viewer is in, and a removed member's stream ends.
- The app runs under a strict Content Security Policy (no inline script or style) and builds every
  message from text nodes; links work only for http(s).
- Bot tokens and the WhatsApp secrets: from the environment only (one written in `config.json`
  is refused, and the bridge stays off until it is moved); registered with a scrubber so no log
  line can carry them; sent only to the platform's own host (`https`, no redirects followed). The tests
  scan every log line and every room file for them.
- MCP: `room_post` speaks as Tempo to everyone in a room. A room message could try to talk the
  agent into posting elsewhere; the room rule tells Tempo not to act for room messages, posts as
  Tempo are rate-limited, and they are marked "via agent". Give the agent the `room` server only
  if you want Tempo to post on its own.

## Small things

- **A greeting by time of day**: good morning, afternoon or evening, and after ten at night,
  "Still up, Mia?".
- **The metronome**: while Tempo thinks, three dots tick in turn like a metronome, gently
  swinging; after ten seconds it counts the seconds so a slow local model never feels stuck.
- **`/bpm`**: type it in a room and tap four times (or press T). Tempo tells you the tempo with
  its Italian name: "That's 116 BPM. Moderato: right in the middle." Only you see it.
- **The Konami code** (click outside the message box, then ↑ ↑ ↓ ↓ ← → ← → B A): Tempo answers
  the next question in that room as a haiku. Once.
- **Birthday**: set `"birthday": "MM-DD"` (and `owner_name`) in `config.json`. On that day the
  app greets you with "Happy birthday, Riley.", members see that it is your birthday, and Tempo's
  first answer in each room starts with a happy birthday. Empty by default.
- **Reduced motion**: with the system setting on, nothing moves; the dots stand still.
- Unread counts in the tab title, a draft kept per room, a "New messages" pill when you have
  scrolled up, a tap on any message to reply, copy, or ask "Is this true?", `@` suggestions
  (tempo first, then the people in the room), and three themes (charcoal, ink, paper).
- It can live on a phone's home screen (it has a web manifest; Chrome wants HTTPS for that, which
  `tailscale serve` gives).

## Files in the data folder

`config.json` (settings, bridges, the hash of the owner link), `rooms.json`, `members.json` (each member's devices, as hashes),
`invites.json` (room invites and sign-in links, as hashes), `team.json` (your teammates), `team-convs.json` (each teammate's agent conversation per room), `owner-sessions.json` (hashes), `control.key` (this run's CLI and MCP key),
`messages/<room>.jsonl` (one message per line, redacted), `agent-cursors.json` (where each room's
agent conversation is), `bridge-<name>.json` (last seen ids), `server.json` (the running
server's address, for the CLI and MCP), `remembered/` (remember without a vault), `trash/`
(removed members and teammates). Every rewrite is atomic and keeps the previous copy as `<file>.prev`.

## Not done (yet)

No attachments or images, no message editing or deleting, no push notifications. People on a
bridge (Discord, Telegram, WhatsApp) cannot be muted from the app; use the platform's own tools.
The bridges are Untested against the real Discord, Telegram and WhatsApp services; they are
tested against fakes.
