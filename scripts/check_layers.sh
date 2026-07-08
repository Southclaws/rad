#!/bin/sh
# Enforces the layer rule: a package under rad/NN_* may only import rad
# packages from the same or a lower-numbered layer. cmd/, e2e, and the root
# doc package sit above all layers and are exempt.
set -eu

cd "$(dirname "$0")/.."

go list -f '{{.ImportPath}} {{join .Imports " "}}' ./... | awk '
function layer(path) {
    if (path !~ /^rad\/rad\/[0-9][0-9]_/) return -1
    return substr(path, 9, 2) + 0
}
{
    from = layer($1)
    if (from < 0) next  # cmd/, e2e, root: above all layers
    for (i = 2; i <= NF; i++) {
        if ($i !~ /^rad\//) continue
        to = layer($i)
        if (to > from) {
            printf "layer violation: %s (layer %02d) imports %s (layer %02d)\n", $1, from, $i, to
            bad = 1
        }
    }
}
END { exit bad }
'
echo "layer check ok"
