# Records demo/demo.gif. Run from the repository root:
#
#   vhs demo/demo.tape
#   gifsicle -b -O3 --colors 16 --lossy=40 demo/demo.gif
#
# The recording uses a throwaway HOME and a fake BOOTH (demo/fake-booth.py),
# so no real account, cookie or purchase ever appears.

Output demo/demo.gif

Require cargo
Require python3
Require curl
Require tree

Set Shell bash
Set FontSize 18
Set Width 1100
Set Height 560
Set Padding 20
Set TypingSpeed 40ms
Set Theme "Catppuccin Mocha"

Hide
Type `export PS1='\[\e[1;35m\]$\[\e[0m\] '` Enter
Type `export CARGO_TARGET_DIR="$PWD/target" DEMO="$PWD/demo"` Enter
Type `export DEMO_HOME="$(mktemp -d)" && export CARGO_INSTALL_ROOT="$DEMO_HOME/.local" PATH="$DEMO_HOME/.local/bin:$PATH"` Enter
# Build from a copy so that only throwaway paths appear on screen.
Type `mkdir "$DEMO_HOME/booth-dl" && cp -r Cargo.toml Cargo.lock src "$DEMO_HOME/booth-dl/" && cd "$DEMO_HOME/booth-dl"` Enter
Type `cargo build --release --locked --quiet` Enter
Wait@5m /\$$/
Type `python3 "$DEMO/fake-booth.py" "$DEMO_HOME" 8765 & export BOOTH_DL_BASE_URL=http://127.0.0.1:8765` Enter
Type `until curl -so /dev/null "$BOOTH_DL_BASE_URL"; do sleep 0.1; done` Enter
Type "clear" Enter
Show

Type "# Install" Sleep 300ms Enter
Type "cargo install --locked --path ." Sleep 500ms Enter
Wait+Screen@5m /Installed package/
Sleep 1.5s
Hide
Type `export HOME="$DEMO_HOME" && unset XDG_CONFIG_HOME XDG_DATA_HOME BOOTH_SESSION && cd ~` Enter
Type "clear" Enter
Show

Type "# Enable shell completion (add this to ~/.bashrc)" Sleep 300ms Enter
Type "source <(booth-dl completions bash)" Sleep 500ms Enter
Sleep 1s

Type "# Log in with the BOOTH session from your browser" Sleep 300ms Enter
Type "booth-dl lo" Sleep 300ms Tab Sleep 500ms
Type "--fr" Sleep 300ms Tab Sleep 500ms
Tab@300ms 2
Sleep 1.5s
Type "fi" Sleep 300ms Tab Sleep 500ms Enter
Wait+Screen /Logged in/
Sleep 2.5s
Type "clear" Enter

Type "# Download everything in your library" Sleep 300ms Enter
Type "booth-dl sync --out ~/BOOTH" Sleep 500ms Enter
Wait+Screen@1m /skipped 0, failed 0/
Sleep 2.5s
Type "# Run it again: finished files are skipped" Sleep 300ms Enter
Type "booth-dl sync --out ~/BOOTH" Sleep 500ms Enter
Wait+Screen@1m /skipped 4/
Sleep 2.5s
Type "clear" Enter

Type "# Files and sales pages, per shop and item" Sleep 300ms Enter
Type "tree --noreport -L 2 BOOTH" Sleep 500ms Enter
Sleep 2s
Type `tree --noreport "BOOTH/Sample Records/1003_Sample Album"` Sleep 500ms Enter
Sleep 3s
Type "clear" Enter

Type "# Re-check everything offline" Sleep 300ms Enter
Type "booth-dl verify --out ~/BOOTH" Sleep 500ms Enter
Wait+Screen /problems/
Sleep 3s

Hide
Type `kill %1; cd /; rm -rf "$DEMO_HOME"` Enter
