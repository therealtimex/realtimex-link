#!/bin/sh
# Install (or update) RealTimeX Link in /opt/RealTimeX Link with its own installer: it never
# downgrades, stops a running older Link first and writes the applications-menu entry.
set -e
/usr/lib/realtimex-link/payload/rtx-linkd install "/opt/RealTimeX Link" || {
  echo "RealTimeX Link: install into /opt/RealTimeX Link failed" >&2
  exit 1
}
