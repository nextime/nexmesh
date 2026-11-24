#!/usr/bin/env python3
"""Move an existing CLI conversation into a seat's new per-seat store.

Run this ON THE NODE that holds the seat, once, before the seat next spawns.

WHY IT EXISTS
-------------
A CLI that keeps every seat's conversations in one shared store, and names
none of them, cannot be resumed per seat -- so nexmesh can give such a seat a
store of its own (`[session_store]` in the CLI's descriptor). The first time
that is switched on, the conversation a seat is ALREADY on lives in the
shared store and would be left behind: the seat comes back with nothing.

This copies the store's conversation data into the seat's own directory, so
the conversation is there to be resumed. Pair it with pinning the seat's
resume handle to that conversation:

    POST /v1/slots/<slot>/resume-handle  {"handle": "<conversation id>"}

which makes the seat's FIRST spawn resume it explicitly rather than relying
on "the last one" -- the shared store's copy still holds other seats'
conversations, so "last" is not yet trustworthy inside it. After that spawn
the seat's own activity is the most recent in its own store and the ordinary
`resume_latest_args` path is correct.

WHAT IT DOES NOT DO
-------------------
It does not decide WHICH conversation belongs to which seat. That is not
derivable from the store -- seats share a user, a directory and a start time
-- so the conversation id is an argument, and getting it wrong resumes
somebody else's work. Identify it first and pass it in.

Nothing is deleted. The shared store is only read.

    migrate-cli-store.py --cli codex --slot sl-123 \
        --root ~/.codex --copy 'state_*.sqlite*' --copy sessions \
        [--store-rel ''] [--state-root ~/.nexmesh/cli-state] --apply
"""
import argparse, glob, os, shutil, sqlite3, sys

# Files that are SQLite databases, copied through SQLite rather than by bytes.
DB_SUFFIXES = (".db", ".sqlite")


def is_db(path):
    return any(path.endswith(s) for s in DB_SUFFIXES)


def sidecar(path):
    """A WAL or SHM file, which must not be copied on its own."""
    return any(path.endswith(s + x) for s in DB_SUFFIXES for x in ("-wal", "-shm"))


def copy_db(src, to):
    """A CONSISTENT copy of a live database.

    The CLI whose store this is is still running and still writing. Copying
    the file and its -wal by bytes races those writes and can land a torn
    database -- which would present as a seat that resumes into a corrupt
    conversation, discovered later and hard to attribute. SQLite's own backup
    takes a consistent snapshot of a database being written to, and folds the
    WAL in, so no sidecar files are copied at all.
    """
    one = sqlite3.connect(f"file:{src}?mode=ro", uri=True)
    two = sqlite3.connect(to)
    try:
        one.backup(two)
    finally:
        two.close()
        one.close()


def expand(p):
    return os.path.expanduser(p)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cli", required=True, help="cli id, as the descriptor names it")
    ap.add_argument("--slot", required=True, help="the seat's slot id")
    ap.add_argument("--root", required=True, help="the CLI's shared store, e.g. ~/.codex")
    ap.add_argument("--copy", action="append", default=[],
                    help="a name or glob inside the store to copy (repeatable)")
    ap.add_argument("--store-rel", default="",
                    help="subdirectory the CLI appends to its store env var")
    ap.add_argument("--state-root", default="~/.nexmesh/cli-state",
                    help="where nexmesh keeps per-seat stores (NEXMESH_CLI_STATE)")
    ap.add_argument("--apply", action="store_true",
                    help="actually copy; without it, only say what would happen")
    a = ap.parse_args()

    root = expand(a.root)
    if not os.path.isdir(root):
        print(f"ERROR: no shared store at {root}")
        return 2
    base = os.path.join(expand(a.state_root), a.cli, a.slot)
    dest = os.path.join(base, a.store_rel) if a.store_rel else base

    print(f"cli         {a.cli}")
    print(f"seat        {a.slot}")
    print(f"from        {root}")
    print(f"to          {dest}")
    print(f"mode        {'APPLY' if a.apply else 'dry run (nothing is written)'}")
    print()

    # What exists to copy, resolved before anything is written so the plan and
    # the action cannot disagree.
    items = []
    for pattern in a.copy:
        hits = sorted(glob.glob(os.path.join(root, pattern)))
        if not hits:
            print(f"  (nothing matches {pattern!r} -- skipped)")
            continue
        items.extend(hits)
    if not items:
        print("ERROR: nothing to copy; check --copy patterns")
        return 2

    total = 0
    for src in items:
        name = os.path.basename(src)
        size = 0
        if os.path.isdir(src):
            for dirpath, _, files in os.walk(src):
                for f in files:
                    try: size += os.path.getsize(os.path.join(dirpath, f))
                    except OSError: pass
        else:
            try: size = os.path.getsize(src)
            except OSError: pass
        total += size
        kind = ("dir " if os.path.isdir(src)
                else "wal " if sidecar(src)
                else "db  " if is_db(src) else "file")
        note = "  (folded into its database's snapshot)" if sidecar(src) else ""
        print(f"  {kind}  {name:34} {size/1048576:8.2f} MB{note}")
    print(f"  {'':40} {total/1048576:8.2f} MB total")
    print()

    if not a.apply:
        print("Dry run. Re-run with --apply to copy.")
        return 0

    os.makedirs(dest, exist_ok=True)
    for src in items:
        name = os.path.basename(src)
        to = os.path.join(dest, name)
        if sidecar(src):
            # Folded into the database's own snapshot; copying it separately
            # is how a consistent copy is made inconsistent again.
            print(f"  skipped {name} (its database is copied as a snapshot)")
            continue
        if os.path.exists(to):
            print(f"  SKIP {name}: already in the seat's store")
            continue
        if os.path.isdir(src):
            shutil.copytree(src, to, symlinks=True)
            print(f"  copied {name}")
        elif is_db(src):
            copy_db(src, to)
            print(f"  snapshotted {name} (consistent copy of a live database)")
        else:
            shutil.copy2(src, to)
            print(f"  copied {name}")
    print()
    print(f"Done. {dest}")
    print("Now pin the conversation, or the first spawn will not know which to "
          "continue:\n"
          f"  POST /v1/slots/{a.slot}/resume-handle  {{\"handle\": \"<conversation id>\"}}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
