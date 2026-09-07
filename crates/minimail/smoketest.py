#!/usr/bin/env python3
# minimail smoke test: drives a REAL SMTP conversation (smtplib) against the sink,
# then asserts the captured mail through the JSON API (urllib). Stdlib only, no pip.
# Assumes an already-running server; does NOT start or stop it. See `just smoke`.

import base64
import json
import os
import smtplib
import sys
import time
import urllib.error
import urllib.parse
import urllib.request


def _env(name, default):
    return os.environ.get(name, default)


SMTP_HOST = _env("MINIMAIL_SMTP_HOST", "127.0.0.1")
SMTP_PORT = int(_env("MINIMAIL_SMTP_PORT", "1025"))
HTTP_HOST = _env("MINIMAIL_HTTP_HOST", "127.0.0.1")
HTTP_PORT = int(_env("MINIMAIL_HTTP_PORT", "8025"))
USER = _env("MINIMAIL_USER", "minimail")
PASSWORD = _env("MINIMAIL_PASSWORD", "minimail")

# argv overrides so `just smoke` can point at a non-default server:
#   smoketest.py [--smtp HOST:PORT] [--http HOST:PORT] [--user U] [--password P]
_a = sys.argv[1:]
_i = 0
while _i < len(_a):
    _flag = _a[_i]
    _val = _a[_i + 1] if _i + 1 < len(_a) else ""
    if _flag == "--smtp":
        SMTP_HOST, _, _p = _val.partition(":")
        SMTP_PORT = int(_p) if _p else SMTP_PORT
        _i += 2
    elif _flag == "--http":
        HTTP_HOST, _, _p = _val.partition(":")
        HTTP_PORT = int(_p) if _p else HTTP_PORT
        _i += 2
    elif _flag == "--user":
        USER = _val
        _i += 2
    elif _flag == "--password":
        PASSWORD = _val
        _i += 2
    else:
        print(f"unknown arg: {_flag}")
        sys.exit(2)

API_BASE = f"http://{HTTP_HOST}:{HTTP_PORT}"
BASIC = base64.b64encode(f"{USER}:{PASSWORD}".encode()).decode()
TAG = f"{os.getpid()}-{int(time.time())}"

passed = 0
failed = 0


def t(label, fn):
    global passed, failed
    try:
        r = fn()
        print(f"OK   {label}: {r!r}"[:200])
        passed += 1
    except Exception as e:
        print(f"FAIL {label}: {type(e).__name__}: {e}")
        failed += 1


# ---- HTTP helpers (all API routes are Basic-auth guarded except /healthz) ----


def http(method, path, body=None, extra=None):
    req = urllib.request.Request(API_BASE + path, data=body, method=method)
    req.add_header("Authorization", "Basic " + BASIC)
    for k, v in (extra or {}).items():
        req.add_header(k, v)
    try:
        r = urllib.request.urlopen(req, timeout=15)
        return r.getcode(), r.read(), r.headers
    except urllib.error.HTTPError as e:
        return e.code, e.read(), e.headers


def get_json(path):
    code, body, _ = http("GET", path)
    assert code == 200, f"GET {path} -> {code}"
    return json.loads(body)


def detail(mid):
    return get_json(f"/api/v1/messages/{mid}")


def get_raw(mid):
    code, body, _ = http("GET", f"/api/v1/messages/{mid}/raw")
    assert code == 200, f"raw -> {code}"
    return body


def download_part(mid, part_id):
    q = urllib.parse.quote(part_id, safe="")
    code, body, _ = http("GET", f"/api/v1/messages/{mid}/parts/{q}?download=1")
    assert code == 200, f"part -> {code}"
    return body


# ---- SMTP helpers ----

_seq = 0


def next_from():
    global _seq
    _seq += 1
    return f"probe{_seq}-{TAG}@smoke.test"


def wire(text):
    # Normalize to CRLF and guarantee a trailing CRLF so the stored .eml is byte-exact.
    if not text.endswith("\n"):
        text += "\n"
    return text.replace("\r\n", "\n").replace("\n", "\r\n").encode("utf-8")


def smtp_send(raw, mail_from, rcpt_to, mech="plain"):
    s = smtplib.SMTP(SMTP_HOST, SMTP_PORT, timeout=15)
    try:
        s.ehlo("smoke.local")
        s.user = USER
        s.password = PASSWORD
        if mech == "login":
            # bare "AUTH LOGIN" challenge flow (no initial response), per SPEC §6.
            s.auth("LOGIN", s.auth_login, initial_response_ok=False)
        else:
            s.auth("PLAIN", s.auth_plain)
        s.sendmail(mail_from, rcpt_to, raw)
    finally:
        try:
            s.quit()
        except Exception:
            pass


