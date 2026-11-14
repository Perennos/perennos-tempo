# Update checks

The current 2.1.0 launcher has no update checker. Install the first release that includes this feature through the usual installer once; after that, Tempo can tell you when a newer stable release is available.

Checks start off. Turn them on with `tempo-launch updates --enable`, or leave them off and run `tempo-launch updates --check` when you want. `tempo-launch updates` shows the saved status. Enabled launchers check at most once every 24 hours in a hidden background process; startup does not wait for the network. The status is also shown on the Welcome and Fleet pages.

Tempo asks the official GitHub latest-release API for the Perennos Tempo repository. It accepts only a stable `vMAJOR.MINOR.PATCH` release whose page matches that official repository. The check uses an installed `curl` and has an 11-second timeout and a 1 MiB response cap. A failed check keeps the last known release and records the failure in `<Tempo home>/updates.json`.

`tempo-launch updates --open` opens the cached official release notes. It does not download or run an installer. Install updates through the usual release process.
