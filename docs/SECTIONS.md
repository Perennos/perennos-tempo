# Sections and takes

A big job does not have to be one long conversation. The Conductor can split it into **sections (sub-teams)** that members play at the same time, let each section make a few **takes (attempts)**, and keep the best one. A **producer** judges the takes, the way a producer picks the take that goes on the record.

```
tempo-launch conductor sections BAND [FLOW] [--note TEXT] [--takes 1-3] [--resume RUN]
tempo-launch conductor runs
tempo-launch conductor stop [RUN]
tempo-launch conductor takes accept TAKE      (or reject)
```

This sits on what was already there: the band's members and flows, the engine the Conductor picked for each part, the notes in your Vault (or `<Tempo home>/bands/<band>/runs/`), and the agent's guest conversations. `conductor run` still walks a flow one step at a time. `conductor sections` is for the jobs that are too big for that.

Status lines read like crew: "Section 2 done in 41 s, best of 3 takes, tests green."

## What never ran against real models

Everything below was tested with fake engines and fake checks. The tests prove the rules: the limits, the stop, the depth, the tags, the hash chain, the takes file, resume, the order the takes are judged in. They do not prove that a real model writes a good plan, follows the `SUMMARY:` and file-block formats, or scores fairly as a producer. Run it on a small job first and read the notes. The repo checks ran in the tests only as small shell commands, not against a real project, and the reading of Soundcheck's JSON report was never run against the real agent. Fleet machines do not play sections yet: a section always goes to the agent on this computer, which may then use any engine it knows.

## Budgets and the stop switch

One meter for the whole run. It counts **tokens, tool calls, cloud cents and wall time**, and every member holds a child of its parent's meter, so what a member uses counts all the way up and a limit anywhere above ends everything below. The counters are atomics, so members on other threads share them.

Defaults (400,000 tokens, 30 minutes, 200 tool calls, 50 cloud cents) live in `<Tempo home>/conductor.json`, and a band's own `bands/<band>/conductor.json` lays on top. 0 means no limit on that line:

```json
{ "budget_tokens": 400000, "budget_wall_s": 1800, "budget_tool_calls": 200, "budget_cloud_cents": 50,
  "takes": 1, "check": "cargo test --locked", "workdir": "/path/to/your/project", "soundcheck": true,
  "cloud_engines": ["some-engine-id"], "cloud_cents_per_1k_tokens": 0.5 }
```

An unknown key or a wrong type is an error in words, never skipped. Nothing counts as cloud unless you list it in `cloud_engines`; every engine the Conductor knows is on your own machines.

When a limit runs out, the run ends cleanly with one line that says which limit and what got done, for example: "Stopped at the token limit (400,113 of 400,000 used). 2 of 4 sections done, 5 takes made." The notes for the finished sections are written, and `--resume RUN` picks it up again.

**The stop switch.** `tempo-launch conductor stop [RUN]` (no name means the newest unfinished run) writes a flag file next to the run log. The run checks it every tenth of a second, flips the root meter, and every member ends at its next step. For a member waiting on the agent, the launcher also calls `POST /agent/stop {"conv": ID}`. The agent ends that turn at its next step too. Checking happens between steps: a request already with a model finishes first, so a run can pass a limit by one step. Tokens are counted as characters divided by four for the launcher's part, because the agent does not hand the Conductor real counts; the agent's own turns count real ones.

**In the agent.** A send can carry a budget: `POST /agent/send {"conv": ID, "text": "...", "budget": {"tool_calls": 20}}` (`"budget": {}` uses the config defaults `budget_tokens`, `budget_wall_s`, `budget_tool_calls`, `budget_cloud_cents`). A turn without one runs as before and can still be ended with `POST /agent/stop {"conv": ID}` or `{"all": true}`. `GET /agent/health` says `"stop": true` when the agent has it. `budget.rs` is the same file in the agent and the launcher.

## Sections

