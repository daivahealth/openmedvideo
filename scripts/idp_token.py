#!/usr/bin/env python3
"""Mint a fake IdP identity token (HS256) matching the seeded dev client's
registration (issuer https://idp.dev, audience omv, secret dev-idp-secret).
This simulates what AADI's real IdP would issue; stdlib only.

Usage: python3 scripts/idp_token.py [username]
"""
import base64
import hashlib
import hmac
import json
import sys
import time


def b64url(b: bytes) -> bytes:
    return base64.urlsafe_b64encode(b).rstrip(b"=")


def mint(username: str, secret: str = "dev-idp-secret") -> str:
    header = b64url(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    payload = b64url(json.dumps({
        "iss": "https://idp.dev",
        "aud": "omv",
        "sub": f"uid-{username}",
        "preferred_username": username,
        "exp": int(time.time()) + 600,
    }).encode())
    signing_input = header + b"." + payload
    sig = b64url(hmac.new(secret.encode(), signing_input, hashlib.sha256).digest())
    return (signing_input + b"." + sig).decode()


if __name__ == "__main__":
    print(mint(sys.argv[1] if len(sys.argv) > 1 else "dr.asha"))
