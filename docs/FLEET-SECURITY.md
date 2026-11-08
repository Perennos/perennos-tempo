# Fleet security, in plain words

This is what protects your computers when they talk to each other, piece by piece, and what a quantum computer would and would not change. It goes with the Fleet section of [TEMPO-SETUP.md](TEMPO-SETUP.md). No hype: where something is young or untested, it says so.

## The short version

- The keys that carry your data are 256 bits. A quantum computer running Grover's algorithm cuts that to about 128 bits of strength, which is still far out of reach. Doubling to "512-bit" would add nothing you need.
- What a quantum computer actually breaks is the elliptic-curve maths: SPAKE2 (the pairing code), Ed25519 (identities) and X25519. That is the weak spot, and it matters most for **pairing**, because someone could record a pairing today and break it later ("harvest now, decrypt later").
- So pairing is **hybrid**. After SPAKE2, the two computers also run **ML-KEM-768** (FIPS 203, the standard post-quantum key exchange), and the key that carries the fleet key comes from both secrets together. An attacker has to break both. There is no pairing without the ML-KEM half.

## Piece by piece

| Piece | Algorithm | Strength today | Against a quantum computer |
| --- | --- | --- | --- |
| Pairing code to a first key | SPAKE2 (Ed25519 group) with a short code | A recording gives nothing to test guesses against; each guess is one live try, three tries burn the code | Broken by Shor's algorithm: a recording could be attacked offline later. It never stands alone (next row) |
| Post-quantum half of pairing | ML-KEM-768 (FIPS 203), sent inside the SPAKE2 channel | NIST security category 3 (roughly the difficulty of breaking a 192-bit AES key) | Built to hold: no known quantum attack. Young, and the Rust crate has not been independently audited, which is why it is mixed with SPAKE2 instead of trusted alone |
| Mixing the two | HKDF-SHA-256 over both secrets and the exchange transcript | 256-bit output; as strong as the stronger half | About 128 bits (Grover). The channel key is safe if either half holds |
| The pairing channel (carries the fleet key and identities) | ChaCha20-Poly1305, 256-bit key, one message per key | 256-bit | About 128 bits (Grover). Fine |
| The fleet key | 32 random bytes from the operating system | 256-bit | About 128 bits (Grover). Fine |
| Every frame's body | ChaCha20-Poly1305, 256-bit key from the fleet key (HKDF, label `frame seal`), a fresh random 96-bit nonce per frame, the header as associated data | 256-bit | About 128 bits (Grover). Fine |
| Every frame's authenticity | HMAC-SHA-256 with a second key from the fleet key (label `frame mac`) | 256-bit key | About 128 bits (Grover). Fine |
| Each computer's identity on frames | Ed25519 signature, pinned at pairing | About 128 bits | Breakable by Shor once a large enough machine exists. A forged frame would also need the fleet key, and a signature is only checked live, so a recording gives nothing away. See "ML-DSA" below |
| Rotating the fleet key (`fleet rotate`) | Your own ssh, with your keys | As strong as your ssh | OpenSSH 9.0 and later use a hybrid post-quantum key exchange by default (`ssh -Q kex` lists yours). An older ssh is only as safe as it is |

## What is encrypted, and what stays readable

Every frame body is sealed with ChaCha20-Poly1305, so a note, a heartbeat, a job, a tool call or a piece of a picture cannot be read by anyone listening on your network. Each frame has its own random 96-bit nonce, carried in the header. The header line (kind, id, who it is from and for, topic, time, replay nonce) is the associated data: it stays readable, because routing needs it, and it cannot be changed without the seal failing. Order of checks on arrival: the HMAC, then the sender's pinned Ed25519 signature, then the seal. A frame that fails to open is dropped and counted (the messenger keeps the number), and its body is never parsed.

Still visible to someone listening: who talks to whom, the topic, when, and how big each frame is. Sizes give away more than people expect (a heartbeat is always the same size; a picture is not). Hiding those would need padding and is not done.

The two keys come from the fleet key by HKDF-SHA-256 with different labels, so one never stands in for the other, and `fleet rotate` changes both at once: a new fleet key gives a new MAC key and a new seal key, with nothing else stored.