- The member who plays the flow's first step plans: up to **4 sections** that do not depend on each other, each with a member from the flow. A plan that cannot be read means one section, and the run says so.
- A section marked big can be split once more, by its own member. Depth is capped at **2**: sections, and their sections, and no deeper.
- Up to 4 members are asked at the same time, however many sections there are.
- **A child never holds a tool its parent lacks.** A child's tools are the parent's, narrowed if it asks for fewer. Members are guest conversations, so **guests never get tools**: today every section has none, and the run log records `"tools": []` on each step.
- **A child's output comes back as data.** Only a short summary goes up, in `<section-report ... trust="data">` tags. The next member is told that what is inside the tags is data to read, never instructions, and the tag name is scrubbed out of the text so nothing inside can close it. The full answer goes into your notes, not into the next member's prompt. This is what stops a prompt injection in one section from steering the next.
- **The producer is on a different engine from the player** when one is up (the strongest other engine, with its biggest model). When none is, the player's own engine judges, and the verdict line in the log says `producer_on_another_engine: false`.

Notes land where flow notes do, so the Conductor page's last runs find them: `Index.md` (sections, takes, the winner, what was used, the log), one note per section, and the finished piece written by the last member of the flow.

## Takes and judging

A part is played as N takes (`takes` in conductor.json, or `--takes 2`; 1 by default, up to 3 when you want the best of a few). Each take is judged in trust order:

1. **Tests**: the repo's own checks, run in an isolated copy of `workdir`. A take that writes files marks them as `=== file: path ===` blocks. The checks copy your folder to a scratch folder (skipping `.git`, `target`, `node_modules`, links), lay the files over the copy, run your `check` command there (no shell), and remove the copy. Your folder is never touched. A take with no file blocks has nothing to test: its tests are `null`, which is not green. It is a copy, not a sandbox: the check runs with your rights.
2. **Soundcheck findings** by severity: the agent's `soundcheck --all --scanners-only --json` over that copy, counted as high (critical counts here), medium, low.
3. **The producer's score**, 0 to 1, from a rubric: does it do the task, completely and correctly, in the form asked.
4. **Your accept or reject**: `conductor takes accept TAKE`. An accepted take wins over the rest; a rejected take cannot win. Rejecting the winner hands the win to the next best.

**The self-check loop** is at most 3 rounds per take: run the checks, feed the errors back to the member, try again. The best take wins, ties go to the earlier one.

## The run log

`<agent data>/runs/YYYY-MM-DD/<run-id>.jsonl`, one event per line:

`{"v":1,"ts":...,"run":...,"parent":...,"depth":...,"member":...,"engine":...,"model":...,"kind":"plan|tool|result|final|verdict|stop", ..., "prev_hash":...,"hash":...}`

- `plan` is what a member was asked, `result` its answer, `tool` a repo check, `verdict` the producer's pick for a section, `final` or `stop` the end of the run, with what it used and the budget it had.
- `hash` is SHA-256 over `prev_hash` plus the line as written up to the hash, so a changed, dropped or moved line breaks the chain. `conductor runs` walks it and says "log intact" or "log NOT intact".
- Secrets go through the same redaction as the rest of Tempo before a line is written, and a very long text is cut.
- **Resume.** `--resume RUN` reopens a log that is still an unbroken chain and plays only the steps that are not in it. Every step has an idempotency key (section, take, round, and a hash of what was asked), so a finished answer, check or judgement is read back, not run again, and takes are not written twice. The checks are the only steps with a side effect, and they only touch their own scratch copy. File changes the agent itself makes in an owner conversation are kept by `checkpoint.rs`, as before; sections do not change your files.

## Safety, unchanged

- Nothing is deleted: the stop flag is a small file that stays, and your folders are only ever copied. The one removal is the scratch copy a check made for itself, inside `<agent data>/runs/work/`, only after the check ended.
- Risky actions ask first, and guests never get tools.
- Nothing leaves the computer unless you set it up: the launcher only talks to the agent on 127.0.0.1, and an engine counts as cloud only if you listed it.

## How takes feed Woodshed

Every judged take is written to `<agent data>/takes/takes.jsonl`, with the winner marked and each take paired with the one it beat or lost to. Woodshed reads that file to turn winning takes into tuned local models; the schema and what it does with the file are in [WOODSHED.md](WOODSHED.md). Here the writer is one small module (`conductor/takes_file.rs`); the thumbs in agent chat add your verdicts in the same format. `author` is `cloud` whenever the text came from an engine you listed in `cloud_engines`, so Woodshed can keep cloud text out of training. Lines are only ever added. Your accept or reject adds a fresh line for each take of that part whose verdict, winner or pair changed, and a reader keeps the last line it sees for a take. The whole history stays. `prompt_ref` and `answer_ref` point at lines of the run log (`runs/<date>/<run>.jsonl#<n>`).
