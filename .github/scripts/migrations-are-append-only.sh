#!/usr/bin/env bash
# Fails if a migration that may already have been applied has had its bytes changed.
#
# sqlx records the sha384 of each migration's bytes in `_sqlx_migrations` and compares it
# on every run, so editing an applied migration is not a small thing: `sqlx migrate run`
# refuses with "previously applied but has been modified", and this suite's own
# `check_migrations` refuses to *start* the service. There is no code fix — the files have
# to say what a fresh install should get — so the only repair is to re-stamp the checksums
# by hand (crates/ds-vault/SPEC.md, "Roles and migrations").
#
# That is a thing to find in review rather than in a deployment, and it was once found the
# other way round: a rename rewrote `ds_app` inside all eleven migrations and nothing
# noticed until an existing database refused to migrate.
#
#   migrations-are-append-only.sh [BASE_REF]   (default: origin/main)
#
# What passes: a new migration, which is the point of the directory, and a pure rename of
# a file or its directory — sqlx keys a migration on the version number in its filename
# and checksums only its contents, so moving it changes nothing. What fails: edited or
# deleted contents, including a one-word comment, because the checksum is over the bytes
# and cannot tell a comment from a column.
set -euo pipefail

BASE=${1:-origin/main}

if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
    echo "cannot resolve $BASE; pass the base ref as an argument (CI needs fetch-depth: 0)" >&2
    exit 1
fi

# No pathspec: it would hide a rename whose other half is outside it, which is exactly
# what a crate rename looks like. `-M` so a move is reported as one line with a
# similarity score — R100 is a pure move and allowed, anything less changed the bytes.
# Three dots: what this branch did, not what the base has done since they diverged.
offenders=$(
    git diff -M --name-status --diff-filter=MDR "$BASE...HEAD" |
        awk -F'\t' '
            { target = (NF > 2 ? $3 : $2) }
            target !~ /migrations\/.*\.sql$/ { next }
            $1 == "R100" { next }
            { print $1 "\t" target }
        '
)

if [ -z "$offenders" ]; then
    echo "migrations are append-only against $BASE"
    exit 0
fi

echo "A migration that may already have been applied has changed:" >&2
echo "$offenders" >&2
cat >&2 <<'MSG'

sqlx checksums a migration's bytes, so this makes every existing database refuse both
`sqlx migrate run` and `check_migrations` at startup. Add a new migration instead. If the
change is genuinely unavoidable, say so in the pull request and give operators the
re-stamp from crates/ds-vault/SPEC.md.
MSG
exit 1
