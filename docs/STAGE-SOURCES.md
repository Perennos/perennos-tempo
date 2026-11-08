# Stage: where the workflows came from

Stage (video and image workflows) plays ComfyUI workflows that other people built and shared. This page says whose they are, what each one needs, which we took, and which we left and why. The plain description of Stage is in [STAGE.md](STAGE.md).

## Where this came from

In October 2026 ComfyUI ran a $25,000 community challenge to rebuild the main features of a closed video platform as open ComfyUI workflows on open-weight models, after that platform said it would open its code and did not. The projects below are the community's answers. Stage uses plain names for what they do ("character swap in video", "re-angle", "relight"), not the closed platform's own trademarks.

All of this was read on 2026-11-07 from the public repositories, at the commits named. Stars, licence and last push come from GitHub's search, so the commit Stage pins is the newest one that search could find, not necessarily the very tip. Hugging Face was not reachable from where this was done, so **no model file was downloaded, hashed or sized by us**: sizes and licences are what the projects themselves say, and every model's SHA-256 is marked `verified: false` in `catalog/stage.json` until someone pins it. Stage records the SHA-256 of each model the first time it fetches it and writes it into every render's sidecar.

## What Stage took

Nine workflows, from three projects. Each is pinned to a commit and the SHA-256 of its workflow file (`catalog/stage.json`), and Stage checks that hash before it keeps the file.

| Stage id | Feature | Source project (licence) | Least graphics memory | Custom nodes | Ran upstream |
| --- | --- | --- | --- | --- | --- |
| `swap-character-video` | character swap | neuregex/ComfyUI-freefield (MIT), app `recast`: SCAIL-2 with SAM 3.1 tracking | 24 GB | none | measured on a 48 GB card, not on 24 GB |
| `swap-character-photo` | character swap | neuregex/ComfyUI-freefield (MIT), app `character-swap`: FLUX 3 Klein | 12 GB | none | ran on an RTX 3090 Ti (24 GB) |
| `effect-clip` | effect pack | neuregex/ComfyUI-freefield (MIT), app `effects`: Wan 2.1 image-to-video with open effect LoRAs; Stage offers 9 of the 24 | 20 GB | none | not run on 24 GB upstream |
| `camera-photo` | camera moves | neuregex/ComfyUI-freefield (MIT), app `camera-moves`: Wan 2.2 Fun Camera | 20 GB | none | not run on 24 GB upstream |
| `re-angle-video` | re-angle | Tekholms-LW/ReAngle (Apache-2.0): depth lift, new camera path, LTX-2.5 fills what the move shows | 12 GB | LTXVideo, GGUF, VideoHelperSuite, Video-Depth-Anything, comfyui-reangle | RTX 5080 16 GB, about 55 s a chunk |
| `paint-edit-photo` | paint-to-edit | neuregex/ComfyUI-freefield (MIT), app `inpaint`: FLUX 3 Klein | 12 GB | none | ran on an RTX 3090 Ti |
| `clean-plate-video` | paint-to-edit (remove) | neuregex/ComfyUI-freefield (MIT), app `vfx-clean-plate`: LTX-2.5 clean plate LoRA | 24 GB | LTXVideo | ran on an RTX 3090 Ti |
| `relight-photo` | relight | neuregex/ComfyUI-freefield (MIT), app `relight`: Qwen Image Edit with a relight LoRA | 24 GB | none | not run on 24 GB upstream |
| `lip-sync-video` | lip sync | neuregex/ComfyUI-freefield (MIT), app `dub`: LTX-2.5 DubIt LoRA | 24 GB (with plenty of system memory) | LTXVideo | ran on an RTX 3090 Ti with 104 GB of RAM |

"Least graphics memory" is the lowest tier in the catalogue. For the Wan and LTX workflows it is our reading of upstream's own figures (the largest phase it loads at once, plus room), not something we measured. Stage will not send a run to a machine below it, and picks a smaller setting on a smaller card where the catalogue has one (re-angle has two tiers, 12 and 20 GB).

