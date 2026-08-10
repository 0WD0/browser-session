# AGENTS.md

## Boundary

`browser-session` owns browser discovery, local browser process management,
CDP session capture, and private versioned capture files. It does not know
YouTube, X, or any other provider's authentication schema.

Provider packages own cookie allowlists, page scripts, validation, and mapping
to their own credentials. Do not add provider-specific logic here.

Do not read browser cookie databases or duplicate browser-specific encryption.
Cookie values must never appear in stdout, stderr, logs, fixtures, or test
failure messages. Capture files are private and must be written with mode
`0600` on Unix.

## Rust

Run `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`.

## Emacs Lisp

Every `.el` file uses lexical binding. Public symbols use the
`browser-session-` prefix and private symbols use `browser-session--`.
Public commands and functions need docstrings and user-facing commands need
`;;;###autoload`.

## Scope

Keep the generic browser boundary small. Add a protocol or browser feature
only when a real consumer needs it and its contract can be tested without a
live browser.
