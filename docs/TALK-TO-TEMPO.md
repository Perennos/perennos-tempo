# Talk to Tempo

01 — Say it, don't type it.

Hold the mic next to the chat box and talk. Let go, and your words land in the box and go to Tempo. Tap it once to start and again to stop, if you'd rather. With the box empty and focused, hold Space.

## Your voice stays on your computer

Speech is turned into text by whisper.cpp, running on your own machine. Spoken replies (optional) come from Piper, also on your machine. Tempo never uses the browser's own speech recognition, because Chrome sends that audio to Google. Nothing you say is sent anywhere, and the agent only answers voice requests from this computer, with its key.

The recording is written to a scratch folder, heard once, then moved to Tempo's trash right away (Tempo never deletes). It sits in `<agent data>/trash/<date>/` until you empty it yourself. Only the length of what you said is ever logged, never the words.

## Set it up

```
tempo-launch setup --voice            speech in
tempo-launch setup --voice --piper    speech in and out
```

It shows what it will fetch and how big it is, and asks y/N first. Everything is checked against a pinned SHA-256 before it is used, and goes into `<Tempo home>/voice/`.

- whisper.cpp: Windows gets the official zip. whisper.cpp publishes no ready-made program for macOS or Linux, so there Tempo uses a `whisper-cli` you already have (`brew install whisper-cpp`, or build it) and fetches only the model. The Crate can set that program up too.
- A model that fits your computer: tiny.en (about 75 MB) under 3 GB of memory, base.en (about 150 MB) under 8 GB, small.en (about 490 MB) above that. If your system language isn't English, or you pass `--multilingual`, the multilingual base or small instead.
- With `--piper`: Piper (about 20-26 MB) and one natural English voice, en_US-lessac-medium (about 63 MB).

### The pins

The list lives in `perennos-tempo-launcher/catalog/voice.json`. The program names, versions and addresses there come from the official whisper.cpp releases, the ggml models on Hugging Face (`ggerganov/whisper.cpp`) and Piper's releases, but they have not been checked: the machine that wrote the list could reach neither GitHub nor Hugging Face. So every entry says `"verified": false`, has no hash, and Tempo will not fetch it. A maintainer with network runs `sh catalog/pin-voice.sh`, pastes the real SHA-256 and size into each entry, and sets `"verified": true`. Until then `setup --voice` says so plainly, and you can drop `whisper-cli` and a ggml model in place yourself and set `TEMPO_WHISPER_MODEL`.

## What it asks of the agent

- `GET /agent/voice`: what is set up.
- `POST /agent/voice/transcribe`: 16 kHz mono WAV (the chat sends this). WebM or Ogg only if ffmpeg is installed. Up to 4 MB and two minutes, one at a time, with a timeout.
- `POST /agent/voice/speak {"text"}`: a WAV, when Piper is set up.

## Room

The owner, at the machine, gets the mic in every room. A member gets it only in a room where the owner switched on "Let members use the mic here" (People), because the words are heard by the owner's computer. In a room, speech goes to `@tempo` unless you name someone ("at Sam, ..." becomes `@sam`). From the owner, at their own computer, words about Stage reach it first: say "make it golden hour" or "re-angle the last clip with a slow orbit" and Tempo says what it understood, asks y or n in the room, plays it, and tells only you where the result is (see [STAGE.md](STAGE.md)). A member saying the same words gets an ordinary answer, with no tools.

## Settings

Settings has a Voice block: send when you stop, or let you edit first; read replies aloud; and hands-free.

Hands-free is off by default. When it's on, a bar says so for as long as it is, the page listens through your microphone, and a short whisper pass on a two-second window checks for "Hey Tempo". It only runs while the page is open. Nothing leaves the computer.

With less motion turned on, the ring and wave give way to a plain level bar.

## Not tested yet

Whisper and Piper themselves have not been run end to end: the fake programs in the rehearsals stand in for them, and the pinned downloads are unverified (see above).