**The one source that did most of the work.** neuregex/ComfyUI-freefield ships 117 apps as plain ComfyUI workflows, each with a typed list of its inputs, which widgets they land on, its models with sizes and licences, its custom-node pins, and a measured VRAM figure. Its test file records which apps ran without an error on an RTX 3090 Ti on 2026-11-05 (72 of 117). Stage's catalogue was built from that file's workflows and metadata, plus our own Pedal vocabulary on top. Nothing in Stage was copied from its Python, its web page or its server.

## What we left, and why

| Project (licence) | What it is | Left because |
| --- | --- | --- |
| D3voz/openjutsu (MIT, about 1.9k stars) | One-click character swap in video, a Next.js app over ComfyUI workflows (`workflows/*.api.json`) on MiniMax H3, SAM 3.1 and FLUX | Looked at in full on 2026-11-07 and **not added**: the MiniMax H3 licence does not allow use in the US, the EU, the UK or Korea, and one pack could not be pinned. Details in "openjutsu, read in full" below. |
| Comfy-Org/comfy-openjutsu (MIT) | The app behind a hosted ComfyUI page for the same swap | Its own README says it needs a Comfy Developer Platform account and key, and it asks GPT Image 2.5 (an OpenAI model) for a first frame. That is a paid, cloud route. Dropped. |
| lucataco/open-source-higgsfield (Apache-2.0) | Recasting with SAM 3.1, FLUX 3 Klein and SCAIL-2, long takes in 81-frame segments, core nodes only | Needs about 50 GB of graphics memory and a Python runner that queues each segment; one graph on its own is not the whole job. The same models are in `recast`. Worth a Rust port of its segment runner later. |
| droppedmyza/freefield (MIT) | 20-odd one-photo effect presets as workflows plus a Python gallery app | `neuregex` carries many of the same effects. Its README also describes output checks (likeness, a hand wandering in from the edge) that are worth porting; we did not read that code. |
| raph651/open-source-canvas (MIT) | A node canvas in the browser over ComfyUI | A front end, not a workflow. Its README also says it reaches hosted models through partner nodes. Tempo's own pages cover the canvas; no front end is vendored. |
| Fourques/Takeboard (Apache-2.0) | A desktop creation canvas on ComfyUI | A front end, not a workflow. Same reason. |
| rk0neki/OpenKatana (Apache-2.0) | Agent-driven editing: cuts, beat-synced edits, typography, 75 MCP tools, a handful of ComfyUI workflows | The editing is Python and WebGL code, not ComfyUI workflows; Tempo's version is Rust. It asks before spending credits and has a local-only route. Beat-synced cuts are not built yet: Studio does loudness only, not beats. See "Next steps" in STAGE.md. |
| Arfolomew/backlot (MIT) | About 30 camera moves and effects, a 16 GB card, driven through ComfyUI's SDK | Built around MiniMax H3 and a Python studio; its camera moves and lenses are what the Pedal vocabulary borrows as an idea, not code. No workflow JSON files in the repo to pin. |
| lnguyen503/backlot (MIT) | A workflow registry and an MCP server for ComfyUI and Blender | Last pushed 2026-07, before the challenge, and what it does Stage now does in Rust. Dropped. |
| wexloops/openfield (MIT, see below) | A studio over ComfyUI: video, stills, 3D blocking, effects, re-shoot, 77 workflow files | Not mined in this pass; its workflows overlap the ones we took. The best place to look next for more effects and a motion-copy workflow. |
| AIPowerGrid/openfield (MIT) | Product shorts and video restyling over ComfyUI and open models | It is built to run on your GPU or on Comfy Cloud and shows the price before each step. Search found no workflow files in it to pin. Left for a closer look. |
| AInVFX/ainvfx-paintaplate (Apache-2.0) | Paint on frame 1, carry the edit through the shot, export in the plate's format | Recommended with 32 GB or more of graphics memory, about 60 GB of models, and it works through its own browser page, which cannot be driven without a person painting. `clean-plate-video` and `paint-edit-photo` cover erase and edit for now. |
| navarisun1982/comfyui-motion-previs-studio (MIT) | 3D camera and motion previs in a web app, with optional Gemini and OpenAI assistants | A TypeScript app that needs a person in front of a 3D view; its assistants call paid cloud models. Previs is not built. |
| sintetizacom/easycomfee (MIT) | A studio: image, video, LTX effects, lip sync, music and voice, with Comfy Cloud options | The cloud and paid options (Magnific, Topaz, Qwen3-TTS) sit next to the local ones in one app; the local workflows are in a catalogue of Python and JSON we did not pin. Its LTX effects (day to night, add water, remove compression) are the next ones to add. |
| thePloggy/CreatorOS-Studio (Apache-2.0) | A character-led studio, 62 functions, 25 workflow files, a 15-tool MCP server | Big and Python-heavy, and it pre-wires a model whose licence its own README says excludes the EU, the UK, South Korea and the US. Not mined. |
| kevin9327/OpenSourceHiggsfield (MIT) | A 3-in-1 cinema platform with a 3D re-shoot rig | Has a hybrid mode that sends work to Comfy Cloud, and it is a Python backend. Dropped for now. |
| chenthedesignmaker/ComfyUI-Picky (GPL-3.0), PxTicks/h3-motion (AGPL-3.0), Burgstall-labs/ComfyUI-SpEdE (AGPL-3.0) | Node packs and tools, one for Gaussian splat edits | Not read. Copyleft licences mean Stage would have to link, not bundle, and we did not spend time on them. Listed so they are not forgotten. |

