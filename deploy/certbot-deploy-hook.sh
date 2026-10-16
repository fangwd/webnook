#!/bin/sh
# certbot deploy hook: copy renewed files where the site can read them and
# reload the certificate without dropping connections.
# Install: /etc/letsencrypt/renewal-hooks/deploy/my-site.sh (mode 0755).
#
# Set EXPECTED_LINEAGE below to this site's certificate directory. Directory
# hooks run for every certificate; unrelated renewals must not replace ours.
# certbot sets RENEWED_LINEAGE after a renewal. To run the hook by hand,
# pass the certificate directory instead:
#   sudo RENEWED_LINEAGE=/etc/letsencrypt/live/example.com /etc/letsencrypt/renewal-hooks/deploy/my-site.sh
set -eu

SRC=${RENEWED_LINEAGE:?set RENEWED_LINEAGE to /etc/letsencrypt/live/<domain>}
EXPECTED_LINEAGE=/etc/letsencrypt/live/example.com
DEST=/etc/my-site/tls

if [ "$SRC" != "$EXPECTED_LINEAGE" ]; then
    printf 'skipping %s (expected %s)\n' "$SRC" "$EXPECTED_LINEAGE" >&2
    exit 0
fi

install -m 0644 -o root -g my-site "$SRC/fullchain.pem" "$DEST/fullchain.pem"
install -m 0640 -o root -g my-site "$SRC/privkey.pem" "$DEST/privkey.pem"

# The initial copy happens before the unit is installed. Reload only an
# active service; surface reload failures instead of silently restarting.
if systemctl is-active --quiet my-site; then
    systemctl reload my-site
fi
