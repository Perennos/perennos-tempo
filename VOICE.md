# How Perennos Tempo talks

Read this before you write a word for Tempo: a button, an error, a commit, a doc, a release note. It applies to people and to AI alike.

## Sound like a person who built it

Write the way the founder talks to a friend he respects: direct, warm, a bit cheeky, sure of himself, never stiff. Say the thing. If it fits in five words, use five.

- **Plain words.** "Tempo checks the file before it runs it", not "Tempo performs integrity verification".
- **Talk to one person.** "you" and "your computer". Never "users".
- **Say what happens next.** A message ends with what to do: "Ollama isn't answering at 127.0.0.1:11434. Start it, or run `tempo-launch setup --apply`."
- **Confident, not loud.** No exclamation marks in the product. Let the work be impressive.
- **A little play, never a joke at the user's expense.** One wink per screen, at most.
- **Honest.** If something hasn't been tested, say so. If it might download 4 GB, say 4 GB.

## Words we don't use

robust, leverage, seamless (in copy), comprehensive, utilize, empower, solution, synergy, cutting-edge, best-in-class, "users", "simply", "just" (when it hides effort), "ledger", "claim", "open source" (say **source-available**, or "free to use, not to resell"). No personal names, emails or home-network details, ever.

## Names

Tempo is for many kinds of work, so names stay wide. A good name is short, evocative and makes the feature obvious, with a little character, like a fashion house naming a line rather than an engineer naming a module. Music is one good source (it's where Tempo, Studio, Soundcheck and the Conductor came from), but it isn't a rule: don't force a theme onto everything.

What we have so far:

| Name | What it is |
| --- | --- |
| Tempo | the whole suite |
| Vault | your notes and second brain |
| Studio | sound: mixing, mastering, analysis |
| Room | the private room where people and AI members talk |
| Soundcheck | code and security review before you ship |
| Conductor | sets up your bands of AI members and runs each part on the right engine |
| Fleet | your machines, working as one |

How to name something new:

1. Say what it does in plain words first. Then find a word that makes that obvious and has some character.
2. Keep the plain meaning next to it the first time: "All clear. Ready to play. (Ready)".
3. Commands stay short and clear (`tempo-launch conductor run`); the character is in the words around them.
4. If nothing fits cleanly, use the plain word. Clarity beats a clever name.

## Small things that matter

- Headings: an italic serif line with a small numbered label above it ("01 — What this computer can run").
- Status lines read like a person talking: "Made on the studio box, 6.2 s."
- Easter eggs are rare, kind and never in machine output (JSON, SARIF, logs).
- Comments in code explain *why*, in a sentence a person would say.
- Commit messages say what changed for the person using it.
- Code is lean: standard library and the dependencies we already have first (the Ponytail rule). Nothing you don't need.

When in doubt, read it out loud. If it sounds like a brochure, rewrite it.
