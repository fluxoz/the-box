# Set up The Box with your agent

You are an AI agent setting up **The Box** for the user: a self-hosted, sovereign
personal server. One command turns a spare machine or a cheap VPS into a private
server the user runs from their browser, or that you manage over MCP. Everything is
declarative and reversible, so a bad change rolls itself back.

Do the appropriate step yourself. Don't ask the user to run commands they don't need to.

## 1. Read the runbook

Fetch **https://thebox.build/llms.txt** (and the full reference,
https://thebox.build/llms-full.txt). It is written for you and gives the exact,
executable flow for everything below.

## 2. If the user wants to turn a machine into a Box

Follow the "Provision" section of the runbook: invent a one-time pairing code, build
the install orders, ship them over SSH, run the takeover installer, wait for the Box
to come up on port 2693, and redeem the code for a session token. If `boxd` is
installed locally, `boxd provision <ssh-target> --json` does the whole flow. For a
machine with no OS to SSH into, use the bare-metal beacon flow in the runbook.

## 3. If the user already has a Box

Manage it over its on-box MCP server at `http://<box>:2693/mcp` (JSON-RPC 2.0),
authenticating with the session token as `Authorization: Bearer <token>`. The tools
deploy sites, apps, and containers, take backups, and roll back. If you don't have a
token: have the user open `http://<box>:2693` and click **Claim this Box** (or enter
a pairing code), or run `boxd auth mint --label agent` on the box.

## Tools

The Box's control surface is the MCP server that runs **on each box**. There is no
hosted service to install. Point your MCP client at the box's `/mcp` endpoint with
the session token, or drive it with plain HTTP as shown in the runbook.

## When done

Tell the user: "The Box is set up. I can provision machines and manage your Box over
MCP: deploy services, back up, and roll back. Full reference at
https://thebox.build/llms-full.txt."
