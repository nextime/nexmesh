# Deploying nexmesh

The shape this is written for, because it is the one it was built against:

```
          nexmesh.example.net  (203.0.113.10, machine-a wan0)
                    │
                 nginx  ── terminates public TLS, verifies client certs
                    │
  machine-a ──── nexmesh-hub  --listen 127.0.0.1:7870
  (Devuan,                    --listen 10.0.0.10:7870
   no AVX)                    --trusted-proxy 127.0.0.1
                    │
       ┌────────────┼────────────┐
   machine-b    machine-c    machine-d     ← nodes, over the LAN address
   (a GPU box)  (a web host) (a laptop)
       │            │
 nexmesh-coderai   an external participant
```

Two addresses on the hub on purpose. Loopback is for nginx; the LAN address is
for the machines. **Not `0.0.0.0`** — machine-a has public addresses on `wan0`,
so a wildcard bind would put an API that starts processes on the internet.

## 1. Binaries

Built static against musl, generic CPU. machine-a is a Celeron J1900 with **no
AVX**: a binary built with `-C target-cpu=native` on any other machine here
dies with `SIGILL` on it.

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt-get install musl-tools          # tree-sitter's grammars are C
export CC_x86_64_unknown_linux_musl=musl-gcc
cargo build --release --workspace --target x86_64-unknown-linux-musl
```

Or take the tarball from a GitHub release, which is built exactly this way.

```bash
for h in machine-a machine-b machine-d; do
  scp target/x86_64-unknown-linux-musl/release/{nexmesh,nexmesh-hub,nexmesh-node,nexmesh-mcpd,nexmesh-orch,nexmesh-coderai} root@$h:/usr/local/bin/
  ssh root@$h 'mkdir -p /etc/nexmesh /var/lib/nexmesh /var/log/nexmesh'
  scp -r clis capabilities root@$h:/etc/nexmesh/
done
```

## 2. The fleet CA, on the hub

```bash
ssh root@machine-a
nexmesh-hub --db /var/lib/nexmesh/hub.db pki init \
    --dir /var/lib/nexmesh/pki \
    --host machine-a --host 10.0.0.10 --host 127.0.0.1 \
    --host nexmesh.example.net
# one client certificate per machine, and one for nginx
for n in machine-b machine-d machine-c nginx-proxy operator; do
  nexmesh-hub --db /var/lib/nexmesh/hub.db pki issue "$n" \
      --dir /var/lib/nexmesh/pki --out /var/lib/nexmesh/pki/issued/"$n"
done
```

The CA key never leaves machine-a. Each machine gets `ca-cert.pem` plus its own
certificate and key, all 0600.

## 3. The hub

`/etc/nexmesh/hub.env`, mode 0600:

```sh
NEXMESH_DB=/var/lib/nexmesh/hub.db
NEXMESH_PKI=/var/lib/nexmesh/pki
NEXMESH_TLS_CERT=/var/lib/nexmesh/pki/server-cert.pem
NEXMESH_TLS_KEY=/var/lib/nexmesh/pki/server-key.pem
NEXMESH_CLIENT_CA=/var/lib/nexmesh/pki/ca-cert.pem
# The pairing listener takes no client certificate -- a machine being paired
# has none yet -- so it stays on the LAN and never on wan0.
NEXMESH_PAIR_LISTEN=10.0.0.10:7871
NEXMESH_EXTRA="--listen 127.0.0.1:7870 --listen 10.0.0.10:7870 --trusted-proxy 127.0.0.1"
```

```bash
install -m 755 deploy/initd/nexmesh-hub /etc/init.d/nexmesh-hub
adduser --system --group --no-create-home nexmesh
update-rc.d nexmesh-hub defaults          # start at boot
service nexmesh-hub start
```

Then the first credentials:

```bash
nexmesh-hub --db /var/lib/nexmesh/hub.db dashboard-setup-code   # for the browser
for n in machine-b machine-d machine-c; do
  nexmesh-hub --db /var/lib/nexmesh/hub.db enrol "$n"           # one token each
