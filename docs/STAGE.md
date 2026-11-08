# Stage

Stage (video and image workflows) plays ComfyUI workflows on your own machines: re-angle a clip, swap a character, make an effect clip, move the camera, relight, lip sync. Pedals (modifiers) change them: a camera orbit, a lens, a light, a strength. The AI engines call them as tools, the way they would call a cloud model's tool, except nothing leaves your computer.

The workflows were built by other people, in the open ComfyUI community. Who, under which licence, and what we left out is in [STAGE-SOURCES.md](STAGE-SOURCES.md).

```
tempo-launch stage                      what there is, and what is set up here
tempo-launch stage show ID              one workflow in full: licence, sizes, Pedals
tempo-launch stage check [ID]           does it fit, and on which Fleet machine
tempo-launch stage add ID [--yes] [--with PEDAL=CHOICE]
tempo-launch stage remove ID [--yes]    to the trash
tempo-launch stage enable ID | disable ID
tempo-launch stage run ID --input NAME=PATH --pedal NAME=VALUE [--takes N] [--again] [--on MACHINE] [--seed N]
tempo-launch stage cue "WORDS" [--plan] [--yes] [--json]
                                        say it in words ("golden hour", "slow orbit"): plays the last run again
tempo-launch stage cloud ID on|off      allow a paid or cloud node for one workflow (off, always, until you say)
tempo-launch stage folder [PATH]        where ComfyUI lives
tempo-launch stage mcp                  the tools for the agent
```

## ComfyUI comes from the Crate

