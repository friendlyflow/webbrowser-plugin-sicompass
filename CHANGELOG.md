# Changelog

## 0.3.0

Web Browser is a program of its own now, instead of a sandboxed WebAssembly component.
Sicompass starts it and talks to it, one per tab, and it runs with your rights, so
its entry in the Store says what it does before you install it, and installing it is
your approval.

- It starts your Chrome, Chromium or Edge. A second tab that wants the browser now says it is open in another tab, instead of starting a second Chrome on the same profile.
- One build for each of Linux (x86_64 and arm64, static), macOS (Apple Silicon and
  Intel) and Windows.
- Needs a Sicompass that runs plugin programs. An older Sicompass keeps the 0.2
  version it has.
