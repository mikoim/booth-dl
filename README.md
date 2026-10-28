# booth-dl

Mirror the digital items in your [BOOTH](https://booth.pm/) library to a local directory.

- Downloads every file from purchases, gifts and free downloads, including every audio format of music items.
- Resumes interrupted downloads and skips files that are already complete.
- Verifies each file against the server's MD5 (ETag) and test-extracts zip, 7z, tar and tar.gz (including `.unitypackage`) archives.
- Saves each item's sales page: `item.json`, `item.html`, images and `description.md`.

## Usage

1. Log in to BOOTH in your browser.
2. Run `booth-dl login --from-browser <browser>` to read the session cookie from that browser.
   - Browsers: `firefox` (Linux and Windows); `chrome`, `chromium`, `brave` and `edge` (Linux only, using `secret-tool` from libsecret to unlock the cookie). These need a freedesktop Secret Service keyring (for example GNOME Keyring); KWallet is not supported.
   - Every profile is searched and the most recently used session wins. `--profile <name>` limits the search to one profile.
   - Alternatively, run `booth-dl login` and paste the value of the `_plaza_session_nktz7u` cookie. You can copy it from the developer tools: Application or Storage → Cookies → `https://booth.pm`.
   - The session is stored in `~/.config/booth-dl/session` (Linux) or `%APPDATA%\booth-dl\session` (Windows).
   - The `BOOTH_SESSION` environment variable overrides it.
3. Run `booth-dl sync --out ~/BOOTH`.
   - `--jobs N` (1–4, default 2) sets the number of parallel downloads.
   - `--check-updates` re-checks downloaded files against the server and refreshes sales pages.
4. Run `booth-dl verify --out ~/BOOTH` to re-check local files without network access.

Each item is stored as `<shop>/<item id>_<item name>/` with `files/`, `images/`, `item.json`, `item.html` and `description.md`. Progress is kept in `<out>/.booth-dl/state.json`.

The session cookie gives access to your BOOTH account. Keep it private.

## Shell completion

`booth-dl completions <shell>` prints a completion script for `bash`, `fish`, `powershell`, `zsh` or `elvish`.

- bash: `booth-dl completions bash > ~/.local/share/bash-completion/completions/booth-dl` (needs the bash-completion package)
- fish: `booth-dl completions fish > ~/.config/fish/completions/booth-dl.fish`
- PowerShell: add `booth-dl completions powershell | Out-String | Invoke-Expression` to your `$PROFILE`
- zsh: save `booth-dl completions zsh` as `_booth-dl` in a directory on your `$fpath`
- elvish: add `eval (booth-dl completions elvish | slurp)` to `~/.config/elvish/rc.elv`

Subcommands and options complete in every shell. fish does not complete the shell name after `completions`, and PowerShell does not complete option values such as the browser after `--from-browser`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