On wexloops/openfield: GitHub's search reported its licence as unknown, but the repository does carry an MIT licence file, so it is not link-only. We still took nothing from it.

Nothing in the nine we took calls a paid or cloud service: every graph is ComfyUI core nodes plus the custom packs above, and Stage checks that again on every run (see below).

## openjutsu, read in full

Read on 2026-11-07 from `raw.githubusercontent.com`, at commit `7fe118bb5a61fcfac0148e1bfea3dd29884dbf26` (the newest GitHub search lists, 2026-10-30), MIT, 1,904 stars. `codeload.github.com` answered 403 from here (the session's egress policy), so no archive was downloaded and no file list was read: single files only, found from the README, and GitHub's search for the rest. Hugging Face was out of reach too.

**The decision: not in the catalogue as `swap-character-video-pro`.** It would need every model open and licensed for local use, every pack pinned, and no paid node. Three things fail or could not be shown:

1. **MiniMax H3's licence excludes the US, the EU, the UK and the Republic of Korea.** Read from a copy of the MiniMax H3 Community License Agreement (licence date 2 August 2026) that three repositories carry word for word (`OpenVDN/vdn-minimax-h3`, `SceneWorks/SceneWorks`, `Pika-Labs/Pika-H3-Flex`; a fourth differs only in trailing spaces). The original on Hugging Face could not be opened, so this is a copy, not the source. Its terms: the grant is "solely within the Applicable Territory", which is "worldwide, excluding the Excluded Territories", and those are the European Union, the United Kingdom, the Republic of Korea and the United States of America (I.3 and I.5, II). Using the model or its outputs outside the territory is not authorised (V.4, and the acceptable use policy's first item). Tempo cannot know where its owner is, and most owners are in those places. The same text also asks for a separate authorisation above $20M a year in revenue (IV.1), "MiniMax H3" shown on a commercial product's screen (IV.2), no use of outputs to improve another AI model (V.3), and machine-generated content disclosed in public places (AUP 12). A speed LoRA made from H3 (the workflow loads `minimax_h3_DMAD_4step_full_lora_avg_rank_39_bf16.safetensors`) is a derivative under that text; I did not verify that file's own licence. If you decide Tempo may offer it behind a gate that shows this clause and asks the owner to confirm where they are (the way `stage cloud` asks), that is a product decision, not something I made.
2. **One pack could not be pinned.** `ethanfel/ComfyUI-MiniMaxH3-Context-Loop` (GPL-3.0) supplies the clip chain, review and assembly nodes (`MiniMaxH3Chain*`) that every swap graph uses. GitHub lists it as a fork (566 stars), and search does not index a fork's commits, so no commit could be named without a call I was told not to make. Its README also asks for a second pack (`seitanism/ComfyUI-H3-Motion-Context-MultiRef`), and its `__init__.py` says it patches ComfyUI's H3 code at run time on older builds. I did not read the rest of its code.
3. **"No paid API" is shown for what was read, not for all of it.** In `simple.api.json` (22,570 bytes, SHA-256 `24dc5b8ef45788c839beb96b617b404308ddfd8921e89cfd9008442ad29a666a`, 70 nodes) no class name is a partner node by Stage's own list (the `MiniMaxH3*` classes load local int8 files through `UNETLoader`, `CLIPLoader` and `VAELoader`). D3voz's `comfyui-h3-oneclick-swap` is one 2,123-line `__init__.py`: I read its imports (torch, numpy, Pillow, ComfyUI's own modules) and searched every line for network, subprocess, `eval` and `exec` calls, and found none. For `PromptBuilder` I read the imports of its four files and searched them for outbound calls and found none (its server routes use `aiohttp` to answer the browser); it lists `av` as a Python requirement, which Stage would print and never install. Neither was read line by line. I did not read KJNodes, VideoHelperSuite or the Context-Loop pack's code. The README's "Deploy on the Comfy Developer Platform" is a hosted route, not part of the local graph.

What the rest of the licences look like, for whoever picks this up:

| Part | Licence | Used by |
| --- | --- | --- |
| MiniMax H3 weights, the DMAD speed LoRA | MiniMax H3 Community License (territory above) | every swap |
| SAM 3.1 (`sam3.1_multiplex_fp16`) | Meta's SAM License (already in `swap-character-video`) | every swap |
| Qwen3-VL 8B (describer), Qwen3-VL 32B (H3 text encoder) | Apache-2.0 per the H3 licence's own note and Qwen's licence file; not opened here | every swap |
| FLUX.3 Klein | FLUX Kommunity License (gated) | From picture, optional |
| SolarRenderer LoRAs | repo is Apache-2.0; the weights' own licence not read | From picture, optional |
| InsightFace `buffalo_l` | non-commercial research only, says openjutsu's README | Fix faces, optional |
| `face_yolov8m.pt`, `person_yolov8m-seg.pt` (Bingsu/adetailer) | AGPL-3.0, says the README | Fix faces, optional |

Packs, pinned at the newest commit GitHub search lists on 2026-11-07 (not necessarily the tip). "Simple" is `simple.api.json`, the one-person swap:

| Pack | Pinned at | Licence | Needed by |
| --- | --- | --- | --- |
| ethanfel/ComfyUI-MiniMaxH3-Context-Loop | none: a fork, commits not listed | GPL-3.0 | Simple, Advanced, Object |
| Adudeguyman/ComfyUI-Fantastic-MiniMaxH3-PromptBuilder | 22e719fe89336c628383363182a4da3cb58d4f9b | MIT | Simple, Advanced, Object, Fix faces |
| kijai/ComfyUI-KJNodes | ab72eda7ffad6280422d71ab7d54d3fb87c0ba18 | GPL-3.0 | Simple (mask growth), From picture |
| Kosinkadink/ComfyUI-VideoHelperSuite | 4d907bee61e92c2e65af3bd6383a4e4d356126d1 | GPL-3.0 | Simple (already pinned for re-angle) |
| D3voz/comfyui-h3-oneclick-swap | e44a56e92920000e675d14be484aea0d7498d63e | MIT | Simple, Advanced, Object |
| Enderfga/SolarRenderer (folder `comfyui`) | 19d1239daf668d43ae628436a71a661b16fddcaf | Apache-2.0 | From picture |
| Fannovel16/comfyui_controlnet_aux | 7f914115c4c16dc51da3c24604fb733ec0e89aef | Apache-2.0 | From picture |
| Carasibana/ComfyUI-H3-FaceRefine | d8521d14fe0d721d80cd9417fff5a559cbc21aba | MIT | Fix faces |
| ltdrdata/ComfyUI-Impact-Pack | 9cd74e5134cc441739c193e7a5ae6902ab5edb18 | GPL-3.0 | Fix faces |
| D3voz/comfyui-image-detail-contrast | 485a5e246424ce012b998862b45f06d21e8d63b2 | MIT | Fix colours |
| orion4d/Orion4D_FXMax | 7048e8423ad7d1a6a52119d0d4870c2b7fc2885f | MIT | Fix colours |
| D3voz/comfyui-websocket-reconnect-fix | 12b5fdf38e8bdbf8a560b9196561244bf65621f4 | MIT | live progress only, adds no nodes |
| xmarre/ComfyUI-Spectrum-MiniMax-H3 | 806fe498d574a726fecb8818917f00a4b993fd77 | GPL-3.0 | optional speed-up |

Workflow files, for a later pin (SHA-256 of what was read, at the commit above):

| File | SHA-256 |
| --- | --- |
| `workflows/simple.api.json` | `24dc5b8ef45788c839beb96b617b404308ddfd8921e89cfd9008442ad29a666a` |
| `workflows/advanced.api.json` | `e192bf774435f907e8dfc38953b14e8dd2c81b2683465c21c554344139fb52d7` |
| `workflows/advanced_maxq.api.json` | `5aa5fd9eae7f526e6b6a4614764d5213459f2bdb155453656637c73d7ef24670` |
| `workflows/object.api.json` | `fbfec05071d7f6d9986ba2650d58db9a0e5810a1e13f8efba36afbbf0f67eee2` |
| `workflows/face.api.json` | `6ebc74af833a352ec68e19ba2be6ef43fca132c426cb22c11bfd99d10548dd81` |
| `workflows/color.api.json` | `1a81f437b9aa8f746d083c8ea8e47695628009d62447590b03ae0b2e4bf5b7a8` |
| `workflows/mask.api.json` | `86d7902c94a557eaf03fa1440bca53724ccc3939d2a3b7927890e0216102e4d7` |

Pedals: if it ever goes in, the swap itself maps onto `seed` and `lens`/`light` only through its prompt; I did not map any, because nothing was added.

## Custom nodes and their licences

| Pack | Pinned at | Licence | Why it is needed |
| --- | --- | --- | --- |
| Lightricks/ComfyUI-LTXVideo | 44c8a36b | LTX-2 Community License (not Apache) | LTX-2.5 nodes, IC-LoRA guides |
| city96/ComfyUI-GGUF | 6ea2651e | Apache-2.0 | loads the GGUF model for re-angle |
| Kosinkadink/ComfyUI-VideoHelperSuite | 4d907bee | GPL-3.0 | loads and saves video for re-angle |
| yuvraj108c/ComfyUI-Video-Depth-Anything | a0db08e6 | Apache-2.0 | depth for re-angle |
| Tekholms-LW/ReAngle, folder `comfyui-reshoot-nodes` | d13a2842 | Apache-2.0 | camera paths and depth reprojection |

These run inside ComfyUI, as separate programs; Tempo does not link them. Stage looks at each one's code before it goes in and never runs `pip` for you.

## Models: what to know before you add

- **FLUX 3 Klein** (character swap in a photo, paint to edit): FLUX Kommunity License, non-commercial, or commercial under a revenue cap. Gated: you accept its licence on Hugging Face and put the file in place.
- **LTX-2.5** (re-angle, clean plate, lip sync): LTX-2.x Community License. Free including commercial use under about $10M a year in revenue; larger companies need a paid licence, per upstream. Gated.
- **Wan 2.1 and 2.2, Qwen Image Edit, SCAIL-2:** Apache-2.0 or MIT as the upstream cards say.
- **SAM 3.1:** Meta's SAM License.
- **Video Depth Anything:** use the Small model (Apache-2.0). Base and Large are non-commercial, and Stage's catalogue only names Small.
- **The effect LoRAs:** Apache-2.0 per the Freefield catalogue. Each is its own 0.36 GB file, fetched only when you ask for that effect (`stage add effect-clip --with effect=crush`).

Outputs are AI-made. Keep them labelled that way where you publish, and only use footage and characters you have the rights to.

## Credit

Thank you to the people behind these projects, and to Comfy-Org for ComfyUI and the challenge. The workflows are theirs; each keeps its own licence, named above and in `catalog/stage.json`. Stage's catalogue, Pedals, and Rust code are Tempo's.
