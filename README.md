# webbrowser-plugin-sicompass

*The web, in Sicompass.*

This plugin is part of [Sicompass](https://github.com/friendlyflow/sicompass), a
keyboard-first, accessibility-first way to use your entire computer.

The web browser shows a page as a list. Type an address in the address bar, and
the page comes back as headings, text, links and forms, in the order you would
read them. Right follows a link, forms are filled in place, and the addresses
you visited are rows under the address bar, which b bookmarks. It also renders
the web pages other programs link to.

Pages are rendered by a real Chrome, Chromium or Edge, which is not bundled, so
install one to use the browser. Chrome never appears on your screen. With Xvfb
installed it runs as a normal browser on an invisible display, which is what a
website expects to see. Without it Chrome runs headless, which a few sites
detect and block.

The browser says it starts Chrome and Xvfb, and nothing else. The Store shows
that before you install it. Like any program you install, it runs with your
rights. It works on Linux, macOS and Windows. On macOS and Windows Chrome
always runs headless.

## Install

In Sicompass, open store, then programs, and press Enter on install next to
webbrowser. The Store checks the release's signature before installing it, and
keeps it up to date.

To install a build of your own instead, copy `plugin.json`, the built
`plugin` program (`plugin.exe` on Windows) and `locales/` into a folder named
`webbrowser` in the Sicompass plugins folder (`~/.config/sicompass/plugins/` on
Linux, `~/Library/Application Support/sicompass/plugins/` on macOS) and
restart Sicompass.

## Building from source

```bash
nix develop          # the toolchain
cargo test           # no Chrome needed
cargo test -- --ignored --test-threads=1   # against a real Chrome
cargo build --release
cp target/release/webbrowser-plugin plugin
```

`./scripts/release-plugin.sh --dry-run` builds this computer's release, packs
it, and signs and verifies it with a throwaway key, the way a release is made.

## Related repositories

- [sicompass](https://github.com/friendlyflow/sicompass), the application
- [sicompass-plugin-sdk](https://github.com/friendlyflow/sicompass-plugin-sdk),
  the SDK, the plugin kit and the cloud backup library

## Community

Join the conversation on
[Discord](https://discord.com/channels/1464152138753249313/1464152139231137894).

## License

#### Open source license

If you are creating an open source application under a license compatible with
the GNU GPL license v3, you may use this project under the terms of the GPLv3.
See [LICENSE](LICENSE).

## Contributing

Contributions are welcome. Whether it is code, documentation, or feedback, your
input helps make computing more accessible for everyone.