Stage needs ComfyUI, and ComfyUI comes from [the Crate](TEMPO-SETUP.md): `tempo-launch crate show comfyui`. Stage adds no second way to install it. Tell Stage where it lives once, with `stage folder /path/to/ComfyUI` (or Stage finds it from the agent's `comfy_output_dir`), and start ComfyUI the way you already do.

## Set up one

```
tempo-launch stage add re-angle-video
```

It shows the plan first and asks y/N: the workflow and the commit it is pinned to, each custom node with its repo, commit and licence, each model with its size and licence, the total download, the least graphics memory it needs, and what upstream measured it on. Then:

1. It fetches the workflow file at its pinned commit and checks its SHA-256. A file that does not match goes to the trash and nothing is kept.
2. It checks the graph against the catalogue: every node and widget a Pedal touches must still be there, with the same class. If upstream drifted, Stage will not run it.
3. For each custom node pack it fetches the code at the pinned commit, unpacks it aside, and looks at it before it goes in (below). A pack you already have is left alone.
4. It fetches the models it can, checks each against its pinned SHA-256 when the catalogue has one, and writes down the SHA-256 it saw when it does not. A gated model (FLUX, LTX: a licence you accept on the model's page) is covered in "Gated models" below.
5. `stage enable ID` turns it on once everything is in place.

Nothing is overwritten and nothing is deleted. `stage remove` moves the workflow, and any custom node Stage itself put in (that no other workflow needs), to the trash. Models stay: other workflows share them, and they are big.

## Custom nodes are code

A custom node is Python that runs inside ComfyUI with your rights, so Stage treats each pack as risky:

- The commit is pinned and the pack list is shown before anything is fetched.
- Before a pack goes in, Stage's own scan reads its Python (a call to the internet, one that sends a key, `os.system`, `eval` and `exec`, `pip install`, files that hold secrets, folder deletes), and Soundcheck runs its scanners over the same folder when the agent is installed. The findings are shown by severity.
- **A high finding blocks the install** until you type `yes`, whole, after reading it. `--yes` does not cover it, and neither does a plain `y`.
- **A node that calls a paid or cloud service is refused**, unless you turned that on for that workflow with `stage cloud ID on`. That goes for a pack's code, for a graph that names a partner node, and for any node ComfyUI itself marks as an API node when it runs. Nothing in the catalogue needs it.
- Stage never runs `pip`. If a pack lists Python requirements, Stage prints them and the command; you run it with ComfyUI's own Python.

If Soundcheck is not there (no agent), Stage says so and its own scan runs alone.

## Pedals

One vocabulary across every workflow. A camera orbit is the same Pedal everywhere; each workflow maps it onto its own widgets.

| Pedal | Values | Where it lands |
| --- | --- | --- |
| camera | static, push_in, pull_out, pan_left, pan_right, pan_up, pan_down, orbit_left, orbit_right, truck, crane_up, crane_down, low_angle, high_angle, roll_cw, roll_ccw, dolly_zoom, crash_zoom | a camera-pose widget, a preset, or an effect LoRA, whichever the workflow has |
| lens | 24mm, 35mm, 50mm, 85mm | added to the prompt |
| light | golden_hour, blue_hour, rim, noir, neon, softbox, overcast, candlelight | the relight workflow's own light text, or added to the prompt |
| strength | a range per workflow | an effect LoRA's strength, a step count, or an IC-LoRA's strength |
| angle | 5 to 30 degrees | how far the re-angle camera travels |
| duration | seconds, a range per workflow | the frame count, at the model's frame rate (snapped to what LTX wants) |
| speed | 0.2 to 3 | camera speed |
| size | pixels, shorter side | the working resolution |
| effect | the workflow's own list | which effect LoRA and prompt |
| mode | replace, animate | character swap in video |
| seed | whole number, -1 for a fresh one | the sampler's seed |

`stage show ID` lists the Pedals a workflow has, with their ranges. A value that is out of range, or a Pedal the workflow does not have, is refused in words before the graphics card is touched. Inputs a Pedal cannot carry (a clip, a photo, a line of text) are `--input NAME=VALUE`.

## Running

```
tempo-launch stage run re-angle-video --input video=/path/clip.mp4 --pedal camera=orbit_left --pedal angle=12
```

A run goes to one machine whole. The candidates are this computer's ComfyUI and the Fleet machines that report ComfyUI up and list the workflow as ready (`tempo-launch fleet allow stage` on those machines). A machine with less graphics memory than the workflow's lowest tier is not a candidate. Of those that fit, the roomiest tier wins, then the most free memory, then the shortest line, and this computer wins a tie. Where a workflow has more than one tier (re-angle has 12 and 20 GB), a smaller card gets the smaller settings and the status line says so, instead of failing.

On this computer it takes the GPU lease the agent's settings name (`lease_status_cmd`, `lease_acquire_cmd`, `lease_release_cmd`), takes our language models off the card first, and gives the card back after, the same as the agent's `comfy_image`. It never takes a card another job holds.

### Another computer: through the Fleet messenger, never at its ComfyUI

ComfyUI has no password, so it stays on 127.0.0.1 on every computer, and Stage only ever talks to the one on the computer it runs on. A run for another computer goes through the Fleet messenger, whose frames are sealed (ChaCha20-Poly1305) and signed (see [FLEET-SECURITY.md](FLEET-SECURITY.md)):

1. Each input file is sent as a blob stream: 256 KiB pieces, one sealed frame each, straight from the file to a temp file on the other computer. It checks the size and the SHA-256 and only then renames it into place; a file that fails goes to the trash. A file may be as big as `tempo-launch fleet cap` says (4 GiB unless you change it, on both computers); a bigger one is turned away at the start.
2. One `stage` message names the workflow, the Pedals and the seed. The other computer never takes a path from it: a file input is the id of a stream it has already received and checked.
3. The other computer plays it with its own `run::play`, against its own ComfyUI, under its own GPU lease, and with its own rules (a paid node is refused there whatever you say here). It plays only when its owner ran `tempo-launch fleet allow stage` and the workflow is added and on there. One take at a time per card.
4. What it made comes back the same way, checked, into your workbench, with the `.stage.json` it wrote (its machine name, the hashes of what really ran there) plus where the file went. The status line says "on the studio box".

The computer that asks does not need the models: it needs the workflow added (its graph is checked for paid nodes) and a Fleet machine that has it. Nothing here needs ComfyUI to listen on the network: leave it on 127.0.0.1.

The run goes through ComfyUI's API (on the computer that plays it): your files are put in its input folder, the graph goes to `/prompt`, the answer comes from `/history`, and each file comes back with `/view`. **Outputs land in the workbench** (`<Tempo home>/crate/work/stage/`) and never replace a file: a name taken becomes `-2`, `-3`. Beside each output is `NAME.stage.json`, the proof of where it came from: the workflow's repo, commit and SHA-256, each custom node's commit and tree hash, each model's licence and SHA-256 (pinned or the one Stage saw), the inputs' hashes, the Pedals and seed, the machine and tier, the time, and the command to make it again.

The status line reads like a crew member: "Re-angled on the studio box, 41 s."

### Takes, resume, again

- **`--takes N`** plays N takes, each on its own seed, spread over the Fleet's cards that fit (round robin, best machine first). Each take gets a quick local check: it is a real picture or video, a video has frames and a length, and is not all black or all frozen (ffmpeg, from the Crate, when it is there). The best take wins and goes to `takes.jsonl` like any other part, with the check as its test result. Your accept or reject on a take works the same way (`tempo-launch conductor takes accept TAKE`), and Woodshed reads that file.
- **Resume.** A run's log (`<Tempo home>/stage/runs/KEY.jsonl`) records each finished take. Run the same command again after a crash and only the missing takes play, on the same seeds.
- **`--again`** starts from the last run of the workflow (its inputs, Pedals and seed) with what you give on top: `stage run re-angle-video --again --pedal light=golden_hour`. The seed stays, so ComfyUI reuses every node upstream of what you changed, and the status line says how many nodes it reused.
- **The Vault.** When the Vault folder is there, the winning render gets a note in `Stage/` with the workflow, Pedals, seed, source and machine, so it is searchable and can be made again. Stage never makes a Vault.

## The tools

Every workflow that is added, ready and turned on is an MCP tool, `stage_<id>` (`stage_re_angle_video`), with its Pedals as typed parameters: a choice is an enum, a number has its range. There are two helpers: `stage_again` (play the last run again with Pedals changed, so "make it golden hour" is `{"light": "golden_hour"}`) and `stage_list`.

`tempo-launch stage mcp` serves them on stdio. The agent connects it the way it connects the Crate, when at least one workflow is on, and asks you before every call unless you trusted it (`mcp_trust`). Through the tool an input must sit in the workbench or a folder you allowed (`tempo-launch crate folders add`).

## Where Stage sits in Tempo

- **Crate:** ComfyUI and ffmpeg come from it. Stage's workbench is the Crate's.
- **Fleet:** the machine choice reads what each machine reports (its ComfyUI memory and the workflows it has ready), the allow list gets one new entry, `stage`, and the messenger carries the run (above).
- **Agent:** registered in `products.rs` like the Crate, asking first.
- **Soundcheck:** reads each node pack before it goes in.
- **Conductor:** four band templates, Product ad, Short film, Recast a clip and Music video. Their members are guests and get no tools: they write `SHOT` lines, and the Conductor, acting for you, plays them after one yes (below).
- **Takes and Woodshed:** Stage's takes are in `takes.jsonl`; your thumbs on them are the same accept and reject, and they teach the band (Lessons, below).
- **Room and Voice:** "@stage golden hour", and the same words said to "@tempo" (below).

## The Conductor plays the shot list

The members of a band still get no tools and play nothing. The Stage parts (the shooter, the cinematographer, the operator) write one line per shot:

```
SHOT 2 | re-angle-video | in clip=/work/dance.mp4 | pedal camera=orbit_left lens=35mm | takes 3
```

When a flow ends with `SHOT` lines in its notes, the Conductor checks each against Stage (the workflow is added, its inputs and every Pedal value are real, a file input sits in a folder you allowed, a machine fits) and shows the WHOLE list: each shot's workflow, Pedals, inputs, takes, which machine plays it and about how long, and the shots that cannot play with the reason. Then it asks y/N once:

```
tempo-launch conductor run product-ad
...
The band wrote 3 shots for Stage. Here is the whole list. Nothing has played.
  1. relight-photo  light=golden_hour  [image: pack.png]  1 take, on this computer, about 38 s each
  2. camera-photo   camera=push_in    [image: pack.png]  3 takes, on the studio box, no estimate yet (it has not run there)
  3. effect-clip    will not play: needs image: the Shooter left [CHECK: which photo]
Play the 2 shots that can play (4 takes in all ...; takes spread over your Fleet's cards)? [y/N]
```

On yes it plays each shot through Stage, with its takes spread over the Fleet, one at a time. The results are filed three places: the takes file (under the band and the shooter's part, so the Woodshed and your thumbs see them), the run's hash-chained log (a plan, a result for each shot, a final), and a `Shots` note in the run's folder linked from its Index (a Vault render note is written too, when a Vault is there). `tempo-launch conductor shots BAND` offers the newest run's list later, for a run nobody was there to answer (a scheduled one) or one you said no to. A list holds at most 12 shots and 36 takes. Files come only from the folders you allowed; the words came from a model.

## Say it in words (Room and Voice)

One parser (`src/stage/phrase.rs`, the same file copied into Room) turns words into a workflow and Pedals: "re-angle the last clip with a slow orbit", "make it golden hour", "push in, 35mm", "dolly zoom for 4 seconds", "crane up fast", "rim light and a 50mm lens", "relight the photo with candlelight", "same again with a new seed". A question ("what is golden hour?") is never read as an order, and a long sentence that merely mentions a light is left to Tempo.

- **At the command line:** `tempo-launch stage cue "golden hour"` says what it understood (the workflow, the Pedals, the last run's inputs, which machine, about how long) and asks y/N. A phrase cannot carry a file, so it plays the last run again with the Pedals the words name; with no earlier run it tells you how to make the first. A Pedal the workflow does not have is left out, and it says so (re-angle has no speed Pedal, so "slow" is said to be left out).
- **In Room:** the owner, at their own computer, writes "@stage golden hour" (a member gets "only the owner can"). Room shows the plan and asks y or n in the room, plays it on y, and says where the result is. Everything Stage says back goes to the owner only: a path on your disk is nobody else's business.
- **In Voice:** speech in a room goes to "@tempo", and from the owner the same words reach Stage first ("@tempo make it golden hour"); a member's words are an ordinary question, with no tools.

## Lessons from your thumbs

When you accept or reject a take a band made on Stage (`tempo-launch conductor takes accept|reject TAKE`), the band's Lessons get one bullet that says which Pedals gave a take you kept or turned down ("For re-angle-video, camera=orbit_left, lens=35mm gave a take the owner kept: use it."). The Pedals come from the `.stage.json` beside the file. The same Pedals judged the same way again make the bullet stronger instead of making a second; a thumb that says the opposite counts against the bullet that said otherwise, so a note that stops being true retires itself. A take that no band made gets no bullet.

## Gated models

A model behind a licence you accept on its page (FLUX, LTX) is "gated". Before a fetch Stage says so and shows the licence in a few plain words with the page to read. If you have a Hugging Face token, in `HF_TOKEN` or the file `huggingface-cli login` writes (`$HF_HOME/token`, or `~/.cache/huggingface/token`), Stage asks one more y/N for that model (`--yes` never accepts a licence for you) and fetches it. The token goes to huggingface.co only, over https, as `Authorization: Bearer`. Hugging Face hands the file over from a content server on another name, and that hop gets no token: Stage follows each redirect itself, one hop at a time, instead of letting curl follow it with a header on. It is never printed, logged or stored (curl is given it on stdin, not on its command line, and an error that echoes it is blanked). Without a token nothing changes: you accept the licence and put the file in place, and Stage tells you where.

## What never ran for real

Be straight about this: **no GPU was here and no real ComfyUI ran.** Everything below was tested with a fake ComfyUI over real HTTP (it answers `/system_stats`, `/object_info`, `/upload/image`, `/prompt`, `/history` and `/view`), a fake fetcher and a fake `tar`, and a fake Soundcheck.

What the tests prove: the catalogue parses and every pin is complete; Pedals land on the widgets the catalogue names, are range-checked, and a graph that drifted is refused; the API-call refusal; the Soundcheck gate (a high finding blocks, `--yes` and a plain `y` do not unblock it, a typed `yes` does); a workflow or a model with the wrong hash is refused and trashed; routing to a machine with enough graphics memory; outputs never replace a file; takes across machines, resume, `--again`, the Vault note, the takes file; and the MCP tool list and calls.

What they do not prove, because it never ran:

- That any workflow here actually makes a good picture or clip on your card. We rely on what upstream measured, and for several it did not measure a 24 GB card.
- The least-memory figures for the Wan and LTX workflows. They are our reading of upstream's numbers, not measurements.
- Every model's SHA-256. Hugging Face was out of reach, so each is marked `verified: false`, and the sizes are upstream's own.
- A real pack's Soundcheck: Stage's scan was tried on small fixtures, and the reading of the agent's JSON report was never run against the real agent.
- A node pack's tree hash is recorded when Stage first puts it in; the catalogue cannot pin it ahead, because the archive was not fetched. The commit in the address is the pin.
- The real GPU lease commands, the real `comfy_start_cmd`, and ComfyUI's real caching: the "reused N nodes" count is read from ComfyUI's own history message, and the seed is held so it can happen, but we never saw a real cache hit.
- The real ffmpeg check (black and frozen frames) against a real clip. Its parsing is tested on sample output.
- Fleet machines for real. A remote run was tested with two messenger nodes on this computer, each with its own home, and a fake ComfyUI on 127.0.0.1 behind one of them; never between two real computers, and never over a 4 GiB file (the biggest test file is 65 MiB, past the old 64 MiB limit, with the same code that streams 4 GiB: it holds one 4 MiB batch in memory, not the file).
- Hugging Face for real. The token flow was tested with a fake fetcher that follows a redirect like Hugging Face does, and with a stand-in program for curl that records how it was called (the token is on stdin and not on the command line, no `--location`). It never ran against huggingface.co: that host was out of reach. If Hugging Face answers a HEAD request on a file differently from what Stage expects, `stage add` says so and leaves the file for you.
- The Conductor's shot list and the phrase parser with a real model and real speech. The tests write the `SHOT` lines themselves; no model was asked to, and no speech was turned into words. A small model may write the lines wrongly, and then they show as "will not play", not as something that plays.
- Room's "@stage" in the browser. Room's tests drive the hub; nobody typed in the real page. The real `tempo-launch` is called through a stand-in program in the tests.
- Windows. The code builds for it and the tests are portable, but nothing ran there.

## Decisions worth knowing

- **A remote machine is reached through the Fleet messenger, never at its ComfyUI.** ComfyUI has no password and stays on 127.0.0.1 everywhere. The cost: a remote run waits for the whole file to cross, and the other computer plays it with its own Stage, so it needs the workflow added and on there (`fleet allow stage`) and the models in its own ComfyUI. The computer that asks needs neither the models nor ComfyUI. `Http::parse` refuses any address but this computer's own, so a `comfy_url` that points at another computer is not used by Stage.
- **Files come only from a checked stream.** The other computer never takes a path from a message. A file that arrives is kept only with the right size and SHA-256; a wrong one goes to the trash and the sender is told why. It takes files from a paired computer only, and only when its owner allowed `stage` or it is waiting for the answer to a call it made.
- **Gated models need your own yes.** A licence you accept is yours to accept: even with a token, Stage asks per model, and `--yes` does not cover it.
- **Members in the Conductor get no tools.** The safety rule that guests never get tools holds: a band's member writes `SHOT` lines, and the Conductor, acting for you, plays them after you have seen the whole list and said yes. Anything it plays reads files only from folders you allowed.
- **Stage's answers in a room are private.** A result path is posted to the owner only, even in a room with others.
- **Stage does not run `pip`.**

## Next steps

Things the owner asked for that are left, each smaller than a day but not done here:

- **Beat-synced edits and music videos.** Studio measures loudness only; it does not find tempo or beats, and Stage does not write a detector. The next step is a beat grid in Studio's analysis (a Rust onset detector), then a cut-list planner that turns it and a set of clips into an ffmpeg plan through the Crate.
- **Brain and dashboard.** A run as a node linked to its band and inputs. The takes file and the Vault note are the hooks for it today.
- **A producer's judge of the takes.** Today the check is local and cheap. Judging a picture needs a vision model; the judge Sections uses reads text.
- **Long re-angle.** One graph is one chunk (about 1.7 s at 24 frames a second). Upstream chunks a longer clip with overlap and a colour match in Python; that runner is not ported.
- **More workflows:** the LTX effects in easycomfee (day to night, add water), the motion-copy and multi-person swap workflows, and wexloops/openfield's 77 files. Each needs its hash, its pins and its look at the code.
- **Pin the model hashes,** once Hugging Face can be reached.
- **openjutsu** (the community's one-click character swap) is not in the catalogue: see STAGE-SOURCES.md for exactly what blocks it.
- **A speed Pedal for re-angle.** "Slow orbit" cannot say how slow on re-angle-video, which has no speed Pedal; only camera-photo has one.