A replayed frame still carries the same replay nonce in its header, so it is still refused for the two minutes the clock allows. Honest limit: the seal key is shared by the whole fleet, so a computer holding the fleet key can read frames between two others if it can listen to them. The Ed25519 pinning stops it speaking as them, not hearing them.

## Big files: blob streams (Stage's clips and renders)

A clip going to another computer's Stage, or a render coming back, is a blob stream: 256 KiB pieces, each in its own sealed and signed frame (the same frames, the same keys), written straight from the sender's file to a temp file on the receiver. Nothing is held in memory beyond one 4 MiB batch and nothing goes into SQLite. The receiver hashes the temp file, and only a file with the promised size and SHA-256 is renamed into `<home>/fleet/in/`; a wrong one goes to the trash and the sender is told why. A piece changed on the wire fails its seal and is dropped before anything reads it, and a piece that was changed before it was sealed fails the hash at the end.

- **A cap per file,** `blob_cap_gib` in `fleet.json` (4 GiB unless you set it: `tempo-launch fleet cap GIB`). The receiver turns away a bigger `begin` and a piece past the size it was promised. Each computer keeps its own cap.
- **Who may send.** A paired computer, and only when this computer's owner ran `tempo-launch fleet allow stage`, or when this computer is waiting for that computer's answer to a call it made (an `expect` file, good for six hours). No other peer can fill the disk. At most four files come in at once.
- **No paths from a message.** The `stage` message that follows names a file by the id of a stream already received and checked; the receiving computer never opens a path a message gave.
- **Still visible to someone listening:** the topics (`bs:<id>:<n>`), the sizes and the timing. A clip's size is visible. Padding is not done.
- **Not tested:** a 4 GiB file (the biggest test is 65 MiB), and two real computers.

## ML-DSA: a second signature on frames, not added yet

A post-quantum signature for identities (ML-DSA-65, FIPS 204) was looked at, and a stable pure-Rust crate exists (`ml-dsa` 0.1.x, Apache-2.0 or MIT). It is not on frames yet, for these reasons:

- **The cost is not small.** An ML-DSA-65 signature is 3,309 bytes and its public key 1,952, against 64 and 32 for Ed25519. Most frames are a short note or a heartbeat, so every frame would grow many times over. It also needs a second identity key file per computer, a second pinned key in `fleet.json`, and changes to pairing and to every frame check.
- **Signatures matter when they are checked, not when they are recorded.** A quantum computer that can break Ed25519 can forge a signature only in the future, on frames sent then. It cannot read or alter what was recorded before. And a forged frame would still need the fleet key, which now travels under a post-quantum-protected channel.
- **The crate is young.** Like `ml-kem`, it has had no independent audit.

What would change that: (1) a credible date for a quantum computer that breaks 255-bit elliptic curves, or NIST or your own risk advice moving ML-DSA into "use now" for new designs; or (2) an independent audit of the `ml-dsa` crate; or (3) Tempo carrying frames over networks where a stolen fleet key is a real risk. Any one of them is the cue to add it, as a second signature beside Ed25519, with both required.

## Rotating the fleet key

The fleet key is 256 bits of randomness, so it does not wear out and there is no strength reason to change it on a timer. Rotate it when something happened:

- a computer in the fleet was lost, sold, repaired by someone else or reset;
- someone who had access to one of your computers no longer should;
- you think `fleet.key` was copied.

With none of those, once a year is a sensible habit, and it costs one command:

```
tempo-launch fleet rotate
```

It makes a new key, sends it to each computer over your own ssh, then switches. A computer it cannot reach keeps the old key and drops out until you run `tempo-launch fleet setup NAME` for it. The old key is kept as `fleet.key.old`, and nothing is deleted. A computer you removed from the fleet should be rotated away from straight after, so its copy of the key stops working.

## What was and was not tested

The tests cover frames (the wire never holds the body, a changed body or header does not open, a wrong fleet key gets nothing, a replay is still refused), the pairing round trip with the hybrid, the keys needing both halves, a tampered ML-KEM ciphertext failing closed (nothing is added, no fleet key is handed over), three wrong codes burning the invitation, and rotation. They run on one computer over its own network. They do not prove the post-quantum half holds against a real quantum computer; no one can test that yet, and the claim rests on the standard and on the `ml-kem` crate, which is unaudited.
