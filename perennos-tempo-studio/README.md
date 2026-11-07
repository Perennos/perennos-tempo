# Perennos Tempo studio (`tempo-studio`)

Perennos Tempo can open REAPER and mix and master songs. This crate does that, as a CLI and as an MCP server for the Perennos Tempo local agent.

- **Plan A, REAPER, when asked for.** With `--engine reaper` (or `reaper open`), `tempo-studio` finds REAPER, writes a ReaScript (Lua) that builds the session (one track per stem, stock Cockos FX, ReaLimit on the master, WAV 24-bit render settings), starts REAPER with it, renders, measures the render and, when it misses the loudness target, renders again with corrected track gain (at most twice).
- **Plan B, built in.** By default (`--engine auto` or `builtin`) it mixes and masters itself, with no window, which is what background and scheduled work use: a BS.1770-4 / EBU R128 loudness meter, a mixdown with loudness-based gain staging, and a look-ahead true-peak limiter.
- **Plan C.** If REAPER's best render is still off target, the built-in master fixes that render. If a REAPER run fails, the built-in engine finishes the job. The report says which engine produced the final file and why.

Rust, one dependency (`serde_json`, pinned), standalone crate with its own `Cargo.lock`. Library `tempo_studio`, binary `tempo-studio`.

## Build

```
cargo build --release
./target/release/tempo-studio --help
```

## Examples

```
tempo-studio analyze song.wav other.wav            # loudness, LRA, sample and true peak
tempo-studio analyze song.wav --json

tempo-studio master song.wav                       # streaming preset, built-in engine, no window
tempo-studio master song.wav --engine reaper       # in REAPER: its window opens
tempo-studio master song.wav --preset club --out song-club.wav
tempo-studio master song.wav --target -12 --ceiling -0.8 --engine builtin

tempo-studio mix stems/ --out song-mix.wav                          # a folder of WAV stems
tempo-studio mix kick.wav bass.wav "lead vox.wav" --out mix.wav --master-preset streaming

tempo-studio reaper locate
tempo-studio reaper open song.rpp                  # REAPER GUI with a project, media file or .lua
tempo-studio reaper script song-mix.plan.json --out song-session.lua --render
tempo-studio reaper render song.rpp --preset streaming
```

Everything prints a short summary; `--json` prints the full report (each pass, each render, notes). Exit codes: 0 ok, 1 error, 2 usage.

## Presets

| Preset | Integrated | True-peak ceiling |
| --- | --- | --- |
| `streaming` (default) | -14 LUFS | -1.0 dBTP |
| `apple` | -16 LUFS | -1.0 dBTP |
| `club` | -9 LUFS | -0.3 dBTP |
| `podcast` | -16 LUFS | -1.0 dBTP |
| `broadcast` | -23 LUFS | -1.0 dBTP |

`--target` and `--ceiling` override a preset one by one.

## What it does

**Loudness meter** (`loudness.rs`). K-weighting (BS.1770 high-shelf pre-filter and RLB high-pass) designed by bilinear transform for any sample rate; at 48 kHz it reproduces the BS.1770 table. Integrated loudness with 400 ms blocks, 75% overlap, -70 LUFS absolute and -10 LU relative gates; momentary (400 ms) and short-term (3 s) maxima; loudness range per EBU Tech 3342 (3 s windows every 100 ms, -20 LU relative gate, 10th to 95th percentile); sample peak; true peak by 4x oversampling (49-tap Hann-windowed sinc, polyphase). Channel weights L/R/C 1.0, surrounds 1.41, LFE excluded (5.1 order L R C LFE Ls Rs, or the WAVE_FORMAT_EXTENSIBLE speaker mask).

**WAV** (`wav.rs`). Reads PCM 8/16/24/32-bit, float 32/64, WAVE_FORMAT_EXTENSIBLE and RF64, up to 256 channels, 1 kHz to 1.536 MHz, streamed in blocks (a 10-minute 96 kHz file is analyzed in about 12 MB of memory). Clear errors for other encodings, truncated files and broken chunks; a missing pad byte or a partial last frame is read with a warning. Writes 24-bit PCM (default) or 32-bit float (`--float`).

**Mastering** (`master.rs`). Gain to the target, then the limiter: true-peak detection on the 4x-oversampled signal, a look-ahead hold (5 ms), instant attack, 150 ms release and a moving-average gain ramp, built so the gain at a peak is never above what that peak needs. Up to 3 measuring passes adjust gain and the limiter ceiling until the result is within 0.5 LU of the target with the true peak at most 0.1 dB over the ceiling; then one writing pass, and the written file is measured again from disk. An unreachable target writes the best pass and says why. Silent input is refused.

**Mixing** (`mix.rs`). Every stem is measured as it will sound in the mix and gain-staged to a reference loudness (-23 LUFS by default) plus a role offset from its name: kick +1, drums 0, bass 0, vocal +2, guitar -3, keys -3, pad -6, lead -1, fx -8, other -3 dB. Roles also set a high-pass (vocal 90 Hz, guitar 80, keys 60, pad 120, lead 100, fx 150) and, in REAPER, a gentle ReaComp on vocals and drums. Nothing is panned: mono stems sit in the centre. The sum is trimmed so it peaks at -6.5 dBTP before mastering. Output: the mix WAV and `<mix>.plan.json`. Stems must share a sample rate and be mono or stereo; otherwise the built-in engine says so and REAPER mixes them (REAPER resamples).

