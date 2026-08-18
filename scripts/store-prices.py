#!/usr/bin/env python3
"""Ask each maker's own store what our machines cost right now.

Reads scripts/store-catalog.json, fetches every Shopify-backed source (the
makers all run Shopify, whose /products.json endpoint is a stable, documented
JSON feed — no HTML scraping), computes the store's total (hardware + the flat
15 percent fee, $99 floor), and writes a pricing file the store page fetches
and displays. A source that fails keeps its baked fallback and is marked so
the page leaves the static copy alone rather than showing a stale "live"
number.

Also reports drift: hardware that moved more than 10 percent from the baked
baseline, or a Stripe deposit that no longer approximates the 15 percent fee.
The nightly workflow turns a non-empty drift list into a GitHub issue, because
the deposit amounts live on Stripe payment links that only
scripts/store-stripe-setup.sh can re-mint.

Usage: store-prices.py [--catalog PATH] [--out PATH]
Exit code is 0 even on drift (drift is a report, not a failure); non-zero only
when the check itself could not run.
"""

import argparse
import json
import re
import sys
import time
import urllib.request

UA = "Mozilla/5.0 (compatible; thebox.build nightly price check)"


def fetch_store(domain):
    """All products of one Shopify store, paginated."""
    products = []
    for page in range(1, 5):
        url = f"https://{domain}/products.json?limit=250&page={page}"
        req = urllib.request.Request(url, headers={"User-Agent": UA})
        with urllib.request.urlopen(req, timeout=30) as r:
            batch = json.load(r).get("products", [])
        products += batch
        if len(batch) < 250:
            break
    return products


def current_price(store, handle, variant_pattern):
    """Lowest matching variant, preferring in-stock ones.

    A maker being out of stock is itself signal (the bench buys through
    distribution anyway), so the listed price still counts — it just gets
    reported as out of stock rather than dropped.
    """
    rx = re.compile(variant_pattern, re.I)
    for product in store:
        if product.get("handle") != handle:
            continue
        matching = [v for v in product.get("variants", []) if rx.search(v.get("title", ""))]
        in_stock = [float(v["price"]) for v in matching if v.get("available", True)]
        if in_stock:
            return min(in_stock), True
        if matching:
            return min(float(v["price"]) for v in matching), False
        return None, False
    return None, False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--catalog", default="scripts/store-catalog.json")
    ap.add_argument("--out", default="pricing.json")
    args = ap.parse_args()

    catalog = json.load(open(args.catalog))
    fee_rate = catalog["fee_rate"]
    fee_floor = catalog["fee_floor"]

    stores = {}
    for sku in catalog["skus"].values():
        src = sku["source"]
        if src["type"] == "shopify" and src["domain"] not in stores:
            try:
                stores[src["domain"]] = fetch_store(src["domain"])
            except Exception as e:
                print(f"warn: {src['domain']} unreachable: {e}", file=sys.stderr)
                stores[src["domain"]] = None

    out = {"generated_at": int(time.time()), "skus": {}}
    drift = []
    for sku_id, sku in catalog["skus"].items():
        src = sku["source"]
        hardware = None
        live = False
        in_stock = False
        if src["type"] == "shopify" and stores.get(src["domain"]) is not None:
            hardware, in_stock = current_price(stores[src["domain"]], src["handle"], src["variant"])
            live = hardware is not None
            if not live:
                print(f"warn: {sku_id}: no variant of {src['handle']} matches "
                      f"{src['variant']!r} — check the catalog", file=sys.stderr)
        if hardware is None:
            hardware = sku["baked_hardware"]

        fee = max(fee_floor, round(hardware * fee_rate))
        total = round(hardware + fee)
        deposit = sku["deposit_cents"] / 100
        if live and in_stock:
            hw_str = f"hardware ${hardware:,.0f} at the maker right now"
        elif live:
            hw_str = f"hardware listed ${hardware:,.0f} at the maker (out of stock there; we buy through distribution)"
        else:
            hw_str = f"hardware last seen ${hardware:,.0f}"
        entry = {
            "live": live,
            "in_stock": in_stock,
            "hardware": round(hardware),
            "fee": fee,
            "total": total,
            "total_str": f"from ${total:,.0f}",
            "hw_str": hw_str,
        }
        out["skus"][sku_id] = entry

        baked = sku["baked_hardware"]
        if live and abs(hardware - baked) / baked > 0.10:
            drift.append(f"{sku_id}: hardware ${hardware:,.0f} vs baked ${baked:,.0f} "
                         f"({(hardware - baked) / baked:+.0%}) — update baked_hardware")
        if abs(fee - deposit) / deposit > 0.15:
            drift.append(f"{sku_id}: 15% fee is now ${fee:,.0f} but the Stripe deposit link "
                         f"charges ${deposit:,.0f} — update deposit_cents and re-run "
                         f"store-stripe-setup.sh")

    out["drift"] = drift
    with open(args.out, "w") as f:
        json.dump(out, f, indent=2)

    live_n = sum(1 for e in out["skus"].values() if e["live"])
    print(f"{live_n}/{len(out['skus'])} prices live from the makers; "
          f"{len(drift)} drift finding(s)")
    for d in drift:
        print(f"  drift: {d}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
