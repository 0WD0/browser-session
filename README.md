# browser-session

`browser-session` is a small external helper and Emacs client for capturing a
restricted set of cookies from a browser session through Chromium DevTools.
It is provider-neutral: YouTube Music, X, and other consumers own their cookie
allowlists, page scripts, and credential schemas.

## Status

The initial implementation supports Chromium-compatible browsers through CDP.
It can attach to an existing local DevTools endpoint or start a browser with an
explicit profile directory. Firefox-family WebDriver BiDi support is planned
for the first real consumer that needs the extracted boundary.

The helper never reads browser cookie databases. It reads cookies through the
browser protocol, writes a versioned private capture file, and emits only
non-secret metadata in its JSON response. Cookie values never go to stdout,
stderr, or logs.

## Build

Build the helper and the Emacs client with:

```bash
make check
```

The helper binary is `target/debug/browser-session` during development.

## CLI

Capture selected cookies from a page:

```bash
browser-session capture \
  --url https://x.com \
  --cookie auth_token \
  --cookie ct0 \
  --output ~/.config/chirp/browser-session.json \
  --browser chrome \
  --profile-dir ~/.config/google-chrome
```

`--profile-dir` is a Chromium user-data directory, not the `Default/` child
profile directory. Close the browser before asking the helper to start a
profile that is already in use. An already-running browser is supported when
it exposes the configured local CDP port.

Optional `--script-file FILE` evaluates a provider-owned JavaScript expression
in the opened page and stores its JSON result in the private capture file.
The generic helper does not interpret that result.

## Emacs API

`browser-session.el` is a thin asynchronous client for the helper:

```elisp
(require 'browser-session)

(browser-session-capture
 :url "https://x.com"
 :cookies '("auth_token" "ct0")
 :output-file "~/.config/chirp/browser-session.json"
 :callback (lambda (metadata)
             (message "Captured %s" (alist-get 'browser metadata)))
 :errorback #'message)
```

The callback receives metadata, not cookie values. A consumer explicitly reads
the private output file with `browser-session-read`, validates its provider
fields, and removes it when its workflow no longer needs it.

## Consumer boundary

A provider adapter should look like:

```text
browser-session capture
        -> private generic capture file
        -> provider validation and mapping
        -> provider-specific auth state
```

Do not make this project emit a universal cookie dump or a shared credential
file. A provider may request only the cookie names and origin it needs.
