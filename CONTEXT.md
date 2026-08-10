# Domain Context

## Browser session

An authenticated state held by a user's browser for a particular web origin.
It is the source material that a consumer may use to establish its own
provider credential.

## Session capture

A deliberate handoff of selected browser-session material to one consumer.
Capture is scoped to an origin and an explicit set of fields; it is not a
request to export a browser's complete state.

## Provider credential

The consumer-specific representation derived from a browser session. A
provider credential belongs to its consumer and is not interchangeable with
the browser session or another provider's credential.

## Provider adapter

The consumer-owned boundary that selects fields from a session capture,
validates their meaning, and maps them to the provider credential.
