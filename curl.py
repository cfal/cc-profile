#!/usr/bin/env python3
"""Blocking HTTP through the curl binary.

A Python counterpart to curl.js, and it exists for the same reason: the curl on
this PATH is a TLS-impersonating build, so a request made through it presents a
browser's fingerprint rather than a scripting language's HTTP stack. Endpoints
behind a bot filter answer the two very differently.

Differences from curl.js, which streams:
  - Bodies are read to completion, so there is no callback or stream plumbing.
  - Response status and headers come from `-D -` rather than from parsing `-v`
    stderr. Same information, without a protocol state machine.
  - The request body is passed with `--data-binary @-`; curl.js uses
    `--upload-file .` to keep large uploads unbuffered, which a blocking API
    cannot take advantage of.
"""
from __future__ import annotations

import json as _json
import os
import re
import subprocess
import urllib.parse

__all__ = ["CurlError", "Response", "request", "get", "post"]

DEFAULT_BINARY = "curl"
DEFAULT_CONNECT_TIMEOUT = 30

# Only these reach curl. Dropping the rest is the point: http_proxy, ALL_PROXY
# and CURL_* in the ambient environment silently change where a request goes.
_ENV_PASSTHROUGH = ("PATH", "USER", "HOME", "PWD")

# curl supplies these itself. Any the caller did not set are suppressed with an
# empty `-H name:`, so a request carries exactly the headers asked for.
_CURL_DEFAULT_HEADERS = ("accept", "content-length", "content-type",
                         "transfer-encoding")

# Duplicate handling follows Node's http.IncomingMessage.headers, so responses
# read the same as they do through curl.js.
_KEEP_FIRST = frozenset((
    "age", "authorization", "content-length", "content-type", "etag", "expires",
    "from", "host", "if-modified-since", "if-unmodified-since", "last-modified",
    "location", "max-forwards", "proxy-authorization", "referer", "retry-after",
    "server", "user-agent"))

_STATUS_RE = re.compile(r"^(HTTP/[\d.]+)\s+(\d{3})(?:\s+(.*))?$")


class CurlError(Exception):
    """A request that never produced a response."""

    def __init__(self, message: str, *, returncode: int = 0, stderr: str = ""):
        super().__init__(message)
        self.returncode = returncode
        self.stderr = stderr


class Response:
    __slots__ = ("status", "reason", "http_version", "headers", "body", "url")

    def __init__(self, status, reason, http_version, headers, body, url):
        self.status = status
        self.reason = reason
        self.http_version = http_version
        self.headers = headers          # lower-cased names
        self.body = body                # bytes
        self.url = url

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 400

    @property
    def text(self) -> str:
        return self.body.decode("utf-8", "replace")

    def json(self):
        return _json.loads(self.body)

    def __repr__(self) -> str:
        return f"<Response {self.status} {self.reason!r} {len(self.body)}B>"


def _validate_url(url: str) -> str:
    """Reject what curl would treat as a glob rather than a literal URL.

    curl expands {a,b} and [1-9] in URLs, so an unencoded brace or bracket does
    not fetch the address you passed. Refusing beats fetching the wrong thing.
    """
    if any(ch.isspace() for ch in url):
        raise CurlError("URL contains unencoded whitespace; encode it first")
    for ch in "{}":
        if ch in url:
            raise CurlError(f"URL contains unencoded curl glob character {ch!r}")
    parts = urllib.parse.urlsplit(url)
    if not parts.scheme or not parts.netloc:
        raise CurlError(f"not an absolute URL: {url!r}")
    # Brackets are legal in netloc for IPv6 literals, so only the rest is checked.
    for ch in "[]":
        if ch in f"{parts.path}{parts.query}{parts.fragment}":
            raise CurlError(f"URL contains unencoded curl glob character {ch!r}")
    return url


def _build_args(url, method, headers, has_body, opts) -> list:
    args = [
        "--disable",            # ignore curlrc, whatever it might set
        "--silent", "--show-error", "--no-progress-meter",
        "--dump-header", "-",   # response headers ahead of the body on stdout
        "--output", "-",
    ]
    if method != "GET":
        args += ["-X", method]
    if has_body:
        args += ["--data-binary", "@-"]

    args += ["--connect-timeout",
             str(opts.pop("connect_timeout", None) or DEFAULT_CONNECT_TIMEOUT)]

    for name, flag in (("compressed", "--compressed"), ("insecure", "--insecure"),
                       ("proxy_insecure", "--proxy-insecure"),
                       ("proxy_tunnel", "--proxytunnel"), ("http1", "--http1.1"),
                       ("http2", "--http2"), ("http3", "--http3"),
                       ("http2_prior_knowledge", "--http2-prior-knowledge"),
                       ("http2_no_server_push", "--http2-no-server-push"),
                       ("alps", "--alps"), ("tlsv12", "--tlsv1.2"),
                       ("false_start", "--false-start"),
                       ("tls_permute_extensions", "--tls-permute-extensions")):
        if opts.pop(name, False):
            args.append(flag)

    for name, flag in (("cert_compression", "--cert-compression"),
                       ("ciphers", "--ciphers"), ("tls13_ciphers", "--tls13-ciphers"),
                       ("interface", "--interface"), ("proxy", "--proxy"),
                       ("impersonate", "--impersonate")):
        value = opts.pop(name, None)
        if value:
            args += [flag, ",".join(value) if isinstance(value, (list, tuple)) else str(value)]

    for value in opts.pop("resolve", None) or []:
        args += ["--resolve", value]
    args += list(opts.pop("extra_args", None) or [])
    if opts:
        raise CurlError(f"unknown options: {', '.join(sorted(opts))}")

    supplied = set()
    for name, value in (headers or {}).items():
        if name.lower() == "expect":
            # curl would wait for a 100-continue before sending the body.
            raise CurlError("Expect is not supported")
        for item in (value if isinstance(value, (list, tuple)) else [value]):
            args += ["-H", f"{name}: {item}"]
        supplied.add(name.lower())

    if "host" not in supplied:
        args += ["-H", f"Host: {urllib.parse.urlsplit(url).netloc}"]
    args += ["-H", "Expect:"]           # curl adds this to POSTs on its own
    for name in _CURL_DEFAULT_HEADERS:
        if name not in supplied:
            args += ["-H", f"{name}:"]
    return args + [url]