done
```

A node token is worth a shell on that machine. Put it straight into the
machine's own 0600 config and nowhere else.

## 4. nginx, for the public name

See `deploy/nginx/`. The hub is on loopback for this, and
`--trusted-proxy 127.0.0.1` is what makes the hub believe nginx's verdict
about a client certificate. Without that flag the headers are ignored and the
binding is simply not enforced; with it, **the hub must not be reachable
except through nginx**, which is why it binds loopback and the LAN rather
than `wan0`.

## 5. Nodes

`/etc/nexmesh/node.env` on each machine, mode 0600:

```sh
NEXMESH_HUB=wss://10.0.0.10:7870
NEXMESH_NAME=machine-b                       # or machine-d, machine-c
NEXMESH_TOKEN=<the enrol token for this machine>
NEXMESH_CLIS=/etc/nexmesh/clis
NEXMESH_CA_CERT=/etc/nexmesh/pki/ca-cert.pem
NEXMESH_CLIENT_CERT=/etc/nexmesh/pki/machine-b-cert.pem
NEXMESH_CLIENT_KEY=/etc/nexmesh/pki/machine-b-key.pem
NEXMESH_NODE_USER=agentuser                # it spawns agent CLIs as this user
```

The client key must be **owned by the user the node runs as**. A root-owned
0600 key is unreadable to it, and the node then retries for ever with
`reading client key ...: Permission denied` — which is honest, and easy to
miss if you are not reading its log:

```bash
install -o "$NEXMESH_NODE_USER" -m 600 pki/<name>-key.pem /etc/nexmesh/pki/
install -o "$NEXMESH_NODE_USER" -m 644 pki/<name>-cert.pem /etc/nexmesh/pki/
chown "$NEXMESH_NODE_USER" /etc/nexmesh/node.env
```

That user is the one whose agent CLIs the node will spawn, so it differs per
host: `agentuser` on most, but on a host where the CLI belongs to another
account, that account. The node reports what it found at startup
(`detected: claude, codex, opencode, tmux`), which is the quickest way to tell
you picked the right one.

```bash
install -m 755 deploy/initd/nexmesh-node /etc/init.d/nexmesh-node
update-rc.d nexmesh-node defaults
service nexmesh-node start
nexmesh nodes                            # from anywhere, to confirm
```

`wss://` matters: a node refuses to send its enrolment token in cleartext to
a remote host, and that refusal is deliberate.

## 6. The capability bridge, on machine-b

machine-b has the GPU and the media service, so the bridge belongs there — a local
connection, and the service token never crosses the network.

```bash
nexmesh-hub --db /var/lib/nexmesh/hub.db participant-token bridge --session <id>
```

`/etc/nexmesh/coderai.env`, mode 0600:

```sh
NEXMESH_HUB=https://10.0.0.10:7870
NEXMESH_PARTICIPANT_TOKEN=<participant token>
CODERAI_URL=http://127.0.0.1:8000
CODERAI_TOKEN=<created in that service's own Admin -> Tokens>
NEXMESH_CAPABILITIES=/etc/nexmesh/capabilities/coderai.toml
```

```bash
nexmesh-coderai --probe-only --service http://127.0.0.1:8000   # check first
install -m 755 deploy/initd/nexmesh-coderai /etc/init.d/nexmesh-coderai
update-rc.d nexmesh-coderai defaults
service nexmesh-coderai start
```

## 7. hermes on machine-d, as a participant

hermes is not an agent CLI and has no node to spawn it, which is exactly what
the participant API is for. Mint it a token and point it at the hub:

```bash
nexmesh-hub --db /var/lib/nexmesh/hub.db participant-token hermes --session <id>
# then, from hermes:
POST https://10.0.0.10:7870/v1/participant/join
     {"name": "Hermes", "role": "archivist", "kind_hint": "hermes"}
```

Or, if it speaks MCP, point it at `/v1/mcp` instead and skip the API
entirely.

## Checking it

```bash
nexmesh nodes                       # all three, connected
service nexmesh-hub status
curl -sk https://10.0.0.10:7870/healthz
```

Logs are in `/var/log/nexmesh/`. Every service is `service <name> status` and
starts at boot through `update-rc.d`.
