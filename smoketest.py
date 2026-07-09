#!/usr/bin/env python3
"""End-to-end check against a running minicloak.

Drives the real HTTP surface and verifies RS256 signatures independently, using
nothing but int arithmetic against the published JWKS -- so a bug in minicloak's
own crypto cannot make this pass.

Usage: python smoketest.py [base_url]     (default http://127.0.0.1:19500)
"""
import base64
import hashlib
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:19500"
REALM = "dev"
OIDC = f"{BASE}/realms/{REALM}/protocol/openid-connect"

passed = failed = 0


def check(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print(f"  ok   {name}")
    else:
        failed += 1
        print(f"  FAIL {name} {detail}")


def b64url_decode(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def b64url_encode(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


opener = urllib.request.build_opener(NoRedirect)


def request(method, url, data=None, headers=None):
    body = urllib.parse.urlencode(data).encode() if data else None
    req = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    if body:
        req.add_header("Content-Type", "application/x-www-form-urlencoded")
    try:
        r = opener.open(req)
        return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()


def get_json(url, headers=None):
    status, _, body = request("GET", url, headers=headers)
    return status, json.loads(body) if body else None


def post_json(url, data, headers=None):
    status, _, body = request("POST", url, data=data, headers=headers)
    return status, json.loads(body) if body else None


# --- independent RS256 verification, straight from the JWKS ---

SHA256_DIGEST_INFO = bytes.fromhex("3031300d060960864801650304020105000420")


def verify_rs256(token, jwk):
    header_b64, payload_b64, sig_b64 = token.split(".")
    signing_input = f"{header_b64}.{payload_b64}".encode()
    n = int.from_bytes(b64url_decode(jwk["n"]), "big")
    e = int.from_bytes(b64url_decode(jwk["e"]), "big")
    sig = int.from_bytes(b64url_decode(sig_b64), "big")
    k = (n.bit_length() + 7) // 8
    em = pow(sig, e, n).to_bytes(k, "big")
    digest = hashlib.sha256(signing_input).digest()
    expected = b"\x00\x01" + b"\xff" * (k - 54) + b"\x00" + SHA256_DIGEST_INFO + digest
    return em == expected


def claims(token):
    return json.loads(b64url_decode(token.split(".")[1]))


def header(token):
    return json.loads(b64url_decode(token.split(".")[0]))


def main():
    print(f"minicloak smoketest against {BASE}\n")

    # --- discovery + jwks ---
    print("discovery")
    status, disc = get_json(f"{OIDC.rsplit('/protocol', 1)[0]}/.well-known/openid-configuration")
    check("discovery returns 200", status == 200)
    check("issuer matches realm", disc["issuer"].endswith(f"/realms/{REALM}"), disc["issuer"])
    check("token_endpoint advertised", disc["token_endpoint"] == f"{OIDC}/token")
    check("PKCE S256 advertised", "S256" in disc["code_challenge_methods_supported"])
    status, alias = get_json(f"{BASE}/.well-known/openid-configuration")
    check("root discovery alias works", status == 200 and alias["issuer"] == disc["issuer"])

    status, jwks = get_json(disc["jwks_uri"])
    check("jwks returns 200", status == 200)
    jwk = jwks["keys"][0]
    check("jwk is RS256/RSA", jwk["kty"] == "RSA" and jwk["alg"] == "RS256")
    check("jwk carries no private material", "d" not in jwk and "p" not in jwk)

    # --- authorization code + PKCE (public client) ---
    print("\nauthorization code + PKCE")
    verifier = b64url_encode(os.urandom(32))
    challenge = b64url_encode(hashlib.sha256(verifier.encode()).digest())
    redirect_uri = "http://localhost:5173/callback"
    authz = f"{OIDC}/auth?" + urllib.parse.urlencode({
        "response_type": "code",
        "client_id": "spa",
        "redirect_uri": redirect_uri,
        "scope": "openid profile email",
        "state": "st4te",
        "nonce": "n0nce",
        "code_challenge": challenge,
        "code_challenge_method": "S256",
    })
    status, _, body = request("GET", authz)
    check("authorize renders a login page", status == 200 and b"<form" in body)

    form = {
        "response_type": "code", "client_id": "spa", "redirect_uri": redirect_uri,
        "scope": "openid profile email", "state": "st4te", "nonce": "n0nce",
        "code_challenge": challenge, "code_challenge_method": "S256",
        "username": "alice", "password": "alice",
    }
    status, hdrs, _ = request("POST", f"{OIDC}/auth", data=form)
    check("login redirects (302)", status == 302, status)
    loc = hdrs.get("Location", "")
    check("redirects to the registered uri", loc.startswith(redirect_uri), loc)
    q = urllib.parse.parse_qs(urllib.parse.urlparse(loc).query)
    check("state is echoed back", q.get("state") == ["st4te"])
    code = q["code"][0]
    check("a code was issued", bool(code))

    status, tok = post_json(f"{OIDC}/token", {
        "grant_type": "authorization_code", "code": code, "client_id": "spa",
        "redirect_uri": redirect_uri, "code_verifier": verifier,
    })
    check("token exchange succeeds", status == 200, tok)
    check("access_token present", "access_token" in tok)
    check("id_token present", "id_token" in tok)
    check("refresh_token present", "refresh_token" in tok)
    check("token_type is Bearer", tok["token_type"] == "Bearer")

    # --- signature verification, independent of minicloak ---
    print("\nsignature verification (python, against jwks)")
    check("id_token signature verifies", verify_rs256(tok["id_token"], jwk))
    check("access_token signature verifies", verify_rs256(tok["access_token"], jwk))
    check("kid in header matches jwks", header(tok["id_token"])["kid"] == jwk["kid"])
    idc = claims(tok["id_token"])
    check("id_token nonce echoed", idc.get("nonce") == "n0nce", idc.get("nonce"))
    check("id_token aud is the client", idc["aud"] == "spa")
    check("id_token iss matches discovery", idc["iss"] == disc["issuer"])
    check("id_token has auth_time", "auth_time" in idc)
    check("id_token carries email claim", idc.get("email") == "alice@example.com")
    check("id_token carries roles", "admin" in idc.get("roles", []))
    check("keycloak-style realm_access present", "admin" in idc["realm_access"]["roles"])

    tampered = tok["id_token"][:-4] + ("aaaa" if not tok["id_token"].endswith("aaaa") else "bbbb")
    check("tampered signature is rejected", not verify_rs256(tampered, jwk))

    # --- userinfo ---
    print("\nuserinfo")
    bearer = {"Authorization": f"Bearer {tok['access_token']}"}
    status, ui = get_json(f"{OIDC}/userinfo", headers=bearer)
    check("userinfo returns 200", status == 200)
    check("userinfo sub matches id_token", ui["sub"] == idc["sub"])
    check("userinfo has preferred_username", ui["preferred_username"] == "alice")
    status, _ = get_json(f"{OIDC}/userinfo", headers={"Authorization": "Bearer garbage"})
    check("userinfo rejects a bad token", status == 401, status)
    status, _ = get_json(f"{OIDC}/userinfo")
    check("userinfo rejects a missing token", status == 401)

    # --- code replay and PKCE enforcement ---
    print("\nnegative paths")
    status, err = post_json(f"{OIDC}/token", {
        "grant_type": "authorization_code", "code": code, "client_id": "spa",
        "redirect_uri": redirect_uri, "code_verifier": verifier,
    })
    check("a code cannot be replayed", status == 400 and err["error"] == "invalid_grant", err)

    status, hdrs, _ = request("POST", f"{OIDC}/auth", data=form)
    code2 = urllib.parse.parse_qs(urllib.parse.urlparse(hdrs["Location"]).query)["code"][0]
    status, err = post_json(f"{OIDC}/token", {
        "grant_type": "authorization_code", "code": code2, "client_id": "spa",
        "redirect_uri": redirect_uri, "code_verifier": "wrong-verifier",
    })
    check("wrong code_verifier is rejected", status == 400 and err["error"] == "invalid_grant", err)

    bad = dict(form, redirect_uri="http://evil.example/steal")
    status, _, body = request("POST", f"{OIDC}/auth", data=bad)
    check("unregistered redirect_uri does not redirect", status == 400, status)

    status, _, _ = request("GET", f"{OIDC}/auth?" + urllib.parse.urlencode(
        {"response_type": "code", "client_id": "spa", "redirect_uri": redirect_uri}))
    check("public client without PKCE is refused", status == 302)

    status, err = post_json(f"{OIDC}/token", {
        "grant_type": "client_credentials", "client_id": "myapp", "client_secret": "wrong"})
    check("bad client_secret is rejected", status == 401 and err["error"] == "invalid_client", err)

    status, err = post_json(f"{OIDC}/token", {"grant_type": "magic", "client_id": "spa"})
    check("unknown grant_type is rejected", status == 400 and err["error"] == "unsupported_grant_type")

    # --- refresh ---
    print("\nrefresh token")
    status, ref = post_json(f"{OIDC}/token", {
        "grant_type": "refresh_token", "refresh_token": tok["refresh_token"], "client_id": "spa"})
    check("refresh succeeds", status == 200, ref)
    check("refresh returns a new access_token", ref["access_token"] != tok["access_token"])
    check("refreshed access_token verifies", verify_rs256(ref["access_token"], jwk))
    check("refresh token is rotated", ref["refresh_token"] != tok["refresh_token"])
    status, err = post_json(f"{OIDC}/token", {
        "grant_type": "refresh_token", "refresh_token": tok["refresh_token"], "client_id": "spa"})
    check("old refresh token is dead after rotation", status == 400, err)

    # --- client credentials (confidential) ---
    print("\nclient credentials")
    basic = base64.b64encode(b"myapp:s3cret").decode()
    status, cc = post_json(f"{OIDC}/token", {"grant_type": "client_credentials"},
                           headers={"Authorization": f"Basic {basic}"})
    check("client_credentials via basic auth", status == 200, cc)
    check("client_credentials token verifies", verify_rs256(cc["access_token"], jwk))
    check("client_credentials has no refresh_token", "refresh_token" not in cc)
    check("service account subject", claims(cc["access_token"])["sub"] == "service-account-myapp")
    status, err = post_json(f"{OIDC}/token", {"grant_type": "client_credentials", "client_id": "spa"})
    check("public client cannot use client_credentials", status == 401, err)

    # --- password grant ---
    print("\npassword grant")
    status, pw = post_json(f"{OIDC}/token", {
        "grant_type": "password", "username": "bob", "password": "bob",
        "client_id": "myapp", "client_secret": "s3cret", "scope": "openid email"})
    check("password grant succeeds", status == 200, pw)
    check("password grant id_token verifies", verify_rs256(pw["id_token"], jwk))
    check("password grant honours scope", claims(pw["access_token"])["scope"] == "openid email")
    status, err = post_json(f"{OIDC}/token", {
        "grant_type": "password", "username": "bob", "password": "nope",
        "client_id": "myapp", "client_secret": "s3cret"})
    check("wrong password is rejected", status == 401, err)

    # --- introspection + revocation ---
    print("\nintrospection and revocation")
    status, intro = post_json(f"{OIDC}/token/introspect",
                              {"token": pw["access_token"]},
                              headers={"Authorization": f"Basic {basic}"})
    check("introspect reports active", status == 200 and intro["active"] is True, intro)
    check("introspect reports the client", intro["client_id"] == "myapp")
    status, intro = post_json(f"{OIDC}/token/introspect", {"token": "not-a-token"},
                              headers={"Authorization": f"Basic {basic}"})
    check("introspect reports inactive for junk", intro["active"] is False)
    status, _ = post_json(f"{OIDC}/token/introspect", {"token": pw["access_token"]})
    check("introspect requires client auth", status == 401)

    status, _, _ = request("POST", f"{OIDC}/revoke",
                           data={"token": pw["refresh_token"]},
                           headers={"Authorization": f"Basic {basic}"})
    check("revoke returns 200", status == 200)
    status, err = post_json(f"{OIDC}/token", {
        "grant_type": "refresh_token", "refresh_token": pw["refresh_token"],
        "client_id": "myapp", "client_secret": "s3cret"})
    check("revoked refresh token is dead", status == 400, err)

    # --- CORS ---
    print("\ncors")
    status, hdrs, _ = request("OPTIONS", f"{OIDC}/token",
                              headers={"Origin": "http://localhost:5173"})
    check("preflight returns 204", status == 204, status)
    check("preflight echoes origin",
          hdrs.get("Access-Control-Allow-Origin") == "http://localhost:5173")
    check("preflight allows POST", "POST" in hdrs.get("Access-Control-Allow-Methods", ""))

    print(f"\n{passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
