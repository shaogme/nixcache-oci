#!/usr/bin/env python3
"""
OCI Registry Manager for local integration testing.
Runs a real containerized OCI Distribution Registry via podman or docker.
"""

import atexit
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.request

DEFAULT_PORT = 5002
DEFAULT_STORAGE_DIR = "/tmp/nixcache-test-registry"
IMAGE_NAME = "registry:2"
PULL_IMAGE_NAME = "docker.io/library/registry:2"


def find_engine():
    """Detect an available and working container engine (podman or docker)."""
    env_engine = os.environ.get("CONTAINER_ENGINE")
    candidates = [env_engine] if env_engine else ["podman", "docker"]

    for candidate in candidates:
        if not candidate:
            continue
        path = shutil.which(candidate)
        if not path:
            continue
        try:
            res = subprocess.run(
                [candidate, "ps"],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=5,
            )
            if res.returncode == 0:
                return candidate
        except Exception:
            continue

    raise RuntimeError("Neither podman nor docker is available and operational on this system.")


def ensure_image(engine):
    """Ensure the registry:2 image is available locally."""
    try:
        res = subprocess.run(
            [engine, "images", "-q", IMAGE_NAME],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
        )
        if res.returncode == 0 and res.stdout.strip():
            return
    except Exception:
        pass

    sys.stderr.write(f">>> Pulling {PULL_IMAGE_NAME} using {engine}...\n")
    pull_cmd = [engine, "pull", PULL_IMAGE_NAME]
    subprocess.run(pull_cmd, check=True)

    try:
        subprocess.run([engine, "tag", PULL_IMAGE_NAME, IMAGE_NAME], check=False)
    except Exception:
        pass


def get_container_name(port):
    return f"nixcache-registry-{port}"


def stop_container(engine, container_name):
    """Force remove existing container if any."""
    try:
        subprocess.run(
            [engine, "rm", "-f", container_name],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except Exception:
        pass


def wait_for_ready(port, timeout=20):
    """Poll http://127.0.0.1:{port}/v2/ until ready."""
    url = f"http://127.0.0.1:{port}/v2/"
    start = time.time()
    while time.time() - start < timeout:
        try:
            req = urllib.request.Request(url, method="GET")
            with urllib.request.urlopen(req, timeout=1) as resp:
                if resp.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(0.3)
    return False


def run_server(port, storage_dir=None):
    engine = find_engine()
    ensure_image(engine)

    if not storage_dir:
        storage_dir = os.environ.get("REGISTRY_STORAGE_DIR", DEFAULT_STORAGE_DIR)

    os.makedirs(storage_dir, exist_ok=True)

    container_name = get_container_name(port)
    stop_container(engine, container_name)

    run_cmd = [
        engine, "run", "-d",
        "--name", container_name,
        "--net=host",
        "-e", f"REGISTRY_HTTP_ADDR=0.0.0.0:{port}",
        "-e", "REGISTRY_STORAGE_DELETE_ENABLED=true",
        "-v", f"{storage_dir}:/var/lib/registry",
    ]

    if "podman" in engine:
        run_cmd.insert(3, "--cgroups=disabled")

    run_cmd.append(IMAGE_NAME)

    sys.stderr.write(f">>> Launching {container_name} via {engine}...\n")
    subprocess.run(run_cmd, check=True)

    cleaned_up = False

    def cleanup(*args):
        nonlocal cleaned_up
        if not cleaned_up:
            cleaned_up = True
            sys.stderr.write(f"\n>>> Stopping container {container_name}...\n")
            stop_container(engine, container_name)

    signal.signal(signal.SIGINT, lambda s, f: sys.exit(0))
    signal.signal(signal.SIGTERM, lambda s, f: sys.exit(0))
    if hasattr(signal, "SIGHUP"):
        signal.signal(signal.SIGHUP, lambda s, f: sys.exit(0))
    atexit.register(cleanup)

    if not wait_for_ready(port):
        sys.stderr.write(f"!!! Timed out waiting for registry on port {port}\n")
        cleanup()
        sys.exit(1)

    sys.stderr.write(f"OCI Registry (container: {container_name}) running on http://127.0.0.1:{port} via {engine}\n")

    try:
        while True:
            time.sleep(1)
    except (KeyboardInterrupt, SystemExit):
        pass
    finally:
        cleanup()


def main():
    if len(sys.argv) > 1 and sys.argv[1] in ("stop", "rm"):
        port = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_PORT
        try:
            engine = find_engine()
            stop_container(engine, get_container_name(port))
            print(f"Stopped registry on port {port}")
        except Exception as e:
            sys.stderr.write(f"Error stopping container: {e}\n")
        return

    port = DEFAULT_PORT
    storage_dir = None

    args = sys.argv[1:]
    if args and args[0] == "start":
        args = args[1:]

    if args:
        try:
            port = int(args[0])
        except ValueError:
            pass
        if len(args) > 1:
            storage_dir = args[1]

    run_server(port, storage_dir)


if __name__ == "__main__":
    main()
