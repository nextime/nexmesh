#!/bin/sh
# Install a nexmesh service so it comes back by itself after a reboot.
#
# Starting something by hand is how a fleet works until the first power cut.
# This installs the service file the machine's own init system wants --
# systemd units on a systemd host, /etc/init.d scripts on a sysvinit one --
# ENABLES it at boot, and starts it. Both sets of files are in this
# directory and neither is generated: what is installed is what you can read
# here first.
#
#   sudo deploy/install-service.sh hub
#   sudo deploy/install-service.sh node            # for the invoking user
#   sudo deploy/install-service.sh node --user alice
#   sudo deploy/install-service.sh coderai
#   deploy/install-service.sh node --dry-run       # print, change nothing
#
# It is idempotent: run it again after an upgrade and it re-installs the
# service file, keeps your /etc/nexmesh/*.env untouched, and restarts.
set -eu
# update-rc.d, rc-update and start-stop-daemon live in sbin, which is not on
# every PATH even under sudo.
PATH="/usr/sbin:/sbin:$PATH"
export PATH

HERE=$(cd "$(dirname "$0")" && pwd)
WHAT=""
USER_NAME=""
DRY=0
START=1

usage() {
    cat <<USAGE
usage: $0 {hub|node|coderai} [--user NAME] [--no-start] [--dry-run]

  hub       the fleet's state and the only thing that decides what runs where
  node      spawns and drives agent CLIs on this machine, as one login
  coderai   the capability bridge (OCR, speech, images), beside its service

  --user NAME   which login the node runs as (default: \$SUDO_USER, else you)
  --no-start    install and enable at boot, but do not start it now
  --dry-run     print what would happen and change nothing
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        hub|node|coderai) WHAT="$1" ;;
        --user) shift; USER_NAME="${1:-}" ;;
        --user=*) USER_NAME="${1#--user=}" ;;
        --no-start) START=0 ;;
        --dry-run|-n) DRY=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "$0: unknown argument '$1'" >&2; usage >&2; exit 2 ;;
    esac
    shift
done
[ -n "$WHAT" ] || { usage >&2; exit 2; }

run() {
    if [ "$DRY" = 1 ]; then
        printf '  %s\n' "$*"
    else
        "$@"
    fi
}

# Which init system is actually running, not which one is installed: a Devuan
# box can have systemd packages present and pid 1 be sysvinit.
if [ -d /run/systemd/system ]; then
    INIT=systemd
elif [ -x /sbin/openrc-run ] || [ -x /usr/sbin/openrc-run ]; then
    INIT=openrc
else
    INIT=sysv
fi

if [ "$WHAT" = node ] && [ -z "$USER_NAME" ]; then
    USER_NAME="${SUDO_USER:-$(id -un)}"
fi
if [ "$WHAT" = node ] && [ "$USER_NAME" = root ]; then
    echo "$0: a node must not run as root -- it spawns agent CLIs, which should" >&2
    echo "    use a real user's own tool configuration. Pass --user NAME." >&2
    exit 1
fi

CONF=/etc/nexmesh
case "$WHAT" in
    hub)     ENVFILE="$CONF/hub.env" ;;
    coderai) ENVFILE="$CONF/coderai.env" ;;
    node)    ENVFILE="$CONF/node-$USER_NAME.env" ;;
esac

echo "nexmesh: installing the $WHAT service for $INIT"
if [ "$DRY" = 1 ]; then
    echo "(dry run: nothing below is actually done)"
fi

run mkdir -p "$CONF" /var/log/nexmesh /var/lib/nexmesh

# The environment file is the configuration, and it holds a token: it is
# created empty-but-commented if missing, never overwritten, and 0600.
if [ ! -f "$ENVFILE" ]; then
    echo "  writing a starting point at $ENVFILE (edit it before starting)"
    if [ "$DRY" = 0 ]; then
        case "$WHAT" in
            hub) cat > "$ENVFILE" <<'ENV'
# nexmesh hub. 0600 -- it names the database and may hold TLS paths.
NEXMESH_DB=/var/lib/nexmesh/hub.db
# The editable skills and role templates this fleet can change from the
# dashboard. Seeded with the built-ins at install; edits here win over them,
# and removing the directory reverts to the compiled-in defaults.
NEXMESH_PROMPTS_DIR=/var/lib/nexmesh/prompts
# Never 0.0.0.0 on a machine with a public address: this API starts
# processes. One listen for the loopback (nginx), one for the LAN.
NEXMESH_HUB_ARGS=--listen 127.0.0.1:7870
ENV
            ;;
            node) cat > "$ENVFILE" <<ENV
