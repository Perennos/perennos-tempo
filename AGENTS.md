# Working on Perennos Tempo

- **Voice and names:** read [VOICE.md](VOICE.md) first. Everything that ships, from a button to a commit message, sounds like a person who built it, and names are short and evocative, with music as one source, not a theme.
- **Privacy:** no personal names, emails, home-network addresses or machine names in anything you commit. Example addresses are `192.168.0.x` or `10.x`. Build fake keys at run time so the leak scan never matches the source.
- **Lean code (Ponytail):** the standard library and the dependencies already here come first. Each crate builds on its own; shared files are copied byte for byte.
- **Checks before you push:** `cargo test --locked`, `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` for every crate you touched, and the leak scan in `.github/workflows/ci.yml`.
- **Safety stays:** nothing is deleted (things go to the trash), risky actions ask first, guests never get tools, and nothing leaves the computer unless the owner set it up.
