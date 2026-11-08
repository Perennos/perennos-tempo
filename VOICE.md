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

## Music is the house style

The product is called Tempo. Music is the guide for every name, workflow and little touch, going forward: Tempo is a band that plays on your own machines. Pick the music word that makes the feature obvious, and never one that confuses.

| In music | In Tempo |
| --- | --- |
| The tempo | the whole suite, keeping time |
| Studio | sound: mixing, mastering, analysis |
| Room | the private room where people and AI members talk |
| Soundcheck | code and security review before you go live |
| Conductor | sets up your bands and cues each part on the right engine |
| Band, members, instruments | the team of AI members for a workflow |
| Encore | run it again on what changed |
| Rehearsal | the dry run, the tests |
| Setlist | a flow's steps, in order |
| Fleet | the machines in one field |

How to name something new:

1. Find the music word for what it does. A good one makes the feature obvious ("Soundcheck" is checking before the show).
2. Keep the plain meaning next to it the first time: "All clear. Ready to play. (Ready)".
3. Commands stay short and clear: `tempo-launch conductor run`, not a pun. The wink is in the words around it.
4. If no music word fits cleanly, use the plain word. Clarity beats a stretched metaphor.

## Small things that matter

- Headings: an italic serif line with a small numbered label above it ("01 — What this computer can run").
- Status lines read like a crew member talking: "Made on the studio box, 6.2 s."
- Easter eggs are rare, kind and never in machine output (JSON, SARIF, logs).
- Comments in code explain *why*, in a sentence a person would say.
- Commit messages say what changed for the person using it.
- Code is lean: standard library and the dependencies we already have first (the Ponytail rule). Nothing you don't need.

When in doubt, read it out loud. If it sounds like a brochure, rewrite it.
