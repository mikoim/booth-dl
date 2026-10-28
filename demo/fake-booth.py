#!/usr/bin/env python3
"""A fake BOOTH for recording the demo, so no real account is involved.

usage: python3 demo/fake-booth.py DEMO_HOME PORT

Creates a Firefox profile in DEMO_HOME holding a dummy BOOTH session cookie,
then serves a small library of sample items on 127.0.0.1:PORT. booth-dl talks
to it when BOOTH_DL_BASE_URL=http://127.0.0.1:PORT is set.
"""

import hashlib
import html
import io
import json
import sqlite3
import struct
import sys
import time
import zipfile
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

COOKIE = "_plaza_session_nktz7u"
SESSION = "demo-session"


def png(rgb):
    """A 64x64 solid-color PNG."""
    size = 64
    raw = b"".join(b"\x00" + bytes(rgb) * size for _ in range(size))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    header = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


def sample_zip():
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("README.txt", "Sample 3D model for the booth-dl demo.\n")
        z.writestr("model.obj", "o cube\n" + "v 0 0 0\n" * 2000)
    return buf.getvalue()


SHOPS = {
    "studio": ("Sample Studio", "sample-studio"),
    "records": ("Sample Records", "sample-records"),
}

# item id -> details. Regular files are (downloadable key, file name, content);
# a music track is offered in several formats under one downloadable id.
ITEMS = {
    1001: {
        "title": "Sample Illustration Book",
        "shop": "studio",
        "color": (203, 166, 247),
        "category": "Illustration",
        "tags": ["illustration", "sample"],
        "files": [("2001", "illustrations.pdf", b"%PDF-1.4\n% booth-dl demo\n" + b"0" * 300_000)],
    },
    1002: {
        "title": "Sample 3D Model",
        "shop": "studio",
        "color": (137, 180, 250),
        "category": "3D model",
        "tags": ["3d", "sample"],
        "files": [("2002", "model.zip", sample_zip())],
    },
    1003: {
        "title": "Sample Album",
        "shop": "records",
        "color": (166, 227, 161),
        "category": "Music",
        "tags": ["music", "sample"],
        "track": (
            "2003",
            [
                ("flac", "01-sample-track.flac", b"fLaC" + b"\x00" * 600_000),
                ("mp3", "01-sample-track.mp3", b"ID3" + b"\x00" * 200_000),
            ],
        ),
    },
}

FILES = {}
for item in ITEMS.values():
    for key, _, content in item.get("files", []):
        FILES[key] = content
    if "track" in item:
        downloadable, formats = item["track"]
        for fmt, _, content in formats:
            FILES[f"{downloadable}?type={fmt}"] = content


def library_card(item_id, item):
    shop_name, subdomain = SHOPS[item["shop"]]
    rows = []
    for key, name, _ in item.get("files", []):
        rows.append(
            '<div class="mt-16"><div class="min-w-0 break-words whitespace-pre-line">'
            f'<div class="text-14">{html.escape(name)}</div></div>'
            f'<div><div class="js-download-button" data-href="https://booth.pm/downloadables/{key}" '
            'data-test="downloadable"></div></div></div>'
        )
    if "track" in item:
        downloadable, formats = item["track"]
        options = [
            {
                "text": f"{fmt}でダウンロード",
                "path": f"https://booth.pm/downloadables/{downloadable}?type={fmt}",
                "data": {"filename": name},
            }
            for fmt, name, _ in formats
        ]
        rows.append(
            '<div class="js-download-button" data-test="track-download-button" '
            f'data-dropdown-items="{html.escape(json.dumps(options, ensure_ascii=False), quote=True)}"></div>'
        )
    return (
        '<div class="bg-white p-16"><div class="flex">'
        f'<a href="https://booth.pm/ja/items/{item_id}">'
        f'<img class="l-library-item-thumbnail" src="/images/{item_id}.png"></a><div>'
        f'<a href="https://booth.pm/ja/items/{item_id}"><div class="font-bold">{html.escape(item["title"])}</div></a>'
        f'<a href="https://{subdomain}.booth.pm/"><div class="text-14">{html.escape(shop_name)}</div></a>'
        f'</div></div><div class="mt-16">{"".join(rows)}</div></div>'
    )