def _add_header(store: dict, name: str, value: str) -> None:
    name = name.lower()
    if name not in store:
        store[name] = [value] if name == "set-cookie" else value
    elif name in _KEEP_FIRST:
        pass
    elif name == "set-cookie":
        store[name].append(value)
    else:
        store[name] += ("; " if name == "cookie" else ", ") + value


def _split_response(raw: bytes, url: str) -> Response:
    """Peel header blocks off the front of stdout; the last one owns the body."""
    status = reason = version = None
    headers: dict = {}
    while True:
        marker = raw.find(b"\r\n\r\n")
        if marker < 0 or not raw.startswith(b"HTTP/"):
            break
        block, raw = raw[:marker].decode("iso-8859-1"), raw[marker + 4:]
        lines = block.split("\r\n")
        match = _STATUS_RE.match(lines[0])
        if not match:
            break
        # A 1xx or proxy CONNECT block is preamble; the next one is the answer.
        version, status, reason = match.group(1), int(match.group(2)), match.group(3) or ""
        headers = {}
        for line in lines[1:]:
            if ":" in line:
                name, _, value = line.partition(":")
                _add_header(headers, name.strip(), value.strip())
        if status >= 200:
            break
    if status is None:
        raise CurlError("no HTTP response header in curl output")
    return Response(status, reason, version, headers, raw, url)


def request(url, method="GET", headers=None, data=None, timeout=60,
            binary=DEFAULT_BINARY, env=None, **opts) -> Response:
    """Perform one request and return the whole response.

    `data` is bytes or str. Unknown keyword options are an error rather than
    being ignored, so a misspelled impersonation flag cannot silently do nothing.
    """
    url = _validate_url(str(url))
    method = method.upper()
    if isinstance(data, str):
        data = data.encode()
    args = _build_args(url, method, headers, data is not None, dict(opts))

    child_env = {k: os.environ[k] for k in _ENV_PASSTHROUGH if k in os.environ}
    child_env.update(env or {})
    try:
        done = subprocess.run([binary, *args], input=data or b"", env=child_env,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=timeout)
    except FileNotFoundError:
        raise CurlError(f"{binary!r} not found on PATH") from None
    except subprocess.TimeoutExpired:
        raise CurlError(f"timed out after {timeout}s") from None

    if done.returncode != 0 and not done.stdout:
        detail = done.stderr.decode("utf-8", "replace").strip().splitlines()
        raise CurlError(f"curl exit {done.returncode}: {detail[-1] if detail else '?'}",
                        returncode=done.returncode,
                        stderr=done.stderr.decode("utf-8", "replace"))
    return _split_response(done.stdout, url)


def get(url, **kw) -> Response:
    return request(url, "GET", **kw)


def post(url, data=None, **kw) -> Response:
    return request(url, "POST", data=data, **kw)


if __name__ == "__main__":
    # Parsing is checked without a network, so a flaky third party cannot make
    # this fail for the wrong reason.
    r = _split_response(b"HTTP/2 429 Too Many\r\nRetry-After: 5\r\n\r\n{}", "u")
    assert (r.status, r.reason, r.ok) == (429, "Too Many", False), r
    assert r.headers["retry-after"] == "5" and r.json() == {}, r.headers

    r = _split_response(b"HTTP/1.1 100 Continue\r\n\r\n"
                        b"HTTP/1.1 201 Created\r\nSet-Cookie: a=1\r\n"
                        b"Set-Cookie: b=2\r\nServer: x\r\nServer: y\r\n\r\nbody", "u")
    assert r.status == 201 and r.body == b"body", r          # 1xx preamble skipped
    assert r.headers["set-cookie"] == ["a=1", "b=2"], r.headers
    assert r.headers["server"] == "x", "duplicates of single-valued headers drop"

    for bad in ("https://x.test/a{b}", "https://x.test/a[1]", "https://x.test/a b",
                "notaurl"):
        try:
            get(bad)
            raise AssertionError(f"accepted {bad!r}")
        except CurlError:
            pass

    assert "--tls-permute-extensions" in _build_args(
        "https://x.test/", "GET", None, False, {"tls_permute_extensions": True})
    try:
        _build_args("https://x.test/", "GET", None, False, {"nope": 1})
        raise AssertionError("accepted an unknown option")
    except CurlError:
        pass

    # One real exchange, to prove the subprocess plumbing and TLS actually work.
    live = get("https://example.com", headers={"User-Agent": "curl.py-selfcheck"})
    assert live.status == 200 and b"Example Domain" in live.body, live
    assert "content-type" in live.headers, live.headers
    sent = post("https://example.com", data=b"x" * 32,
                headers={"Content-Type": "application/octet-stream"})
    assert sent.status, sent                                  # body uploaded, reply parsed
    print("curl.py self-check ok")
