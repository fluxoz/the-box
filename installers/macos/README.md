# The Box on a Mac

There is deliberately **no `curl … | sh` takeover for macOS**, and that's an
honest limitation rather than a missing feature:

- **Apple Silicon isn't appliance-grade for this.** Running a wiped, headless
  Linux/NixOS appliance on Apple hardware is fragile and unsupported territory,
  and wiping a Mac to run Box OS is almost never what someone wants.
- **A laptop is a poor always-on server.** It sleeps, it moves, it's your
  daily driver.

So the Mac "equivalent" is **not** to convert the Mac. Instead:

- **Run a Box in a local Linux VM** (UTM, Lima, etc.) for development and to try
  things out, or
- **Point at a cheap Linux VPS** and use the one-liner there:
  `curl -fsSL https://thebox.build/install.sh | sudo sh`.

Your Mac stays your Mac; the Box runs where a server belongs. If real demand
appears for a native macOS story, it would be a separate design, not a wipe.
