# QA backlog - open findings from the 2026-08-19 fresh-eyes sweep

Fixed same-night findings are not listed; this is the open tail, ranked.
Full agent reports: the 8/19 session transcript.

## SEV2 (open)
- SHA256SUMS on the site lists ./netboot/* paths that 404 there (they live on
  the GitHub Release). Fix the publish stamping to emit verifiable paths, or
  annotate the file.
- Meter "cloud equivalent" pads: open-webui+llamacpp count as two hosted-AI
  line items, postgres+redis as two managed DBs. Dedupe by stack.

## SEV3 (open)
- Docs: .img.gz vs .img.xz drift (Pi guide's xz command cannot read the real
  download); ssh username never stated in human docs (llms-full leaks
  "murphy"); box.local vs box-XXXXXX naming inconsistencies; first-steps
  dashboard table missing Approvals/Journal tabs and Approvals has no doc;
  agents.html prev/next footer duplicated; undo/decommission story absent;
  Cloudflare email-obfuscation mangles root@IP examples without JS.
- Console: theme forgets itself across navigations (persist in localStorage);
  UTC timestamps unlabeled next to a local clock; mobile nav gives no
  scroll affordance at 390px; footer /api/v1 and /mcp chips are not links and
  /api/v1 404s (serve a JSON index); GET /devices/agent after mint is a blank
  405 (redirect to /devices); job view repeats phase lines in the log box;
  Access form leaves the domain field enabled under LAN; corrupted SSH keys
  accepted silently by the configurator key field (validate structure).
- Cloud: welcome-page preview banner could name where the enroll token goes
  on the Box (System - Box Cloud) even harder; docs/backups.html says
  "one-time token" where the cloud site says "enrollment token" (unify).
- MCP: initialize instructions claim /sites/<name>/ for all services (static
  only); list_services urls unusable off-box (synthesize host-based URLs or
  null for internal); tool descriptions hardcode port 2693.

## SEV4 (open)
- Store: EVO-X2 deposit figure lags the live fee band; nested parentheses
  copy; "yours forever" domain overpromise; minimum spec never pinned.
- Site: GitHub Pages default 404 page; Cloudflare Insights beacon vs
  "no phone-home" optics; receipts line self-contradicts when a nightly is
  missed (say "no run last night" instead).
- Console: nonexistent service/job pages return 200; devices page em-dash
  glitches remain in a few spots.
- llms docs: 32 of 50 tools still undocumented (authority note added; write
  the reference when the surface settles).
