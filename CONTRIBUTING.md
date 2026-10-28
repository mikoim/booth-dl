# Contributing

## Running the tests

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

The tests never contact booth.pm or read your browsers; network tests use a
local test server and browser tests use temporary profiles.

## Recording the demo

The demo GIF in the README (`demo/demo.gif`) is recorded with
[VHS](https://github.com/charmbracelet/vhs) and shrunk with
[gifsicle](https://www.lcdf.org/gifsicle/). Run from the repository root:

```sh
vhs demo/demo.tape
gifsicle -b -O3 --colors 16 --lossy=40 demo/demo.gif
```

The recording needs `python3`, `curl` and `tree`. It runs in a throwaway home
directory against a fake BOOTH, `demo/fake-booth.py`, which serves sample items
and creates a Firefox profile holding a dummy session cookie. No real account,
cookie or purchase appears in the recording.

booth-dl talks to the fake BOOTH because the tape sets `BOOTH_DL_BASE_URL`.
That variable only accepts a loopback URL such as `http://127.0.0.1:8765`,
since the session cookie is sent to it.
