"""Real SO_PEERCRED boundary, for CI running as root in a disposable container."""
import os
from pathlib import Path
import socket
import unittest

import test_service


@unittest.skipUnless(os.geteuid() == 0 and test_service.DAEMON.is_file(),
                     "requires a built daemon and two UIDs in the test namespace")
class PeerCredentials(unittest.TestCase):
    def test_foreign_peer_still_refused_if_socket_permissions_are_relaxed(self):
        service = test_service.Service("test_web_authenticates_reads_and_mutations")
        self.addCleanup(service.doCleanups)
        service.setUp()
        sock = service.root / "run/cc-profile/ccpd.sock"
        # Use a directory FD so the test never changes permissions above its fixture.
        directory = os.open(sock.parent, os.O_RDONLY | os.O_DIRECTORY)
        self.addCleanup(os.close, directory)
        sock.chmod(0o666)
        sock.parent.chmod(0o711)
        pid = os.fork()
        if pid == 0:
            try:
                os.setgroups([])
                os.setgid(65534)
                os.setuid(65534)
                path = Path("/proc/self/fd") / str(directory) / sock.name
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.settimeout(2)
                    client.connect(str(path))
                    client.sendall(b"POST /v3/launch HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
                    status = client.makefile("rb").readline()
                os._exit(0 if status.startswith(b"HTTP/1.1 403 ") else 1)
            except OSError:
                os._exit(2)
        self.assertEqual(os.waitpid(pid, 0)[1], 0)
        self.assertEqual(service.api("GET", "startups")[1]["items"], [])
