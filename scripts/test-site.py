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

The landing page now IS the minimal claim form (key in, pairing code out, image
or command out), so it gets the same treatment as the Configurator: generate a
keypair, save both halves, and prove the key and the pairing-code hash actually
ride in the base64 orders of BOTH commands — curl for Linux, iex for Windows.

Usage: test-site.py <site-dir>
"""

import base64
import hashlib
import http.server
import functools
import json
import os
import re
import subprocess
import tempfile
import socketserver
import sys
import threading
from pathlib import Path

from playwright.sync_api import sync_playwright

FAILURES: list[str] = []

HEX_PAIRCODE = "() => /^[0-9a-f]{20,}$/.test((document.querySelector('#paircode')||{}).textContent||'')"


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


def validate_with_installer(orders: dict, what: str) -> None:
    """Run the same validator the installer runs before it wipes a disk."""
    validator = os.environ.get("BOX_INSTALLER_BIN")
    if not validator:
        print(f"  skip BOX_INSTALLER_BIN not set — {what} not checked against the installer")
        return
    orders = dict(orders)
    orders.setdefault("erase_disk", True)
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        json.dump(orders, f)
        path = f.name
    r = subprocess.run([validator, "validate-orders", path], capture_output=True, text=True)
    check(r.returncode == 0, f"the installer would accept {what}",
          (r.stderr or r.stdout).strip()[:110])


def credential_checks(page, where: str) -> tuple[str, str]:
    """The credentials the whole install depends on: a pairing code, and a
    generated keypair whose halves are really a pair and save under the exact
    names ssh looks for. Returns (pairing_code, public_key)."""
    # An exception anywhere in init used to leave the code silently empty, and
    # it renders async (the hash is computed first), so wait for the real thing.
    page.wait_for_function(HEX_PAIRCODE, timeout=10_000)
    code = (page.text_content("#paircode") or "").strip()
    check(bool(code), f"{where}: a pairing code is shown")
    # 80 bits. It was 40, and only the HASH of it travels in the orders, which
    # get pasted into shell commands and screenshots.
    check(len(code) >= 20, f"{where}: the pairing code carries at least 80 bits", f"len={len(code)}")

    # --- both halves of a generated keypair ----------------------------
    page.click("#key-gen")
    page.wait_for_selector("#pub-text", timeout=20_000)
    priv = page.input_value("#priv-text")
    pub = page.input_value("#pub-text")
    check(priv.startswith("-----BEGIN OPENSSH PRIVATE KEY"), f"{where}: the private key is shown")
    check(pub.startswith("ssh-ed25519 "), f"{where}: the public key is shown", pub[:40])

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
                f"{where}: saving {button} yields {expected}",
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
                check(False, f"{where}: the saved private key is one ssh can read",
                      r.stderr.strip()[:110])
            else:
                derived = r.stdout.split()[:2]
                stated = open(pub_path).read().split()[:2]
                check(derived == stated,
                      f"{where}: the saved keys are actually a pair",
                      f"private derives {' '.join(derived)[:44]}…")
    return code, pub


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
        # --- the landing page: the minimal claim form -----------------------
        page.goto(f"{base}/", wait_until="networkidle")

        body = page.inner_text("body")
        check("flash" in body.lower(), "the landing page offers the flash path")
        check("curl" in body.lower(), "the landing page offers the takeover path")
        hrefs = page.eval_on_selector_all("a[href]", "els => els.map(e => e.getAttribute('href'))")
        check(
            any("/configurator" in (h or "") for h in hrefs),
            "the landing page links to the Configurator",
        )

        code, pub = credential_checks(page, "landing")

        # The commands render from the live form state; wait for the real ones,
        # not the placeholder, then hold them to the standard: orders embedded.
        page.wait_for_function(
            "() => (document.getElementById('cmd-linux')||{textContent:''}).textContent.includes('BOX_ORDERS_B64')",
            timeout=10_000,
        )
        # The headline command was, for a long time, a copyable
        # `curl … | sudo sh` that could never work: install.sh exits unless
        # BOX_ORDERS_B64 is set. Nothing caught it because nothing ran it.
        copyable = page.evaluate(
            "() => [...document.querySelectorAll('[data-copy]')]"
            ".map(b => (document.getElementById(b.dataset.copy)||{}).textContent || '')"
        )
        bare = [c for c in copyable if "install.sh" in c and "BOX_ORDERS_B64" not in c]
        check(not bare, "no copyable install command that lacks orders", "; ".join(bare)[:70])
        bare_win = [c for c in copyable if "install.ps1" in c and "BOX_ORDERS_B64" not in c]
        check(not bare_win, "no copyable Windows command that lacks orders", "; ".join(bare_win)[:70])

        # Decode the rider and hold it to what the whole flow promises: the
        # public key just generated and the hash of the code just shown, in
        # orders an unattended installer will accept.
        linux_cmd = page.text_content("#cmd-linux") or ""
        win_cmd = page.text_content("#cmd-win") or ""
        m = re.search(r"BOX_ORDERS_B64='([A-Za-z0-9+/=]+)'", linux_cmd)
        check(m is not None, "the curl command carries a base64 rider", linux_cmd[:70])
        if m:
            rider = m.group(1)
            orders = json.loads(base64.b64decode(rider))
            check(orders.get("erase_disk") is True, "the rider consents to erase_disk")
            check(orders.get("ssh_authorized_keys") == [pub],
                  "the generated key rides in the orders")
            want = hashlib.sha256(code.encode()).hexdigest()
            check(orders.get("enrollment_code_hash") == want,
                  "the rider carries the hash of the code on screen")
            check(rider in win_cmd and "install.ps1" in win_cmd and "iex" in win_cmd,
                  "the Windows command carries the same rider", win_cmd[:70])
            validate_with_installer(orders, "the landing page's orders")

        # The other door: the image download exists and is armed, and it is not
        # a Pi-only door — any supported machine flashes.
        check(page.query_selector("#pi-go") is not None, "the flash door offers an image download")
        check(page.eval_on_selector("#pi-go", "b => !b.disabled"),
              "the image download is enabled once the form is live")
        models = page.eval_on_selector_all("#pi-model option", "els => els.map(e => e.value)")
        check("thebox-x86" in models and "thebox-pi5" in models,
              "the flash door offers x86 and Pi images", ",".join(models))

        # --- the Configurator, which is part of this site ------------------
        page.goto(f"{base}/configurator/", wait_until="networkidle")

        cfg_code, _cfg_pub = credential_checks(page, "configurator")

        link = page.get_attribute("#claimlink", "href") or ""
        check(cfg_code and cfg_code in link, "the claim link carries that code", link[:60])

        # The public half is useless unless the Box actually trusts it.
        orders_view = page.evaluate("() => document.body.innerText")
        check("ssh_authorized_keys" in orders_view, "the public key reaches the orders")
        check("enrollment_code_hash" in orders_view, "the pairing hash reaches the orders")

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
            validate_with_installer(parsed, "what this page produced")

        # --- the two doors are the only doors ------------------------------
        vec_names = page.eval_on_selector_all(
            ".vec .name", "els => els.map(e => e.textContent.trim())"
        )
        check(len(vec_names) == 2, "the Configurator offers exactly two deployment vectors",
              ", ".join(vec_names))

        # Arming a complete config must produce the goods: both takeover
        # commands, orders embedded.
        page.click("[data-vec=sh]")
        page.click(".arm:has(#arm)")
        page.wait_for_selector("#sh-cmd", timeout=10_000)
        sh_cmd = page.text_content("#sh-cmd") or ""
        cfg_win_cmd = page.text_content("#win-cmd") or ""
        check("BOX_ORDERS_B64='" in sh_cmd and "install.sh" in sh_cmd,
              "arming yields a curl command with orders embedded", sh_cmd[:70])
        check("BOX_ORDERS_B64='" in cfg_win_cmd and "install.ps1" in cfg_win_cmd,
              "arming yields a Windows command with orders embedded", cfg_win_cmd[:70])

        # --- the fleet panel: PXE is the network delivery of the same orders -
        # Not a third door (the vector count above pins that); it must still
        # emit a working depot command and a boot.ipxe that carry exactly the
        # orders on screen, or a fleet takes different orders than the one
        # machine would.
        page.click("#fleet-toggle")
        page.wait_for_selector("#pxe-cmd", timeout=10_000)
        pxe_cmd = page.text_content("#pxe-cmd") or ""
        check("pxe.sh" in pxe_cmd and "BOX_ORDERS_B64='" in pxe_cmd,
              "the fleet panel yields a PXE depot command with orders embedded", pxe_cmd[:70])
        m = re.search(r"BOX_ORDERS_B64='([A-Za-z0-9+/=]+)'", pxe_cmd)
        fleet_orders = None
        if m:
            fleet_orders = json.loads(base64.b64decode(m.group(1)))
            check(fleet_orders.get("erase_disk") is True,
                  "the fleet rider consents to erase_disk")
            check(fleet_orders.get("enrollment_code_hash")
                  == hashlib.sha256(cfg_code.encode()).hexdigest(),
                  "the fleet rider carries the hash of the code on screen")
        with page.expect_download(timeout=15_000) as dl:
            page.click("#dl-ipxe")
        d = dl.value
        check(d.suggested_filename == "boot.ipxe", "the fleet panel saves boot.ipxe",
              f"got {d.suggested_filename}")
        with tempfile.TemporaryDirectory() as ipxedir:
            dest = os.path.join(ipxedir, "boot.ipxe")
            d.save_as(dest)
            script = open(dest).read()
            check(script.startswith("#!ipxe"), "boot.ipxe is an iPXE script", script[:30])
            check("chain netboot.ipxe" in script,
                  "boot.ipxe chains the published netboot.ipxe")
            m = re.search(r"box\.install-b64=([A-Za-z0-9+/=]+)", script)
            check(m is not None, "boot.ipxe carries the orders on the kernel command line")
            if m and fleet_orders is not None:
                check(json.loads(base64.b64decode(m.group(1))) == fleet_orders,
                      "boot.ipxe and the depot command carry the same orders")

        # --- the store: every machine card is wired for live pricing --------
        # The nightly price check publishes totals the page fills in by
        # data-sku; a card missing its hooks silently keeps stale numbers
        # forever, which is the exact rot this machinery exists to stop.
        page.goto(f"{base}/store/", wait_until="networkidle")
        skus = page.eval_on_selector_all(
            "[data-sku]", "els => els.map(e => e.dataset.sku)"
        )
        check(len(skus) >= 9, "the store offers at least nine machines", ",".join(skus))
        unwired = page.eval_on_selector_all(
            "[data-sku]",
            "els => els.filter(e => !e.querySelector('[data-p=total]')"
            " || !e.querySelector('[data-p=hw]')).map(e => e.dataset.sku)",
        )
        check(not unwired, "every machine card has live-price hooks", ",".join(unwired))
        # Standing rule (John, 2026-08-18): every store flow runs through
        # Stripe. A GitHub issue is never a checkout, a waitlist, or anything
        # else a customer touches.
        issue_links = page.eval_on_selector_all(
            "a[href*='issues/new']", "els => els.map(e => e.href)"
        )
        check(not issue_links, "no store flow goes through a GitHub issue",
              "; ".join(issue_links)[:80])
        buy = page.eval_on_selector_all(
            "[data-sku] a.go", "els => els.map(e => e.href)"
        )
        not_stripe = [u for u in buy if "buy.stripe.com" not in u]
        check(not not_stripe, "every machine checkout is a Stripe link",
              "; ".join(not_stripe)[:80])

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
