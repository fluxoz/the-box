#!/usr/bin/env python3
"""Drive thebox.build in a real browser and assert what a person actually gets.

This exists because three bugs shipped that every other kind of checking was
structurally blind to:

  * A `<form>` rendered inside the page's own `<form>`. HTML forbids nesting, so
    the parser DROPPED it; the element was never in the DOM, wiring a handler to
    it threw, and that exception killed the rest of init — including the pairing
    code. The source read correctly. The built artifact was correct. The bug
    only existed after the HTML parser ran.
  * The generated private key saved as `id_ed25519.txt`, because a browser
    appends `.txt` to an extensionless filename served as `text/plain`, and ssh
    needs the exact name. That only exists after a click and a download.
  * Generating a keypair returned one half of the pair. Nothing asserted the
    page handed back a usable result.

So this asserts on the RENDERED, INTERACTED page, which is the only place those
live. It is deliberately about outcomes a person can see, not implementation.

Covers the two things the whole funnel rests on: the landing page pointing at a
command that works, and the Configurator handing back a usable pairing code and
keypair. The Configurator is part of the site, so it is tested as part of it.

Usage: test-site.py <site-dir>
"""

import http.server
import functools
import json
import os
import subprocess
import tempfile
import socketserver
import sys
import threading
from pathlib import Path

from playwright.sync_api import sync_playwright

FAILURES: list[str] = []


def check(ok: bool, what: str, detail: str = "") -> None:
    if ok:
        print(f"  ok   {what}")
    else:
        FAILURES.append(f"{what}{f' — {detail}' if detail else ''}")
        print(f"  FAIL {what}" + (f" — {detail}" if detail else ""))