def smtp_auth_code(password):
    # Returns 235 on success or the SMTP error code (e.g. 535) on rejection.
    s = smtplib.SMTP(SMTP_HOST, SMTP_PORT, timeout=15)
    try:
        s.ehlo("smoke.local")
        s.user = USER
        s.password = password
        try:
            s.auth("PLAIN", s.auth_plain)
            return 235
        except smtplib.SMTPAuthenticationError as e:
            return e.smtp_code
    finally:
        try:
            s.quit()
        except Exception:
            pass


def find_by_from(mail_from, tries=40):
    # Store writes synchronously before the 250 reply; a short retry covers any lag.
    for _ in range(tries):
        data = get_json("/api/v1/messages?limit=500&offset=0")
        for m in data["messages"]:
            if m.get("from") == mail_from:
                return m
        time.sleep(0.05)
    raise AssertionError(f"message from {mail_from} not found in list")


# ---- checks ----


def check_healthz():
    code, body, _ = http("GET", "/healthz")
    assert code == 200, code
    assert body.strip() == b"ok", body
    return body.decode().strip()


def check_purge_existing():
    code, body, _ = http("DELETE", "/api/v1/messages")
    assert code == 200, code
    return json.loads(body).get("deleted")


def check_plain():
    mf = next_from()
    to = "bob@smoke.test"
    subj = f"Plain hello {TAG}"
    msg = wire(
        f"""From: Alice <{mf}>
To: Bob <{to}>
Subject: {subj}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8

Hello, smoketest body line.
Second line.
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    # list-level assertions (GET /api/v1/messages)
    assert m["subject"] == subj, m["subject"]
    assert m["from"] == mf, m["from"]
    assert m["to"] == [to], m["to"]
    assert mf in m["from_header"], m["from_header"]
    # detail-route assertions
    d = detail(m["id"])
    assert d["summary"]["subject"] == subj
    assert "Hello, smoketest body line." in (d["text"] or ""), d["text"]
    hdrs = {k.lower(): v for k, v in d["headers"]}
    assert hdrs.get("subject") == subj, hdrs.get("subject")
    return m["id"]


def check_multipart_alt():
    mf = next_from()
    to = "bob@smoke.test"
    b = "ALTBOUND"
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: alt {TAG}
MIME-Version: 1.0
Content-Type: multipart/alternative; boundary="{b}"

--{b}
Content-Type: text/plain; charset=utf-8

PLAINPART {TAG}
--{b}
Content-Type: text/html; charset=utf-8

<html><body><p>HTMLPART {TAG}</p></body></html>
--{b}--
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    assert m["has_text"] and m["has_html"], m
    d = detail(m["id"])
    assert d["text"] and f"PLAINPART {TAG}" in d["text"], d["text"]
    assert d["html"] and f"HTMLPART {TAG}" in d["html"], d["html"]
    return "text+html decoded"


def check_binary_attachment():
    # THE regression test for the boundary-CRLF bug: bytes must round-trip byte-exact.
    mf = next_from()
    to = "bob@smoke.test"
    blob = bytes(range(256))  # 256 bytes, \x00..\xff
    enc = base64.b64encode(blob).decode()
    wrapped = "\n".join(enc[i : i + 76] for i in range(0, len(enc), 76))
    bnd = "MIXEDBOUND"
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: attach {TAG}
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary="{bnd}"

--{bnd}
Content-Type: text/plain; charset=utf-8

see attachment {TAG}
--{bnd}
Content-Type: application/octet-stream; name="blob.bin"
Content-Transfer-Encoding: base64
Content-Disposition: attachment; filename="blob.bin"

{wrapped}
--{bnd}--
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    atts = m["attachments"]
    assert len(atts) == 1, atts
    a = atts[0]
    assert a["filename"] == "blob.bin", a
    assert a["content_type"] == "application/octet-stream", a
    assert a["size"] == 256, a
    got = download_part(m["id"], a["part_id"])
    assert len(got) == 256, f"got {len(got)} bytes"
    assert got == blob, "attachment bytes are NOT byte-identical to what was sent"
    return f"{len(got)} bytes byte-identical"


def check_encoded_subject():
    mf = next_from()
    to = "bob@smoke.test"
    raw_subj = f"Grüße ☀ {TAG}"  # "Grüße ☀ <tag>"
    enc = base64.b64encode(raw_subj.encode("utf-8")).decode()
    subj_hdr = f"=?utf-8?B?{enc}?="
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: {subj_hdr}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8

body {TAG}
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    assert m["subject"] == raw_subj, repr(m["subject"])
    return m["subject"]


def check_dot_stuffing():
    mf = next_from()
    to = "bob@smoke.test"
    dotline = f".leading dot survives {TAG}"
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: dot {TAG}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8

first line
{dotline}
last line
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    d = detail(m["id"])
    lines = (d["text"] or "").replace("\r\n", "\n").split("\n")
    assert dotline in lines, lines
    # raw must have the single (un-stuffed) dot, not the wire-doubled "..".
    raw = get_raw(m["id"])
    assert dotline.encode() in raw
    assert ("." + dotline).encode() not in raw
    return "leading dot round-tripped"


def check_quoted_printable():
    mf = next_from()
    to = "bob@smoke.test"
    # =E2=82=AC -> EUR sign, =C3=A9 -> "é", trailing "=" is a soft break joining next line.
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: qp {TAG}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8
Content-Transfer-Encoding: quoted-printable

Price 10=E2=82=AC for a caf=C3=A9=
 today {TAG}
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    d = detail(m["id"])
    want = f"Price 10€ for a café today {TAG}"
    assert want in (d["text"] or ""), repr(d["text"])
    return "quoted-printable decoded"


def check_base64_cte():
    mf = next_from()
    to = "bob@smoke.test"
    payload = f"Base64 body Ωmega ✓ {TAG}"  # "Ωmega ✓"
    enc = base64.b64encode(payload.encode("utf-8")).decode()
    wrapped = "\n".join(enc[i : i + 76] for i in range(0, len(enc), 76))
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: b64 {TAG}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8
Content-Transfer-Encoding: base64

{wrapped}
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    d = detail(m["id"])
    assert payload in (d["text"] or ""), repr(d["text"])
    return "base64 CTE decoded"


def check_auth_plain():
    mf = next_from()
    to = "bob@smoke.test"
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: authplain {TAG}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8

authed via PLAIN {TAG}
"""
    )
    smtp_send(msg, mf, [to], mech="plain")
    m = find_by_from(mf)
    assert m["auth_user"] == USER, m["auth_user"]
    return m["auth_user"]


