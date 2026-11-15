# Packaging

Three ways in, in order of how much you want to think about it.

## One command

```bash
curl -fsSL https://raw.githubusercontent.com/nextime/nexmesh/main/packaging/install.sh | sh
```

Static musl binaries, so the only requirement is a Linux kernel: no libc
version to match and nothing to compile. Add `--role node --hub wss://…` and
it also installs the service for whichever init this machine runs and enables
it at boot. `--dry-run` prints what it would do.

Run from a checkout it uses the binaries already built there, which is what
you want while developing.

## .deb and .rpm

```bash
cargo install cargo-deb cargo-generate-rpm       # once
cargo build --release --workspace
cargo deb -p nexmesh-ctl                         # target/debian/nexmesh_*.deb
cargo generate-rpm -p crates/ctl                 # target/generate-rpm/nexmesh-*.rpm
```

One package, `nexmesh`, with all six binaries, the man page, the CLI
descriptors in `/etc/nexmesh/clis`, the capability manifests, both sets of
service files, and `nexmesh-install` — which is `deploy/install-service.sh`
under a name that is on `PATH`.

**Neither package enables anything.** A machine that installs this may want a
node, or a hub, or neither, and a service that starts itself before anybody
has configured it is one that fails in the logs all night. So installing
prints what to run:

```bash
nexmesh-install hub
nexmesh-install node --user alice
nexmesh-install coderai
```

Each installs the service for the init system that is **actually running** —
systemd units from `/usr/share/nexmesh/systemd`, `/etc/init.d` scripts from
`/usr/share/nexmesh/initd` — enables it at boot, and starts it.

## From source

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install musl-tools          # tree-sitter's grammars are C
export CC_x86_64_unknown_linux_musl=musl-gcc
cargo build --release --workspace --target x86_64-unknown-linux-musl
sh packaging/install.sh --role node
```

Build on the oldest CPU in the fleet, or with the default target CPU: a binary
built with `-C target-cpu=native` on a modern machine dies with `SIGILL` on an
older one, and the hub is often the oldest machine there is.

## What goes where

| | |
|---|---|
| `/usr/bin/nexmesh*` | the six binaries |
| `/usr/bin/nexmesh-install` | the service installer, both init systems |
| `/etc/nexmesh/clis/*.toml` | which agent CLIs this machine knows how to run |
| `/etc/nexmesh/capabilities/*.toml` | capability manifests for the bridge |
| `/etc/nexmesh/*.env` | configuration, 0600, written once and never replaced |
| `/usr/share/nexmesh/{systemd,initd}/` | service files for both init systems |
| `/var/lib/nexmesh/` | the hub's database |
| `/var/log/nexmesh/` | logs, when the service writes them |