def serve(root: Path) -> tuple[socketserver.TCPServer, int]:
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(root))
    httpd = socketserver.TCPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd, httpd.server_address[1]


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <site-dir>", file=sys.stderr)
        return 2
    site = Path(sys.argv[1])
    for need in ("index.html", "configurator/index.html"):
        if not (site / need).is_file():
            print(f"no {need} in {site}", file=sys.stderr)
            return 2

    httpd, port = serve(site)
    base = f"http://127.0.0.1:{port}"
    print(f"driving {base}")

    with sync_playwright() as p:
        # Point at the chromium nix provides rather than playwright's bundled
        # download: the bundle is a separate versioned artifact that drifts out
        # of step with the library, and CI must not fetch browsers at runtime.
        exe = os.environ.get("CHROMIUM_BIN")
        browser = p.chromium.launch(executable_path=exe) if exe else p.chromium.launch()
        ctx = browser.new_context(accept_downloads=True)
        page = ctx.new_page()

        errors: list[str] = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.on(
            "console",
            lambda m: errors.append(m.text) if m.type == "error" and "404" not in m.text else None,
        )
        # --- the landing page: does it point anywhere real? ----------------
        page.goto(f"{base}/", wait_until="networkidle")

        # The headline command was, for a long time, a copyable
        # `curl … | sudo sh` that could never work: install.sh exits unless
        # BOX_ORDERS_B64 is set. Nothing caught it because nothing ran it.
        body = page.inner_text("body")
        copyable = page.evaluate(
            "() => [...document.querySelectorAll('[data-copy]')]"
            ".map(b => (document.getElementById(b.dataset.copy)||{}).textContent || '')"
        )
        bare = [c for c in copyable if "install.sh" in c and "BOX_ORDERS_B64" not in c]
        check(not bare, "no copyable install command that lacks orders", "; ".join(bare)[:70])

        check("flash" in body.lower(), "the landing page offers the flash path")
        check("curl" in body.lower(), "the landing page offers the takeover path")
        hrefs = page.eval_on_selector_all("a[href]", "els => els.map(e => e.getAttribute('href'))")
        check(
            any("/configurator" in (h or "") for h in hrefs),
            "the landing page links to the Configurator",
        )

        # --- the Configurator, which is part of this site ------------------
        page.goto(f"{base}/configurator/", wait_until="networkidle")

        # --- the credential the whole install depends on -------------------
        # An exception anywhere in init used to leave this silently empty.
        page.wait_for_selector("#paircode", timeout=10_000)
        code = (page.text_content("#paircode") or "").strip()
        check(bool(code), "a pairing code is shown")
        # 80 bits. It was 40, and only the HASH of it travels in the orders,
        # which get pasted into shell commands and screenshots.
        check(len(code) >= 20, "the pairing code carries at least 80 bits", f"len={len(code)}")

        link = page.get_attribute("#claimlink", "href") or ""
        check(code and code in link, "the claim link carries that code", link[:60])

        # --- both halves of a generated keypair ----------------------------
        page.click("#key-gen")
        page.wait_for_selector("#pub-text", timeout=20_000)
        priv = page.input_value("#priv-text")
        pub = page.input_value("#pub-text")
        check(priv.startswith("-----BEGIN OPENSSH PRIVATE KEY"), "the private key is shown")
        check(pub.startswith("ssh-ed25519 "), "the public key is shown", pub[:40])

        # The public half is useless unless the Box actually trusts it.
        orders = page.evaluate("() => document.body.innerText")
        check("ssh_authorized_keys" in orders, "the public key reaches the orders")
        check("enrollment_code_hash" in orders, "the pairing hash reaches the orders")

        # --- a detailed setup must survive into installable orders ---------
        #
        # This page exists to configure an unattended install, so the thing that
        # matters is not that it renders: it is that what it emits is something
        # a machine with nobody in front of it will actually accept. The same
        # validator the installer runs before it wipes a disk is run here on
        # whatever the page just produced.
        # The name field only appears once you choose to set one, which is
        # itself part of what this page configures.
        # A styled radio: the visible <span> takes the click, so drive the
        # label a person would actually press.
        page.click("label:has(input[name=namemode][value=custom])")
        page.fill("#hostname", "kitchen")
        page.dispatch_event("#hostname", "input")
        orders_text = page.evaluate(
            "() => document.querySelector('#json')?.innerText"
            " || document.querySelector('#jsonout')?.innerText || ''"
        )
        if not orders_text.strip():
            orders_text = page.evaluate("() => (window.currentJSON || '')")
        parsed = None
        try:
            parsed = json.loads(orders_text)
        except Exception:
            pass
        check(parsed is not None, "the page exposes the orders it built", orders_text[:60])

        if parsed is not None:
            check(parsed.get("hostname") == "kitchen", "a configured hostname reaches the orders",
                  repr(parsed.get("hostname")))
            check(bool(parsed.get("ssh_authorized_keys")), "the generated key reaches the orders")
            check(len(str(parsed.get("enrollment_code_hash", ""))) == 64,
                  "the orders carry a full pairing hash")

            validator = os.environ.get("BOX_INSTALLER_BIN")
            if validator:
                # The installer forces consent itself; the page never sets it.
                parsed.setdefault("erase_disk", True)
                with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
                    json.dump(parsed, f)
                    path = f.name
                r = subprocess.run([validator, "validate-orders", path],
                                   capture_output=True, text=True)
                check(r.returncode == 0,
                      "the installer would accept what this page produced",
                      (r.stderr or r.stdout).strip()[:110])
            else:
                print("  skip BOX_INSTALLER_BIN not set — orders not checked against the installer")

        # --- the files a person ends up with -------------------------------
        # Named exactly, because ssh looks for id_ed25519 and nothing else.
        saved: dict[str, str] = {}
        with tempfile.TemporaryDirectory() as keydir:
            for button, expected in (("#dl-priv", "id_ed25519"), ("#dl-pub", "id_ed25519.pub")):
                with page.expect_download(timeout=15_000) as dl:
                    page.click(button)
                d = dl.value
                check(
                    d.suggested_filename == expected,
                    f"saving {button} yields {expected}",
                    f"got {d.suggested_filename}",
                )
                dest = os.path.join(keydir, expected)
                d.save_as(dest)
                saved[expected] = dest

            # The two halves must actually be a PAIR. Asserting the public key
            # merely starts with "ssh-ed25519" would pass just as happily on a
            # key belonging to somebody else, which is the failure that would
            # matter: you would authorize one key on the Box and hold another.
            # So derive the public half from the private one and compare.
            priv_path = saved.get("id_ed25519")
            pub_path = saved.get("id_ed25519.pub")
            if priv_path and pub_path:
                os.chmod(priv_path, 0o600)
                r = subprocess.run(
                    ["ssh-keygen", "-y", "-f", priv_path],
                    capture_output=True, text=True,
                )
                if r.returncode != 0:
                    check(False, "the saved private key is one ssh can read",
                          r.stderr.strip()[:110])
                else:
                    derived = r.stdout.split()[:2]
                    stated = open(pub_path).read().split()[:2]
                    check(derived == stated,
                          "the saved keys are actually a pair",
                          f"private derives {' '.join(derived)[:44]}…")

        # --- the page did not quietly break --------------------------------
        check(not errors, "no uncaught errors on the page", "; ".join(errors[:2]))

        browser.close()
    httpd.shutdown()

    if FAILURES:
        print(f"\n{len(FAILURES)} failure(s):", file=sys.stderr)
        for f in FAILURES:
            print(f"  - {f}", file=sys.stderr)
        return 1
    print("\nthebox.build: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
