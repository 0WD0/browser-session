# browser-session

`browser-session` is a small external helper and Emacs client for capturing
provider-scoped browser session material through Chromium DevTools or Firefox
WebDriver BiDi. It is provider-neutral: YouTube Music, X, and other consumers
own cookie requirements, page scripts, validation, and credential schemas.

## Boundary

The helper never reads browser cookie databases. It reads session material
through the browser's remote-control protocol, writes a versioned private
capture file, and emits only non-secret metadata in its JSON response. Cookie
values never go to stdout, stderr, or logs.

The Emacs API deliberately supports two consumer styles:

- use the metadata callback and pass the private capture-file path to an
  external provider helper; or
- explicitly call `browser-session-read` when the consumer intentionally needs
  to handle its private session material in Elisp.

The second style is a capability, not an implicit callback behavior. Consumers
must not persist the returned values or include them in diagnostics.

## Build

Build and verify the helper and Emacs client with:

```bash
make check
```

The development binary is `target/debug/browser-session`.

## CLI

Capture individually required cookies:

```bash
browser-session capture \
  --url https://x.com \
  --cookie auth_token \
  --cookie ct0 \
  --output ~/.config/chirp/browser-session.json \
  --browser chrome \
  --profile-dir ~/.config/google-chrome
```

Use `--any-cookie` to require one name from an alternative group. A provider
that needs a complete, origin-scoped session may explicitly use
`--all-origin-cookies`; the helper still filters to the requested URL's host
and unexpired cookies, including path-scoped cookies for that origin. It never
captures a browser-wide cookie dump. A provider that needs only a page result
may omit cookie requirements when it supplies `--script-file` or
`--script-stdin`.

`--profile-dir` means a Chromium user-data directory for Chromium-compatible
browsers and a profile directory for Firefox or Zen. `--profile-root` asks the
helper to use automatic isolated profiles below that root for Chrome, Firefox,
or Zen. It uses `login-profile`, `login-profile-firefox`, or
`login-profile-zen`, respectively.

On Unix, automatic profiles are created and maintained with mode `0700`, must
be owned by the current user, and cannot be symbolic links. The helper does not
change permissions on a profile supplied explicitly with `--profile-dir`.

By default the helper reserves a dynamic loopback port and starts the requested
browser itself. It never reuses a pre-existing endpoint implicitly. Reusing the
same profile root preserves browser login across captures, and the helper
closes only an isolated browser instance it started after allowing it to flush
its persistent profile.

Attaching to an endpoint is an explicit, non-isolated operation:

```bash
browser-session capture --attach-existing --port 9222 \
  --browser chrome --url https://example.com \
  --cookie session --output /tmp/session.json
```

Attach mode cannot be combined with profile, proxy, or restart options and
cannot prove profile identity. Source metadata comes from the endpoint. If a
normal running browser prevents a managed launch, the helper returns the
structured `browser-restart-required` error; `--restart-running` permits its
explicit macOS restart path. Firefox and Zen use WebDriver BiDi;
Chromium-compatible browsers use CDP.

`--script-file FILE` evaluates a provider-owned JavaScript expression from a
file. `--script-stdin` reads the expression from standard input without placing
multiline code in argv:

```bash
printf '%s' '(() => window.providerState || null)()' |
  browser-session capture --url https://example.com \
    --script-stdin --output /tmp/provider-session.json
```

The two script sources are mutually exclusive. The result is stored in the
private capture file. A script should return `null` until its required page
state is ready, so the helper continues waiting. The generic helper does not
interpret non-null page results. A page script may be the capture's sole
completion requirement, in which case the capture contains an empty `cookies`
array.

Prepare an ordinary Firefox-family isolated-profile login before a BiDi
capture:

```bash
browser-session prepare-profile \
  --url https://music.youtube.com \
  --browser firefox \
  --profile-root ~/.ytm-radio
```

The command opens the profile without remote control and waits for the user to
sign in and close it. The provider can then start its normal capture flow.

## Emacs API

`browser-session.el` starts the helper asynchronously:

```elisp
(require 'browser-session)

(browser-session-capture
 :url "https://x.com"
 :cookies '("auth_token" "ct0")
 :output-file "~/.config/chirp/browser-session.json"
 :callback (lambda (metadata)
             (message "Captured %s" (alist-get 'browser metadata)))
 :errorback (lambda (error)
              (message "%s" (browser-session-error-message error))))
```

Providers may pass an expression directly with `:script`; the Emacs client
sends it through helper stdin. `:script` and `:script-file` are mutually
exclusive.

`browser-session-capture` returns a `browser-session-request`. Providers may
call `browser-session-cancel` to interrupt it; the error callback receives the
stable `cancelled` code after the helper has closed an isolated browser it
started. `browser-session-page` returns the provider-owned page result from a
validated private capture. Page results may contain secrets and follow the same
non-persistence and non-diagnostic rules as cookie values.

The callback receives metadata, not cookie values. `ERRORBACK` receives a
structured alist; use `browser-session-error-code` for routing decisions such
as a deliberate restart confirmation. The Emacs client defaults
`browser-session-profile-root` to `(locate-user-emacs-file "browser-session/")`,
giving Chrome, Firefox, and Zen stable isolated profiles. Set it to nil only
when intentionally using a browser's normal profile.

`browser-session-prepare-profile` exposes the Firefox/Zen preparation command
with the same asynchronous callback conventions.

## Consumer boundary

A provider adapter should look like:

```text
browser-session capture
        -> private generic capture file
        -> provider validation and mapping
        -> provider-specific auth state
```

Do not make this project emit a shared credential file. A provider controls its
own origin and either an explicit cookie list or the exceptional complete
origin-scoped capture it genuinely requires.
