#!/usr/bin/env python3
"""Tiny per-host fixture app for the multi-host criterion-2 gate.

Behaviour is a function of the active release's version — the basename of the
`current` symlink fraisier's host-pull artifact adapter activates (exactly as a
real app reads its build):

  * a version containing ``crash`` exits before signalling readiness, so the
    saga's ``systemctl restart`` of that release fails (the restart phase);
  * a version containing ``sick`` serves HTTP 500, so the health probe fails;
  * otherwise it serves 200 with the version as the body.

The unit is ``Type=notify``: a start only succeeds once the socket is bound and
readiness is signalled, so a crash build is a genuine restart failure.
"""

import os
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

active = sys.argv[1] if len(sys.argv) > 1 else "/var/lib/app/current"
port = int(sys.argv[2]) if len(sys.argv) > 2 else 8080

try:
    version = os.path.basename(os.readlink(active))
except OSError:
    version = "none"

if "crash" in version:
    sys.stderr.write(f"app: crash build {version!r} — exiting before readiness\n")
    sys.exit(1)


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 (http.server API)
        if "sick" in version:
            self.send_response(500)
            self.end_headers()
            self.wfile.write(b"unhealthy")
        else:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(version.encode())

    def log_message(self, *args):  # silence per-request logging
        pass


server = HTTPServer(("0.0.0.0", port), Handler)
# Signal readiness once the socket is bound (NotifyAccess=all lets this subprocess
# notify on the unit's behalf).
subprocess.run(["systemd-notify", "--ready"], check=False)
sys.stderr.write(f"app: {version!r} serving on :{port}\n")
server.serve_forever()