**REAPER** (`reaper.rs`).
- Locate: `--reaper` > env `TEMPO_REAPER` > config `reaper_exe` > `C:\Program Files\REAPER (x64)\reaper.exe`, `C:\Program Files\REAPER\reaper.exe` (and `%ProgramFiles%`) / `/Applications/REAPER.app/Contents/MacOS/REAPER` / `reaper` on PATH then `/opt/REAPER/reaper`. "Not found" lists every path tried.
- Session script: new project tab, sample rate, one track per stem (`InsertTrackAtIndex`, `P_NAME`, `D_VOL`, `InsertMedia` with a source-item fallback), ReaEQ high-pass, ReaComp, ReaLimit on the master, render file / WAV 24-bit / sample rate / whole project, `Main_SaveProjectEx`, and only when asked `Main_OnCommand(42230)` and close. Every optional API call is checked first, risky blocks run in `pcall`, FX parameters are set by searching for the displayed value (no guessing at internal mappings), a parameter that cannot be set leaves the plug-in bypassed, and nothing is ever overwritten. The script writes a JSON status file (steps, warnings, FX that could not be added) that `tempo-studio` reads back.
- Render: `reaper -newinst -nosplash -renderproject <copy.rpp>` on a new copy of the project whose render settings point at a fresh WAV. The render is measured; a miss is corrected by moving every top-level track's volume (before the master FX, so ReaLimit still holds the ceiling) in another copy, at most twice.
- Background REAPER runs get `CREATE_NO_WINDOW` on Windows. Opening the REAPER GUI on purpose (`open`, sessions) is visible.

## MCP server

`tempo-studio mcp` speaks newline-delimited JSON-RPC on stdin/stdout (`initialize`, `tools/list`, `tools/call`, `ping`). Tools: `studio_analyze`, `studio_master`, `studio_mix`, `reaper_locate`, `reaper_open`, `reaper_build_session`. Every path must be absolute and inside an `--allow-root` folder (repeatable; default the current folder, or `allow_roots` from the config). `reaper_open` takes a project or an audio file only, never a script (run a script with `reaper_build_session`). A tool that fails unexpectedly answers with an error and the server keeps running. Results are JSON text.

For the Perennos Tempo local agent (its `mcp_config` file):

```json
{"mcpServers":{"tempo-studio":{"command":"tempo-studio","args":["mcp","--allow-root","D:/LocalAI/outputs"]}}}
```

## Config

Optional JSON file via `--config PATH` or env `TEMPO_STUDIO_CONFIG` (see `config.example.json`). Flags win over the file.

| Key | Meaning |
| --- | --- |
| `reaper_exe` | The REAPER program (after `--reaper` and `TEMPO_REAPER`) |
| `engine` | `auto` (default), `reaper` or `builtin` |
| `preset` | Default mastering preset (`streaming`) |
| `allow_roots` | MCP folders when no `--allow-root` is given |
| `render_timeout_s` | How long to wait for REAPER (540 s, under the local agent's 600 s MCP call limit) |
| `reference_lufs` | Stem gain-staging reference (-23) |
| `output_format` | `pcm24` (default) or `float32` |

## Safety

- Outputs are new files. A taken name gets `-2`, `-3` ...; `--force` moves the old output to `.tempo-trash/<date>/` next to it before writing (nothing is deleted) and never writes over an input. Data is written to `<name>.partial` and renamed into place when complete; a failed run leaves only the `.partial`.
- Nothing is ever deleted, and REAPER is never killed: a REAPER that hangs is reported with its process id and left running.
- The REAPER session script works in a new project tab and never overwrites a file; REAPER project copies for re-renders sit next to the project as `<project>-tempo-render.rpp`, `-2` ...

## Test

```
cargo test
cargo clippy --all-targets -- -D warnings
```

The tests use synthetic WAVs only. REAPER is replaced by a fake behind the `Runner` trait (unit tests) and by a shell-script stand-in (Unix integration test). The generated Lua also runs under plain Lua 5.4 against a stub of the REAPER API: `lua5.4 tests/lua/reaper_stub.lua <session.lua> full|minimal|nolimit|failing`.

## Needs a machine with REAPER (not tested without it)

Real REAPER behaviour has not been run: that REAPER runs a `.lua` given on its command line; the API calls in the session script (`InsertMedia` on the selected track, `TrackFX_AddByName` with "ReaEQ", "ReaComp", "ReaLimit", ReaEQ `BANDTYPE0` = 4 for high-pass, the displayed parameter names and values, `Main_SaveProjectEx`, command IDs 40859, 40860, 40914, 40026, 42230); the render settings keys (`RENDER_FILE` as folder plus `RENDER_PATTERN`, `RENDER_FORMAT` `ZXZhdxgAAQ==` for WAV 24-bit, `RENDER_SETTINGS`, `RENDER_BOUNDSFLAG`, `RENDER_SRATE`); the `.rpp` lines the re-render edits (`RENDER_FILE`, `RENDER_PATTERN`, `<RENDER_CFG`, `RENDER_STEMS`, `RENDER_RANGE`, track `VOLPAN`, `ISBUS`); `-newinst -nosplash -renderproject` exiting when done; and the Windows process flags and paths.
