#!/bin/sh
# Copyright 2026 The ros2ci authors
# SPDX-License-Identifier: Apache-2.0

# Makes a private certificate authority and a certificate for the
# coordinator, with openssl, in the directory certs beside this script.
#
#   ./make-certs.sh <name or address> [<name or address> ...]
#
# Give every name and address that workers will use to reach the
# coordinator. The name "coordinator", which the worker container in
# compose.yaml uses, is always included.
#
#   certs/ca.pem       give this to every worker (--ca-file)
#   certs/ca.key       keep this private; it can sign more certificates
#   certs/server.pem   the coordinator's certificate
#   certs/server.key   the coordinator's private key
#
# An existing authority is kept, so running this again only replaces the
# coordinator's certificate and workers keep working.

set -eu

if [ "$#" -lt 1 ]; then
    echo "usage: $0 <name or address> [<name or address> ...]" >&2
    exit 64
fi

umask 077
dir=$(dirname "$0")/certs
mkdir -p "$dir"
# Only this user may look inside. The three files that containers read are
# mounted one by one, which does not depend on the directory's mode.
chmod 700 "$dir"

names="DNS:coordinator"
for name in "$@"; do
    case "$name" in
        *[!0-9.]*) kind=DNS ;;
        *) kind=IP ;;
    esac
    case "$name" in
        *:*) kind=IP ;;
    esac
    names="$names,$kind:$name"
done

if [ ! -f "$dir/ca.key" ]; then
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
        -keyout "$dir/ca.key" -out "$dir/ca.pem" -days 3650 \
        -subj "/CN=ros2ci private authority" \
        -addext "basicConstraints=critical,CA:TRUE,pathlen:0" \
        -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null
fi

openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout "$dir/server.key" -out "$dir/server.csr" \
    -subj "/CN=ros2ci coordinator" 2>/dev/null

extensions=$(mktemp)
trap 'rm -f "$extensions" "$dir/server.csr"' EXIT
cat > "$extensions" <<EOF
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=$names
EOF

openssl x509 -req -in "$dir/server.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" \
    -CAcreateserial -out "$dir/server.pem" -days 825 -extfile "$extensions" 2>/dev/null

# The coordinator's container runs as its own user and has to read these.
# The directory's mode keeps other users of this machine out, and the
# authority's key stays private to this user.
chmod 644 "$dir/ca.pem" "$dir/server.pem" "$dir/server.key"

echo "wrote $dir/ca.pem, $dir/server.pem and $dir/server.key for: $names"
