#!/bin/bash
# Issue the fixed HTTPS certificate for zmng on securityserver, signed by Caddy's local root CA.
#
# Why not `tls internal`: Caddy's internal issuer renews the leaf every ~8 hours (12 h lifetime).
# A browser that has not installed the root CA can only "accept" a certificate, and that
# acceptance is for one exact certificate, so it stopped working at every renewal; with the
# offline copy of the page (service worker) the browser never showed its warning again and the
# UI was an empty shell. One certificate that stays the same fixes that: devices with the root
# installed never see a warning, the others accept once.
#
# Validity is 825 days, the longest Apple platforms accept for a TLS server certificate.
# Re-run this script before it expires (then: systemctl reload caddy). Everyone who accepted
# the old certificate by hand is asked once more; devices that trust the root notice nothing.
#
#   sudo ./issue-cert.sh            # issue (keeps the previous pair as *.prev)
#   sudo ./issue-cert.sh --check    # print the current certificate's dates and names
set -euo pipefail
CA=/var/lib/caddy/.local/share/caddy/pki/authorities/local
OUT=/etc/caddy/certs
NAMES="IP:10.10.100.100,DNS:securityserver"
DAYS=825

if [ "${1:-}" = "--check" ]; then
    openssl x509 -in "$OUT/zmng.crt" -noout -subject -issuer -dates -ext subjectAltName
    exit 0
fi
[ -r "$CA/root.key" ] || { echo "run as root: cannot read $CA/root.key" >&2; exit 1; }
umask 077
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
openssl ecparam -name prime256v1 -genkey -noout -out "$tmp/zmng.key"
openssl req -new -key "$tmp/zmng.key" -subj "/CN=zmng securityserver" -out "$tmp/zmng.csr"
cat > "$tmp/ext" <<X
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=$NAMES
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid
X
openssl x509 -req -in "$tmp/zmng.csr" -CA "$CA/root.crt" -CAkey "$CA/root.key" -CAcreateserial -CAserial "$tmp/serial" \
    -days $DAYS -sha256 -extfile "$tmp/ext" -out "$tmp/zmng.crt" 2>/dev/null
openssl verify -CAfile "$CA/root.crt" "$tmp/zmng.crt" >/dev/null
install -d -o root -g caddy -m 750 "$OUT"
for f in zmng.crt zmng.key; do [ -e "$OUT/$f" ] && cp -p "$OUT/$f" "$OUT/$f.prev"; done
install -o root -g caddy -m 640 "$tmp/zmng.key" "$OUT/zmng.key"
install -o root -g caddy -m 644 "$tmp/zmng.crt" "$OUT/zmng.crt"
openssl x509 -in "$OUT/zmng.crt" -noout -subject -issuer -dates -ext subjectAltName
echo "installed in $OUT; now: caddy validate --config /etc/caddy/Caddyfile && systemctl reload caddy"
