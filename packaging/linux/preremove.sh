#!/bin/sh
# Removing the package (not upgrading it): stop RealTimeX Link and remove what the post-install
# script installed. Each user's data (~/.config/realtimex-link) stays.
set -e
case "$1" in
  remove | purge | 0) ;;
  *) exit 0 ;;
esac
for name in rtx-link rtx-linkd rtx-control; do
  pkill -f "^/opt/RealTimeX Link/$name" 2>/dev/null || true
done
rm -rf "/opt/RealTimeX Link"
rm -f /usr/share/applications/realtimex-link.desktop
rm -f /usr/share/icons/hicolor/*/apps/realtimex-link.png
if [ -f /usr/share/icons/hicolor/icon-theme.cache ]; then
  gtk-update-icon-cache -q -f /usr/share/icons/hicolor 2>/dev/null || true
fi
