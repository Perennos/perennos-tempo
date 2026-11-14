# Set up the fleet from your main computer

`tempo-launch fleet page` opens an owner-only workbench on `127.0.0.1:18776`.
It reads the native fleet registry. The count includes this computer and admitted
members; discovery and received health are shown separately.

Find computers checks private addresses and offers saved SSH connections. Review
setup reads the exact SSH destination, OS, architecture, user, native home,
existing launcher, fleet fingerprint, and official controls artifact. It makes
no remote writes. Unknown or changed SSH host keys and missing authentication
stay errors for that computer. Perennos Tempo does not ask for passwords or copy
SSH private keys.

Approve setup consumes a server-held review handle once. The target facts are
read again before writing. Decline changes nothing. A tab refresh retains the
operation; polling never starts one. An error needs a new explicit review.

Missing controls are downloaded on the main computer from the exact official
stable release matching its package version. The archive, manifest, regular
member list, target, edition, file sizes and SHA-256 digests must match before
any binary runs. Only the verified control files are transferred over SSH.
There is no full-installer or arbitrary-URL fallback. A release without a
matching controls asset cannot install a new node. Original 2.1.0 releases
predate these controls bundles.

Controls install under a versioned per-user directory. Existing launchers and
full editions are not replaced. Pairing sends the fleet key on SSH input, then
uses a Linux user service, macOS LaunchAgent, or limited Windows sign-in task.
Linux persistence must verify linger; missing authority leaves an explicit
failure. The messenger must answer a challenge signed by the exact peer identity
and then provide fresh received health before the operation says Connected.
Models, model services and learning settings are outside this setup action.

The approved setup starts this computer's messenger for the current session if
needed. It does not silently add a new startup schedule on this computer; verify
its existing sign-in or service configuration separately for unattended use.
An existing launcher can join through the default controller fleet port. A
custom controller port currently requires a newly installed, verified controls
bundle; an existing launcher is refused before remote writes when its custom-port
capability cannot be established.

The workbench token lives in a private native-home file. It reaches the browser
in the opening fragment, which is removed immediately. It is never returned by
an unauthenticated request. HTTP requests require the exact Host and Origin,
JSON, bounded lengths, an absolute deadline and the owner token. The fleet
network endpoint is separate.

The small Updates control reads the existing release cache. Check now checks the
fixed publisher. Automatic checks use the existing opt-in/cache rules. Open
release accepts only an official stable tag page and never runs an installer.

## Partial setup and rollback

Download, transfer, promotion, pairing and persistence errors are separate
stages. Staging files and versioned controls are retained for recovery; setup
does not repeatedly reinstall in the background. Existing controls are never
overwritten. Do not delete a retained directory before checking its exact path
and whether an owned service refers to it.

If fresh signed health does not arrive after pairing, the local fleet entry is
restored only when its enrollment-owned fields still match this operation. A
newly added entry is removed only if it is unchanged. Remote controls and the
service remain for review. Concurrent changes are kept and reported.

An existing Windows task is retried only if its owner, limited interactive
principal, action, wrapper, logon trigger and settings match this setup. It is
not overwritten or started again if already running. Full remote rollback and
real native rollback/Task Scheduler acceptance remain **Untested** in this source
candidate. The native platform matrix and browser acceptance also need
independent review. A control-plane connection is separate from proof that a
model job ran.