# nexmesh node. 0600 -- NEXMESH_TOKEN is worth a shell on this machine.
NEXMESH_NODE_USER=$USER_NAME
NEXMESH_HUB=wss://hub.example.net/v1/node/ws
NEXMESH_TOKEN=
NEXMESH_NAME=$(hostname -s)
NEXMESH_CLIS=/etc/nexmesh/clis
NEXMESH_SOCK=/run/nexmesh/node-$USER_NAME.sock
NEXMESH_MCPD=/usr/local/bin/nexmesh-mcpd
# NEXMESH_CA_CERT=/etc/nexmesh/pki/ca.pem
# NEXMESH_CLIENT_CERT=/etc/nexmesh/pki/node.pem
# NEXMESH_CLIENT_KEY=/etc/nexmesh/pki/node.key
ENV
            ;;
            coderai) cat > "$ENVFILE" <<'ENV'
# nexmesh capability bridge. 0600 -- it holds the media service's token.
NEXMESH_HUB_HTTP=https://hub.example.net
NEXMESH_PARTICIPANT_TOKEN=
CODERAI_URL=http://127.0.0.1:8000
CODERAI_TOKEN=
NEXMESH_SESSION=all
# NEXMESH_CA_CERT=/etc/nexmesh/pki/ca.pem
ENV
            ;;
        esac
        chmod 600 "$ENVFILE"
    fi
else
    echo "  keeping $ENVFILE as it is"
fi

# Make the editable prompts store for a hub -- the DIRECTORY only, not its
# contents. The built-in skills and templates are served from the binary and
# ship with it, so they are never copied in: that way a binary upgrade that
# improves a built-in reaches the fleet, where a seeded copy would have
# shadowed it forever. A file appears here only when the operator edits one
# (through the dashboard), which is exactly the thing that SHOULD shadow the
# built-in. The hub defaults to this path when it exists, so creating it is
# all the configuration an install needs.
#
# (`nexmesh-hub seed-prompts <dir>` still exists to fork the built-ins into
# files on purpose -- but those forks then stop tracking upgrades, so it is
# not run automatically.)
if [ "$WHAT" = hub ]; then
    run mkdir -p /var/lib/nexmesh/prompts/skills /var/lib/nexmesh/prompts/roles
fi

case "$INIT" in
    systemd)
        case "$WHAT" in
            hub)     SRC="$HERE/systemd/nexmesh-hub.service";     UNIT=nexmesh-hub.service ;;
            coderai) SRC="$HERE/systemd/nexmesh-coderai.service"; UNIT=nexmesh-coderai.service ;;
            node)    SRC="$HERE/systemd/nexmesh-node@.service";   UNIT="nexmesh-node@$USER_NAME.service" ;;
        esac
        TARGET=/etc/systemd/system/$(basename "$SRC")
        run install -Dm644 "$SRC" "$TARGET"
        run systemctl daemon-reload
        # `enable` is the whole point of this script: it is what makes the
        # service come back after a reboot.
        run systemctl enable "$UNIT"
        if [ "$START" = 1 ]; then
            run systemctl restart "$UNIT"
        fi
        echo "done. status:  systemctl status $UNIT"
        ;;
    sysv|openrc)
        case "$WHAT" in
            hub)     SRC="$HERE/initd/nexmesh-hub";     NAME=nexmesh-hub ;;
            coderai) SRC="$HERE/initd/nexmesh-coderai"; NAME=nexmesh-coderai ;;
            # One script per login, named after it, because the script reads
            # its own filename to find its environment file.
            node)    SRC="$HERE/initd/nexmesh-node";    NAME="nexmesh-node-$USER_NAME" ;;
        esac
        run install -Dm755 "$SRC" "/etc/init.d/$NAME"
        if [ "$WHAT" = node ]; then
            # The generic script expects /etc/nexmesh/node-<instance>.env,
            # which is exactly what was written above.
            echo "  /etc/init.d/$NAME reads $ENVFILE"
        fi
        if command -v update-rc.d >/dev/null 2>&1; then
            run update-rc.d "$NAME" defaults
        elif command -v rc-update >/dev/null 2>&1; then
            run rc-update add "$NAME" default
        elif command -v chkconfig >/dev/null 2>&1; then
            run chkconfig --add "$NAME"
        else
            echo "  no update-rc.d, rc-update or chkconfig: link it into your runlevel by hand" >&2
        fi
        if [ "$START" = 1 ]; then
            run "/etc/init.d/$NAME" restart
        fi
        echo "done. status:  /etc/init.d/$NAME status"
        ;;
esac

if [ "$WHAT" = node ] && [ "$DRY" = 0 ]; then
    grep -q '^NEXMESH_TOKEN=.' "$ENVFILE" 2>/dev/null || cat <<'NOTE'

It will not connect until it has a token. On the hub:

    nexmesh-hub enrol <this machine's name>

then put it in NEXMESH_TOKEN= in the environment file above and start it again.
NOTE
fi