def check_auth_login():
    mf = next_from()
    to = "bob@smoke.test"
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: authlogin {TAG}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8

authed via LOGIN {TAG}
"""
    )
    smtp_send(msg, mf, [to], mech="login")
    m = find_by_from(mf)
    assert m["auth_user"] == USER, m["auth_user"]
    return m["auth_user"]


def check_wrong_password():
    code = smtp_auth_code("definitely-wrong-" + TAG)
    assert code == 535, f"expected 535, got {code}"
    return code


def check_raw_route():
    mf = next_from()
    to = "bob@smoke.test"
    # No dot-lines and explicit CRLF, so the stored .eml must equal the sent bytes exactly.
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: raw exact {TAG}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8

raw body must match exactly {TAG}
trailing line
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    raw = get_raw(m["id"])
    assert raw == msg, f"raw mismatch: got {len(raw)} bytes, sent {len(msg)}"
    return f"raw {len(raw)} bytes identical"


def check_delete_one():
    mf = next_from()
    to = "bob@smoke.test"
    msg = wire(
        f"""From: <{mf}>
To: <{to}>
Subject: to delete {TAG}
MIME-Version: 1.0
Content-Type: text/plain; charset=utf-8

delete me {TAG}
"""
    )
    smtp_send(msg, mf, [to])
    m = find_by_from(mf)
    mid = m["id"]
    code, _, _ = http("DELETE", f"/api/v1/messages/{mid}")
    assert code == 204, code
    code2, _, _ = http("GET", f"/api/v1/messages/{mid}")
    assert code2 == 404, code2
    return mid


def check_purge_all():
    code, body, _ = http("DELETE", "/api/v1/messages")
    assert code == 200, code
    obj = json.loads(body)
    assert "deleted" in obj, obj
    assert obj["deleted"] >= 1, obj
    return obj["deleted"]


def check_store_empty():
    data = get_json("/api/v1/messages?limit=500")
    assert data["total"] == 0, data["total"]
    assert data["count"] == 0, data
    assert data["messages"] == [], data["messages"]
    return data["total"]


print(f"minimail smoke: SMTP {SMTP_HOST}:{SMTP_PORT}  HTTP {API_BASE}  user {USER!r}\n")

t("healthz", check_healthz)
t("purge existing", check_purge_existing)
t("plain text message", check_plain)
t("multipart/alternative", check_multipart_alt)
t("binary attachment byte-exact", check_binary_attachment)
t("RFC 2047 encoded subject", check_encoded_subject)
t("dot-stuffing round-trip", check_dot_stuffing)
t("quoted-printable transfer encoding", check_quoted_printable)
t("base64 transfer encoding", check_base64_cte)
t("AUTH PLAIN", check_auth_plain)
t("AUTH LOGIN", check_auth_login)
t("wrong password rejected (535)", check_wrong_password)
t("raw route returns original bytes", check_raw_route)
t("delete one message", check_delete_one)
t("purge all messages", check_purge_all)
t("store is empty", check_store_empty)

print(f"\n{passed} passed, {failed} failed")
sys.exit(0 if failed == 0 else 1)
