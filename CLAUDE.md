# Project Instructions

webbrowser-plugin-sicompass was split out of the
[sicompass](https://github.com/friendlyflow/sicompass) workspace, and its git
history before that point is the history of `lib/lib_webbrowser` (earlier
`lib/lib_webbrowser-rs`, and the C tests in `tests/lib_webbrowser`) there.
Work on it is usually driven from a sicompass checkout next to this one
(`../sicompass`), whose `/commit-and-push`, `/release`, `/sync` and
`/update-cargo` take this repo's name as their first argument and then follow
the skills in this repo's `.claude/skills/`.

It is a sicompass **plugin process**: a program (`src/main.rs`) built with the
SDK's `plugin` feature, which sicompass starts and talks to over its stdin and
stdout. It runs with the user's rights. The Store installs it from this repo's
GitHub releases, one build per platform. The plugin platform is described in
`../sicompass/docs/plugin-platform.md` and `../sicompass/docs/process-plugins.md`.

- `plugin.json` is the manifest. Its `name` is `webbrowser` and its
  `displayName` `web browser` is the settings section (the key the built-in
  had, `urlHistorySize`, so a saved value carries over). It asks for
  `storage` and, under `process`, the names Chrome goes by and `Xvfb`, which
  `cdp::CHROME_NAMES` and `cdp::XVFB` must match (a test checks). They are
  what the plugin declares it does, shown to the user before install. It says
  `"rendersPages": true`: the app asks it to render the pages other programs
  link to.
- `locales/<lang>.ftl`, every id prefixed `webbrowser-`, in all four
  languages.

## How it runs

A call from the app has ten seconds, and a page load can take thirty, so
Chrome is never driven from a call:

- **Chrome** is found by name (`src/program.rs`): on `PATH`, then
  `~/.local/bin`, then on macOS `/Applications` and `~/Applications`
  (`<name>.app/Contents/MacOS/<name>`), and on Windows the browsers' install
  folders under Program Files and LocalAppData. It is started with
  `sicompass_sdk::plugin::command`.
- **The protocol** (`cdp.rs`, a small blocking client for exactly the calls
  the browser makes: a tab with a flat session, navigate and wait for the load
  event, evaluate, viewport, cookies, close). On Unix it goes over
  `--remote-debugging-pipe`, file descriptors 3 and 4 made here, with no
  network port. On Windows, which has no descriptors 3 and 4 to give a
  program, Chrome gets `--remote-debugging-port=0`, writes the port it took to
  `DevToolsActivePort` in its profile, and the protocol goes over a websocket
  on 127.0.0.1 (`tungstenite`, plain `ws://`). The ignored test
  `chrome_answers_over_its_devtools_port` runs that path on Linux too.
- **Off the screen:** on Linux with Xvfb available the plugin starts it
  (`-displayfd 1`: it picks a free display and says which on stdout) and runs
  Chrome headed on it, which is what sites that turn headless Chrome away
  accept. Without Xvfb, and always on macOS and Windows (where a headed Chrome
  would be a window on the user's screen), Chrome runs headless. Either way
  Chrome gets an accessibility bus address that goes nowhere, so the screen
  reader never sees it.
- **The browser thread** (`worker.rs`) holds Chrome and the reader's tab for
  the plugin's life. The UI sends it `Job`s over a channel and takes its
  `Done`s in `poll`. Navigations are numbered: an answer for one the user
  typed past is dropped, and a navigation queued behind a newer one is
  skipped. A thread that dies fails the load in flight.
- **Pages for links** (`render_url`) load in a tab of their own in the same
  Chrome, and the browser thread hands them to the app itself with
  `host::page_rendered`.
- **Stopping Chrome.** Every program started is registered in a
  `cdp::Children`. Dropping the `Worker` (in `cleanup`, which the runtime
  calls when the app closes the plugin's stdin, and on drop) asks the thread
  to close Chrome, and when the thread is busy with a load it stops Chrome and
  Xvfb from outside (`SIGTERM`, then kill), all inside the runtime's two
  seconds. Behind that: Chrome exits when its pipe closes, Xvfb has
  `-terminate`, and on Linux both get `PR_SET_PDEATHSIG`. A Chrome left behind
  is the worst outcome here.
- **Chrome's profile** is `chrome/profile` in the plugin's storage folder
  (`sicompass_sdk::plugin::storage_dir`), and **the URL history** is `history`
  there. Outside sicompass (the tests) the profile is a throwaway folder under
  the temp folder and the history is never written (`TEST_NO_HISTORY`).
- **Strings** come from the app (`host::translate`). The unit tests run
  outside sicompass and read the English bundle instead (`src/localize.rs`).
- stdout is the channel to the app. `println!` lands in stderr, the app's log.

## Environment (Nix)

The toolchain comes from the flake dev shell in [flake.nix](flake.nix): Rust
from rust-overlay with this computer's plugin target (static musl on Linux,
which nixpkgs' rustc has no `std` for) and `jq`. Nothing is installed
system-wide. Chrome and Xvfb are not in it: the live tests use the ones
installed on the machine.

- **Check once per session**, then stick with the answer: `command -v cargo`.
  - Non-empty: the shell is inside `nix develop`, so run `cargo ...` directly.
  - Empty: prefix every toolchain command with `nix develop -c`.
- `nix develop -c <cmd>` prints a `warning: Git tree ... is dirty` line on
  stderr first. That warning is noise, not a failure.
- Evaluate the flake through `git+file://$PWD`, never a plain path (a plain path
  copies `target/` into the store and hangs), and always under `timeout`.
- The version lives in `plugin.json` and in `[package] version` in `Cargo.toml`.
  Bump both together.

## Generated files that are committed

- `THIRD-PARTY-LICENSES.html`: `cargo about generate about.hbs -o
  THIRD-PARTY-LICENSES.html` (cargo-about 0.9.2, the version the `licenses.yml`
  workflow pins). Regenerate and commit it with any dependency change. The
  workflow fails if it drifts.

## Code Style

Follow standard Rust idioms. Use `#[allow(...)]` sparingly and only when
justified. In `README.md`, do not use em dashes or semicolons. Use commas
instead, or split into separate sentences.

## Testing

- After implementing changes, always run the tests before finishing:
  `cargo test`, and `./scripts/release-plugin.sh --dry-run`, which also builds
  this computer's release and verifies it the way the Store will.
- The live tests (`#[ignore]`) start a real Chrome: `cargo test -- --ignored
  --test-threads=1`, one at a time. Each closes its own Chrome and Xvfb, and
  afterwards none should be left (`ps -eo args | grep remote-debugging-pipe`).
  Never stop Chrome by name: the user's own browser is Chrome too. The four
  named `live_*` and `test_chromium_fetches_real_cloudflare_site` reach real
  sites, which change: `live_bpost_answers_cookies_then_shows_content` fails
  since bpost stopped showing its banner to a fresh profile (2026-09),
  with the old built-in browser as well.
- When adding new code, write or update tests.
- If tests fail, fix the code. Never leave a task with failing tests.

## Test Integrity

- Never remove or weaken test assertions to make a failing test pass. Fix the
  code instead.
- If a test itself is genuinely wrong and needs changing, **ask the user
  first** before modifying it.

## Releasing

A release is a `vX.Y.Z` tag on `main`, equal to `plugin.json`'s version. See
`.claude/skills/release/SKILL.md`. Before tagging, run
`nix develop -c ./scripts/release-plugin.sh --dry-run` (needs the
`sicompass-plugin` tool: `cargo install --git
https://github.com/friendlyflow/sicompass-plugin-sdk sicompass-plugin`). The
release workflow signs with the `PLUGIN_SIGNING_KEY` secret and checks it
against the `PLUGIN_PUBLIC_KEY` variable, the key the sicompass store list
names. The secret key file is `~/.config/sicompass/plugin-keys/webbrowser.key`
on the maintainer's machine. Never print, copy or commit it.

The SDK comes from crates.io (the source is `../sicompass-plugin-sdk`). The
commented-out `[patch]` in `Cargo.toml` is for working on them together, and
stays commented on main.

A release has one archive per platform. The release workflow builds them on
five runners (Linux x86_64 and arm64 as static musl, macOS arm64 and x86_64,
Windows x86_64), then packs, signs and verifies them in one job.
