# ADR 0001: Keep browser capture outside the Emacs process

## Status

Accepted.

## Context

Both ytm-radio and Chirp need a way to obtain a small, provider-specific set
of cookies from a browser session. Browser discovery, CDP/BiDi messaging,
profile lifecycle, and cross-platform process handling are not Emacs UI
responsibilities. The captured values are also secrets and must not enter
stdout, logs, or durable Emacs state accidentally.

A Rust dynamic module would provide a convenient direct API to Emacs, but it
would move browser protocol failures and native-code crashes into the Emacs
process and require a platform/Emacs ABI distribution matrix. It would also
conflict with ytm-radio's existing external-helper boundary.

## Decision

Ship a provider-neutral Rust library and short-lived CLI helper. Provide a
thin `browser-session.el` client that starts the helper asynchronously and
reads private, versioned capture files when a consumer explicitly asks for
them.

The shared boundary owns browser capture. Consumers own cookie allowlists,
page expressions, validation, and provider-specific credentials. Capture files
are private and contain only the fields requested by one consumer.

## Consequences

Consumer packages depend on one Emacs API instead of wrapping a helper
process independently. Browser protocol and native-code failures remain
outside Emacs. The project must ship helper binaries or provide an explicit
build/install path, and consumers still need small provider adapters.

A dynamic module can be reconsidered only if the external process boundary
proves insufficient for a concrete consumer and the distribution and crash
trade-offs are accepted explicitly.
