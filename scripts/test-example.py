"""Start the sample app, wait for readiness, run Whirl, and always stop the app."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--update-snapshots", action="store_true")
    args = parser.parse_args()
    reports = ROOT / "whirl-artifacts" / "example"
    reports.mkdir(parents=True, exist_ok=True)
    with (reports / "server.log").open("w") as log:
        server = subprocess.Popen([sys.executable, str(ROOT / "examples/shop/app.py"), "--port", "0"], stdout=subprocess.PIPE, stderr=log, text=True)
        try:
            # The app prints its URL only after it has bound an available port.
            base = server.stdout.readline().strip()
            if not base.startswith("http://127.0.0.1:"):
                raise RuntimeError("Sample app failed to start; see " + str(reports / "server.log"))
            deadline = time.monotonic() + 10
            while True:
                try:
                    with urllib.request.urlopen(base + "/health", timeout=1) as response:
                        if response.status == 200:
                            break
                except (urllib.error.URLError, TimeoutError):
                    if time.monotonic() >= deadline:
                        raise RuntimeError("Sample app readiness timed out")
                    time.sleep(0.1)
            command = [os.environ.get("WHIRL_BIN", str(ROOT / "target/debug/whirl")), "--base", base, "--trace", "--report-json", str(reports / "report.json"), "--report-junit", str(reports / "junit.xml"), "--artifacts", str(reports / "flows")]
            if args.update_snapshots:
                command.append("--update-snapshots")
            command.append(str(ROOT / "examples/shop/flows"))
            return subprocess.run(command, cwd=ROOT, timeout=120).returncode
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()


if __name__ == "__main__":
    sys.exit(main())
