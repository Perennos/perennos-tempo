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
4. It fetches the models it can, checks each against its pinned SHA-256 when the catalogue has one, and writes down the SHA-256 it saw when it does not. Models whose licence you must accept on the model's page (FLUX, LTX) are listed with the path to put them at; `stage check` tells you when they are there.
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

The run goes through ComfyUI's API: your files are put in its input folder, the graph goes to `/prompt`, the answer comes from `/history`, and each file comes back with `/view`. **Outputs land in the workbench** (`<Tempo home>/crate/work/stage/`) and never replace a file: a name taken becomes `-2`, `-3`. Beside each output is `NAME.stage.json`, the proof of where it came from: the workflow's repo, commit and SHA-256, each custom node's commit and tree hash, each model's licence and SHA-256 (pinned or the one Stage saw), the inputs' hashes, the Pedals and seed, the machine and tier, the time, and the command to make it again.

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
- **Fleet:** the machine choice reads what each machine reports (its ComfyUI memory and the workflows it has ready), and the allow list gets one new entry, `stage`.
- **Agent:** registered in `products.rs` like the Crate, asking first.
- **Soundcheck:** reads each node pack before it goes in.
- **Conductor:** four band templates, Product ad, Short film, Recast a clip and Music video. Their members are guests and get no tools: they write the shot list and the exact `stage run` lines, and you run them. A part that ran on Stage shows as a take in the takes file.
- **Takes and Woodshed:** Stage's takes are in `takes.jsonl`; your thumbs on them are the same accept and reject.

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
- Fleet machines for real. A remote run goes straight to that machine's ComfyUI over your private network (see below), and that path ran only against a fake on this computer.
- Windows. The code builds for it and the tests are portable, but nothing ran there.

## Decisions worth knowing

- **A remote machine is reached at its own ComfyUI.** Fleet's call channel carries arguments and brings files back, but it cannot carry a video in. So Stage talks to the other machine's ComfyUI directly, at the address the Fleet already holds (port 8188, or `TEMPO_STAGE_COMFY_PORT`), on a private or tailnet address only. That needs ComfyUI there to listen on the network (`--listen`), and ComfyUI has no password: only do this on a network you trust. The Fleet's signed channel is used for what it can carry: the heartbeat that says ComfyUI is up, how much memory it has, and which workflows are ready.
- **Gated models are fetched by you.** The downloader has no way to send a Hugging Face token, and a licence you have to accept is yours to accept. Stage lists the file, the page and the path.
- **Members in the Conductor get no tools.** The safety rule that guests never get tools holds: a band's member writes the plan, and Stage plays it when you run it.
- **Stage does not run `pip`.**

## Next steps

Things the owner asked for that are left, each smaller than a day but not done here:

- **Beat-synced edits and music videos.** Studio measures loudness only; it does not find tempo or beats, and Stage does not write a detector. The next step is a beat grid in Studio's analysis (a Rust onset detector), then a cut-list planner that turns it and a set of clips into an ffmpeg plan through the Crate.
- **Room.** "@director re-angle this with a slow orbit" routing to a Stage tool, with the result posted back as a link. The tool exists; the Room hook does not.
- **Voice.** "Make it golden hour" mapped to `stage_again`. The tool exists; the phrase mapping does not.
- **Brain and dashboard.** A run as a node linked to its band and inputs. The takes file and the Vault note are the hooks for it today.
- **A producer's judge of the takes.** Today the check is local and cheap. Judging a picture needs a vision model; the judge Sections uses reads text.
- **Lessons.** Recording which Pedals worked for a band, from your accept and reject on takes.
- **Long re-angle.** One graph is one chunk (about 1.7 s at 24 frames a second). Upstream chunks a longer clip with overlap and a colour match in Python; that runner is not ported.
- **A fleet tool that carries inputs,** so a remote run can use the signed channel instead of ComfyUI's own port.
- **More workflows:** the LTX effects in easycomfee (day to night, add water), the motion-copy and multi-person swap workflows, and wexloops/openfield's 77 files. Each needs its hash, its pins and its look at the code.
- **Pin the model hashes,** once Hugging Face can be reached.
