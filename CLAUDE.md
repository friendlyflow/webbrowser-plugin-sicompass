# Project Instructions

webbrowser_plugin_sicompass was split out of the
[sicompass](https://github.com/friendlyflow/sicompass) workspace, and its git
history before that point is the history of `lib/lib_webbrowser` (earlier
`lib/lib_webbrowser-rs`, and the C tests in `tests/lib_webbrowser`) there.
Work on it is usually driven from a sicompass checkout next to this one
(`../sicompass`), whose `/commit-and-push`, `/release`, `/sync` and
`/update-cargo` take this repo's name as their first argument and then follow
the skills in this repo's `.claude/skills/`.

It is a sicompass **WASM plugin**: a `cdylib` built for `wasm32-wasip2` with
`sicompass-pdk`, installed by the sicompass Store from this repo's GitHub
releases. The plugin platform is described in
`../sicompass/docs/plugin-platform.md` and `../sicompass/docs/wasm-plugins.md`.

- `plugin.json` is the manifest. Its `name` is `webbrowser` and its
  `displayName` `web browser` is the settings section (the key the built-in
  had, `urlHistorySize`, so a saved value carries over). It asks for
  `storage` and, under `process`, the names Chrome goes by and `Xvfb`, which
  `cdp::CHROME_NAMES` and `cdp::XVFB` must match (a test checks). It says
  `"rendersPages": true`: the host asks it to render the pages other programs
  link to.
- `locales/<lang>.ftl`, every id prefixed `webbrowser-`, in all four
  languages.

## The sandbox, and what it changes

A call into the plugin's UI instance has ten seconds, and a page load can take
thirty, so Chrome is never driven from there:

- **Chrome** is started with `process.child.spawn-with-channel` and
  `--remote-debugging-pipe`: the DevTools protocol on file descriptors 3 and
  4, no network port. `cdp.rs` is a small blocking client for exactly the
  calls the browser makes (a tab with a flat session, navigate and wait for
  the load event, evaluate, viewport, cookies, close). It replaced
  chromiumoxide. Natively (the live tests) it makes the pipes itself.
- **Off the screen:** with Xvfb available the plugin starts it
  (`-displayfd 1`: it picks a free display and says which on stdout) and runs
  Chrome headed on it, which is what sites that turn headless Chrome away
  accept. Without Xvfb, Chrome runs headless. Either way Chrome gets an
  accessibility bus address that goes nowhere, so the screen reader never
  sees it.
- **The browser task** (`worker.rs`, `worker::BROWSER_TASK`) holds Chrome and
  the reader's tab for the plugin's life. The UI sends it `Job`s through the
  task inbox and it answers `Done`s, JSON both ways. Navigations are numbered:
  an answer for one the user typed past is dropped, and a navigation queued
  behind a newer one is skipped. Natively the same loop is a thread.
- **Pages for links** (`render_url`, the host's `sicompass:render-url`) load
  in a tab of their own in the same Chrome, and go back to the host with
  `host.page_rendered`.
- **Chrome's profile** is `/storage/chrome/profile`. Chrome runs outside the
  sandbox and the host translates only a working directory, so Chrome starts
  in `/storage/chrome` with a relative `--user-data-dir`. The app moved the
  built-in's profile and URL history there.
- **The URL history** is `/storage/history`.
- Windows is not supported yet: the host's process channel is Unix-only.

## Environment (Nix)

The toolchain comes from the flake dev shell in [flake.nix](flake.nix): Rust
from rust-overlay with the `wasm32-wasip2` target (nixpkgs' rustc has no `std`
for it), `wasm-tools` and `jq`. Nothing is installed system-wide. Chrome and
Xvfb are not in it: the live tests use the ones installed on the machine.

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
  `cargo test` (natively), and `./scripts/release-plugin.sh --dry-run`, which
  also builds the component and audits its imports.
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

The SDK and the pdk (in `../sicompass-plugin-sdk`) come by git at one rev
until they are on crates.io. The commented-out
`[patch]` in `Cargo.toml` is for working on them together, and stays commented
on main.