def item_json(item_id, item, base):
    shop_name, subdomain = SHOPS[item["shop"]]
    return {
        "id": item_id,
        "name": item["title"],
        "description": f"{item['title']} is a sample item served by the booth-dl demo.",
        "url": f"https://{subdomain}.booth.pm/items/{item_id}",
        "price": "¥ 500",
        "published_at": "2026-10-01T12:00:00.000+09:00",
        "category": {"name": item["category"]},
        "tags": [{"name": tag} for tag in item["tags"]],
        "images": [{"original": f"{base}/images/{item_id}.png"}],
        "shop": {"name": shop_name, "url": f"https://{subdomain}.booth.pm/"},
    }


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, status, body=b"", content_type="text/html; charset=utf-8", headers=()):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        base = f"http://127.0.0.1:{self.server.server_port}"
        path = self.path
        # Presigned file URLs and images need no session, like on BOOTH.
        if path.startswith("/files/"):
            content = FILES.get(path[len("/files/"):])
            if content is None:
                return self.reply(404)
            etag = '"' + hashlib.md5(content).hexdigest() + '"'
            return self.reply(200, content, "application/octet-stream", [("ETag", etag)])
        if path.startswith("/images/"):
            name = path[len("/images/"):].removesuffix(".png")
            item = ITEMS.get(int(name)) if name.isdigit() else None
            return self.reply(200, png(item["color"]), "image/png") if item else self.reply(404)

        if f"{COOKIE}={SESSION}" not in self.headers.get("Cookie", ""):
            return self.reply(302, headers=[("Location", f"{base}/users/sign_in")])
        if path == "/library?page=1":
            cards = "".join(library_card(i, item) for i, item in ITEMS.items())
            return self.reply(200, f"<html><body>{cards}</body></html>".encode())
        if path.startswith("/library"):
            return self.reply(200, b"<html><body></body></html>")
        if path.startswith("/downloadables/"):
            key = path[len("/downloadables/"):]
            if key not in FILES:
                return self.reply(404)
            return self.reply(302, headers=[("Location", f"{base}/files/{key}")])
        if path.startswith("/ja/items/"):
            name = path[len("/ja/items/"):]
            item_id = int(name.removesuffix(".json")) if name.removesuffix(".json").isdigit() else 0
            item = ITEMS.get(item_id)
            if item is None:
                return self.reply(404)
            if name.endswith(".json"):
                body = json.dumps(item_json(item_id, item, base), ensure_ascii=False).encode()
                return self.reply(200, body, "application/json")
            return self.reply(200, f"<html><body><h1>{html.escape(item['title'])}</h1></body></html>".encode())
        return self.reply(404)


def write_firefox_profile(home):
    root = home / ".config" / "mozilla" / "firefox"
    # Never overwrite an existing Firefox setup, e.g. a real one under a real home.
    if root.exists():
        sys.exit(f'{root} already exists; pass a fresh directory such as "$(mktemp -d)" as DEMO_HOME')
    profile = root / "demo.default-release"
    profile.mkdir(parents=True, exist_ok=True)
    (root / "profiles.ini").write_text(
        "[Profile0]\nName=default-release\nIsRelative=1\nPath=demo.default-release\nDefault=1\n\n"
        "[General]\nStartWithLastProfile=1\nVersion=2\n"
    )
    con = sqlite3.connect(profile / "cookies.sqlite")
    con.execute(
        "CREATE TABLE IF NOT EXISTS moz_cookies (id INTEGER PRIMARY KEY, originAttributes TEXT NOT NULL DEFAULT '', "
        "name TEXT, value TEXT, host TEXT, path TEXT, expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER, "
        "isSecure INTEGER, isHttpOnly INTEGER, inBrowserElement INTEGER DEFAULT 0, sameSite INTEGER DEFAULT 0, "
        "schemeMap INTEGER DEFAULT 0, isPartitionedAttributeSet INTEGER DEFAULT 0)"
    )
    now = time.time()
    con.execute(
        "INSERT INTO moz_cookies (name, value, host, path, expiry, lastAccessed, creationTime, isSecure, isHttpOnly) "
        "VALUES (?, ?, '.booth.pm', '/', ?, ?, ?, 1, 1)",
        (COOKIE, SESSION, int(now) + 365 * 24 * 3600, int(now * 1e6), int(now * 1e6)),
    )
    con.commit()
    con.close()


def main():
    home, port = Path(sys.argv[1]), int(sys.argv[2])
    write_firefox_profile(home)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
