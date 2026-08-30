#!/usr/bin/env python3
"""
Lightweight Mock OCI Registry v2 for local testing without Docker.
Supports basic OCI distribution spec:
- GET /v2/
- GET /token
- POST /v2/<repo>/nix-cache/blobs/uploads/
- PUT /v2/<repo>/nix-cache/blobs/uploads/<upload_id>?digest=<digest>
- HEAD /v2/<repo>/nix-cache/blobs/<digest>
- GET /v2/<repo>/nix-cache/blobs/<digest>
- PUT /v2/<repo>/nix-cache/manifests/<tag>
- GET /v2/<repo>/nix-cache/manifests/<tag>
"""

import hashlib
import http.server
import json
import os
import socketserver
import sys
import uuid
from urllib.parse import parse_qs, urlparse

STORAGE_DIR = "/tmp/mock-oci-registry"
os.makedirs(os.path.join(STORAGE_DIR, "blobs"), exist_ok=True)
os.makedirs(os.path.join(STORAGE_DIR, "manifests"), exist_ok=True)
os.makedirs(os.path.join(STORAGE_DIR, "uploads"), exist_ok=True)


class MockRegistryHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        # Silent or print to stderr
        sys.stderr.write("%s - - [%s] %s\n" % (self.address_string(), self.log_date_time_string(), format % args))

    def check_fault_injection(self):
        fault_status = int(os.environ.get("MOCK_FAULT_STATUS", "0"))
        if fault_status > 0 and self.path != "/v2/" and self.path != "/v2":
            self.send_response(fault_status)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"Injected Registry Fault\n")
            return True
        return False

    def do_GET(self):
        if self.check_fault_injection():
            return
        parsed = urlparse(self.path)
        path = parsed.path

        if path == "/v2/" or path == "/v2":
            self.send_response(200)
            self.send_header("Docker-Distribution-Api-Version", "registry/2.0")
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b"{}")
            return

        if path.startswith("/token"):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"token": "mock-token-12345"}).encode())
            return

        # GET blob: /v2/<repo>/nix-cache/blobs/<digest>
        if "/blobs/" in path:
            digest = path.split("/blobs/")[-1]
            safe_name = digest.replace(":", "_")
            blob_file = os.path.join(STORAGE_DIR, "blobs", safe_name)
            if os.path.exists(blob_file):
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(os.path.getsize(blob_file)))
                self.send_header("Docker-Content-Digest", digest)
                self.end_headers()
                with open(blob_file, "rb") as f:
                    self.wfile.write(f.read())
            else:
                self.send_response(404)
                self.end_headers()
            return

        # GET manifest: /v2/<repo>/nix-cache/manifests/<tag>
        if "/manifests/" in path:
            tag = path.split("/manifests/")[-1]
            safe_tag = tag.replace(":", "_")
            manifest_file = os.path.join(STORAGE_DIR, "manifests", tag)
            if not os.path.exists(manifest_file):
                manifest_file = os.path.join(STORAGE_DIR, "manifests", safe_tag)

            if os.path.exists(manifest_file):
                with open(manifest_file, "rb") as f:
                    content = f.read()
                digest = f"sha256:{hashlib.sha256(content).hexdigest()}"
                self.send_response(200)
                self.send_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
                self.send_header("Content-Length", str(len(content)))
                self.send_header("Docker-Content-Digest", digest)
                self.send_header("ETag", f'"{digest}"')
                self.end_headers()
                self.wfile.write(content)
            else:
                self.send_response(404)
                self.end_headers()
            return

        self.send_response(404)
        self.end_headers()

    def do_HEAD(self):
        if self.check_fault_injection():
            return
        parsed = urlparse(self.path)
        path = parsed.path

        if "/blobs/" in path:
            digest = path.split("/blobs/")[-1]
            safe_name = digest.replace(":", "_")
            blob_file = os.path.join(STORAGE_DIR, "blobs", safe_name)
            if os.path.exists(blob_file):
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(os.path.getsize(blob_file)))
                self.send_header("Docker-Content-Digest", digest)
                self.end_headers()
            else:
                self.send_response(404)
                self.end_headers()
            return

        if "/manifests/" in path:
            tag = path.split("/manifests/")[-1]
            safe_tag = tag.replace(":", "_")
            manifest_file = os.path.join(STORAGE_DIR, "manifests", tag)
            if not os.path.exists(manifest_file):
                manifest_file = os.path.join(STORAGE_DIR, "manifests", safe_tag)

            if os.path.exists(manifest_file):
                with open(manifest_file, "rb") as f:
                    content = f.read()
                digest = f"sha256:{hashlib.sha256(content).hexdigest()}"
                self.send_response(200)
                self.send_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
                self.send_header("Content-Length", str(len(content)))
                self.send_header("Docker-Content-Digest", digest)
                self.send_header("ETag", f'"{digest}"')
                self.end_headers()
            else:
                self.send_response(404)
                self.end_headers()
            return

        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        if self.check_fault_injection():
            return
        parsed = urlparse(self.path)
        path = parsed.path
        query = parse_qs(parsed.query)

        # POST /v2/<repo>/nix-cache/blobs/uploads/
        if "/blobs/uploads" in path:
            digest = query.get("digest", [None])[0]
            if digest:
                length = int(self.headers.get("Content-Length", 0))
                body = self.rfile.read(length)
                safe_name = digest.replace(":", "_")
                blob_file = os.path.join(STORAGE_DIR, "blobs", safe_name)
                with open(blob_file, "wb") as f:
                    f.write(body)
                self.send_response(201)
                self.send_header("Location", f"/v2/blobs/{digest}")
                self.send_header("Docker-Content-Digest", digest)
                self.end_headers()
                return

            upload_id = str(uuid.uuid4())
            location = f"{path.rstrip('/')}/{upload_id}"
            upload_file = os.path.join(STORAGE_DIR, "uploads", upload_id)
            open(upload_file, "wb").close()

            self.send_response(202)
            self.send_header("Location", location)
            self.send_header("Range", "0-0")
            self.send_header("Docker-Upload-UUID", upload_id)
            self.end_headers()
            return

        self.send_response(404)
        self.end_headers()

    def do_PATCH(self):
        if self.check_fault_injection():
            return
        parsed = urlparse(self.path)
        path = parsed.path

        # PATCH blob upload: /v2/<repo>/nix-cache/blobs/uploads/<upload_id>
        if "/blobs/uploads/" in path:
            upload_id = path.split("/blobs/uploads/")[-1].split("?")[0]
            upload_file = os.path.join(STORAGE_DIR, "uploads", upload_id)
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length)

            with open(upload_file, "ab") as f:
                f.write(body)

            total_size = os.path.getsize(upload_file)
            end_range = max(0, total_size - 1)

            self.send_response(202)
            self.send_header("Location", path)
            self.send_header("Range", f"0-{end_range}")
            self.send_header("Docker-Upload-UUID", upload_id)
            self.end_headers()
            return

        self.send_response(404)
        self.end_headers()

    def do_PUT(self):
        if self.check_fault_injection():
            return
        parsed = urlparse(self.path)
        path = parsed.path
        query = parse_qs(parsed.query)

        # PUT blob upload: /v2/<repo>/nix-cache/blobs/uploads/<upload_id>?digest=<digest>
        if "/blobs/uploads/" in path:
            digest = query.get("digest", [None])[0]
            if not digest:
                self.send_response(400)
                self.end_headers()
                return

            upload_id = path.split("/blobs/uploads/")[-1].split("?")[0]
            upload_file = os.path.join(STORAGE_DIR, "uploads", upload_id)

            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length)

            safe_name = digest.replace(":", "_")
            blob_file = os.path.join(STORAGE_DIR, "blobs", safe_name)

            if os.path.exists(upload_file):
                if body:
                    with open(upload_file, "ab") as f:
                        f.write(body)
                os.replace(upload_file, blob_file)
            else:
                with open(blob_file, "wb") as f:
                    f.write(body)

            self.send_response(201)
            self.send_header("Location", f"/v2/blobs/{digest}")
            self.send_header("Docker-Content-Digest", digest)
            self.end_headers()
            return

        # PUT manifest: /v2/<repo>/nix-cache/manifests/<tag>
        if "/manifests/" in path:
            tag = path.split("/manifests/")[-1]
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length)

            # Validate referenced blobs exist in storage (compliance with OCI Distribution Spec)
            try:
                manifest_json = json.loads(body.decode("utf-8"))
                if isinstance(manifest_json, dict):
                    if "config" in manifest_json and isinstance(manifest_json["config"], dict):
                        cfg_digest = manifest_json["config"].get("digest")
                        if cfg_digest:
                            cfg_file = os.path.join(STORAGE_DIR, "blobs", cfg_digest.replace(":", "_"))
                            if not os.path.exists(cfg_file):
                                self.send_response(400)
                                self.send_header("Content-Type", "application/json")
                                self.end_headers()
                                self.wfile.write(json.dumps({
                                    "errors": [{
                                        "code": "BLOB_UNKNOWN",
                                        "message": f"blob unknown to registry: {cfg_digest}",
                                        "detail": cfg_digest
                                    }]
                                }).encode())
                                return
                    if "layers" in manifest_json and isinstance(manifest_json["layers"], list):
                        for layer in manifest_json["layers"]:
                            if isinstance(layer, dict):
                                l_digest = layer.get("digest")
                                if l_digest:
                                    l_file = os.path.join(STORAGE_DIR, "blobs", l_digest.replace(":", "_"))
                                    if not os.path.exists(l_file):
                                        self.send_response(400)
                                        self.send_header("Content-Type", "application/json")
                                        self.end_headers()
                                        self.wfile.write(json.dumps({
                                            "errors": [{
                                                "code": "BLOB_UNKNOWN",
                                                "message": f"blob unknown to registry: {l_digest}",
                                                "detail": l_digest
                                            }]
                                        }).encode())
                                        return
            except Exception:
                pass

            manifest_file = os.path.join(STORAGE_DIR, "manifests", tag)

            # CAS optimistic concurrency check via If-Match header
            if_match = self.headers.get("If-Match")
            if if_match:
                if_match = if_match.strip('"')
                if os.path.exists(manifest_file):
                    with open(manifest_file, "rb") as f:
                        existing_content = f.read()
                    existing_digest = f"sha256:{hashlib.sha256(existing_content).hexdigest()}"
                    if if_match != existing_digest and if_match != f'"{existing_digest}"':
                        self.send_response(412)
                        self.send_header("Content-Type", "text/plain")
                        self.end_headers()
                        self.wfile.write(b"Precondition Failed: CAS digest mismatch\n")
                        return

            computed_digest = f"sha256:{hashlib.sha256(body).hexdigest()}"
            safe_digest = computed_digest.replace(":", "_")

            with open(manifest_file, "wb") as f:
                f.write(body)
            with open(os.path.join(STORAGE_DIR, "manifests", safe_digest), "wb") as f:
                f.write(body)
            with open(os.path.join(STORAGE_DIR, "manifests", computed_digest), "wb") as f:
                f.write(body)

            self.send_response(201)
            self.send_header("Docker-Content-Digest", computed_digest)
            self.send_header("ETag", f'"{computed_digest}"')
            self.end_headers()
            return

        self.send_response(404)
        self.end_headers()

    def do_DELETE(self):
        if self.check_fault_injection():
            return
        parsed = urlparse(self.path)
        path = parsed.path

        # DELETE manifest: /v2/<repo>/nix-cache/manifests/<tag>
        if "/manifests/" in path:
            tag = path.split("/manifests/")[-1]
            safe_tag = tag.replace(":", "_")
            manifest_file = os.path.join(STORAGE_DIR, "manifests", tag)
            safe_manifest_file = os.path.join(STORAGE_DIR, "manifests", safe_tag)
            deleted = False
            if os.path.exists(manifest_file):
                os.remove(manifest_file)
                deleted = True
            if os.path.exists(safe_manifest_file):
                try:
                    os.remove(safe_manifest_file)
                    deleted = True
                except OSError:
                    pass

            if deleted:
                self.send_response(202)
                self.end_headers()
            else:
                self.send_response(404)
                self.end_headers()
            return

        # DELETE blob: /v2/<repo>/nix-cache/blobs/<digest>
        if "/blobs/" in path:
            digest = path.split("/blobs/")[-1]
            safe_name = digest.replace(":", "_")
            blob_file = os.path.join(STORAGE_DIR, "blobs", safe_name)
            if os.path.exists(blob_file):
                os.remove(blob_file)
                self.send_response(202)
                self.end_headers()
            else:
                self.send_response(404)
                self.end_headers()
            return

        self.send_response(404)
        self.end_headers()


def run_server(port):
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("127.0.0.1", port), MockRegistryHandler) as httpd:
        sys.stderr.write(f"Mock OCI Registry running on http://127.0.0.1:{port}\n")
        httpd.serve_forever()


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 5002
    run_server(port)
