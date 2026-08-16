#!/usr/bin/env bash
# Creates the store's Stripe products, prices, and payment links, idempotently.
# Run again any time; existing objects are found and reused, never duplicated.
#
# The checkout model matches the store's pricing rule: the $499 fee (the only
# fixed, ours-to-charge part of the price) is paid at checkout and is fully
# refundable until the hardware ships. The hardware itself is billed at cost
# afterward via a Stripe invoice carrying the distributor's numbers. The $99
# Installer Kit is simply paid in full.
#
# Needs: STRIPE_KEY (a restricted key with write on Products, Prices, and
# Payment Links). Prints one "sku url" line per product for the store page.
set -euo pipefail

[ -n "${STRIPE_KEY:-}" ] || { echo "set STRIPE_KEY (restricted key: Products/Prices/Payment Links write)" >&2; exit 1; }
API="https://api.stripe.com/v1"
auth() { curl -fsS -u "$STRIPE_KEY:" "$@"; }
jqpy() { python3 -c "import json,sys; d=json.load(sys.stdin); $1"; }

ORDER_MSG="Order received. Within one business day we email your hardware quote with the distributor's invoice attached. Approve it and we order your machine to our bench: Box OS installed, your unit benchmarked against its rating, access keys loaded on the USB in the box. Then it ships to you. Full refund any time before it ships. The fee covers the bench work, a year of Box Cloud, and support until it works."
KIT_MSG="Order received. The setup stick ships within a few days; a year of Box Cloud is included. Instructions ride along in the envelope."

ensure() { # sku name amount_cents message
  local sku="$1" name="$2" amount="$3" msg="$4"

  local product
  product=$(auth -G "$API/products/search" --data-urlencode "query=metadata['sku']:'$sku'" \
    | jqpy "print(d['data'][0]['id'] if d['data'] else '')")
  if [ -z "$product" ]; then
    product=$(auth "$API/products" -d "name=$name" -d "metadata[sku]=$sku" | jqpy "print(d['id'])")
    echo "created product $product ($sku)" >&2
  fi

  local price cur_amount
  price=$(auth -G "$API/products/$product" | jqpy "print(d.get('default_price') or '')")
  cur_amount=""
  [ -n "$price" ] && cur_amount=$(auth -G "$API/prices/$price" | jqpy "print(d['unit_amount'])")
  if [ "$cur_amount" != "$amount" ]; then
    # Price changed (or never existed): mint a new price, retire the old
    # payment link so nobody can pay yesterday's number.
    price=$(auth "$API/prices" -d "product=$product" -d "unit_amount=$amount" -d "currency=usd" \
      | jqpy "print(d['id'])")
    auth "$API/products/$product" -d "default_price=$price" >/dev/null
    local old_link
    old_link=$(auth -G "$API/products/$product" | jqpy "print(d['metadata'].get('payment_link_id') or '')")
    [ -n "$old_link" ] && auth "$API/payment_links/$old_link" -d "active=false" >/dev/null
    auth "$API/products/$product" -d "metadata[sku]=$sku" -d "metadata[payment_link_url]=" \
      -d "metadata[payment_link_id]=" >/dev/null
    echo "priced $sku at $amount" >&2
  fi

  local url
  url=$(auth -G "$API/products/$product" | jqpy "print(d['metadata'].get('payment_link_url') or '')")
  if [ -n "$url" ] && [ -n "${FORCE_RELINK:-}" ]; then
    # Recreate links in place (e.g. the confirmation message changed).
    local old_link
    old_link=$(auth -G "$API/products/$product" | jqpy "print(d['metadata'].get('payment_link_id') or '')")
    [ -n "$old_link" ] && auth "$API/payment_links/$old_link" -d "active=false" >/dev/null
    url=""
  fi
  if [ -z "$url" ]; then
    url=$(auth "$API/payment_links" \
      -d "line_items[0][price]=$price" -d "line_items[0][quantity]=1" \
      -d "shipping_address_collection[allowed_countries][0]=US" \
      -d "shipping_address_collection[allowed_countries][1]=CA" \
      -d "custom_fields[0][key]=confignotes" \
      -d "custom_fields[0][label][type]=custom" \
      --data-urlencode "custom_fields[0][label][custom]=Configuration notes (optional)" \
      -d "custom_fields[0][type]=text" \
      -d "custom_fields[0][optional]=true" \
      -d "after_completion[type]=hosted_confirmation" \
      --data-urlencode "after_completion[hosted_confirmation][custom_message]=$msg" \
      -d "metadata[sku]=$sku" \
      | jqpy "print(d['url'])")
    local link_id
    link_id=$(auth -G "$API/payment_links" -d "limit=100" | jqpy "print(next((l['id'] for l in d['data'] if l.get('url')=='$url'),''))")
    auth "$API/products/$product" -d "metadata[sku]=$sku" -d "metadata[payment_link_url]=$url" \
      -d "metadata[payment_link_id]=$link_id" >/dev/null
    echo "created payment link ($sku)" >&2
  fi

  echo "$sku $url"
}

# The fee is a flat 15 percent of the hardware, charged at order time from
# the current price band and refundable until ship. Re-run after any band
# move; changed amounts retire the old link and mint a fresh one.
ensure "order-hp-z2-128"        "Inference Box order: HP Z2 Mini G1a (128GB)"       35900 "$ORDER_MSG"
ensure "order-gmktec-evo-128"   "Inference Box order: GMKtec EVO-X2 (128GB)"        48900 "$ORDER_MSG"
ensure "order-minisforum-s1max" "Inference Box order: Minisforum MS-S1 MAX (128GB)" 55900 "$ORDER_MSG"
ensure "order-gmktec-evo-64"    "Inference Box order: GMKtec EVO-X2 (64GB)"         29900 "$ORDER_MSG"
ensure "order-gmktec-g2-starter" "Starter Box order: GMKtec G2 Plus (N150)"           9900 "$ORDER_MSG"
ensure "kit-usb"                "The Installer Kit"                                  9900 "$KIT_MSG"
