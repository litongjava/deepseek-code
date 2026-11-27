---
name: Testing reqwest proxy routing for public HTTPS pages
description: How to prove a public HTTPS WebFetch request reached a configured proxy without network access
metadata:
  type: reference
---

Symptom: a unit test must prove that a public page fetch is routed through a
configured proxy, but the page URL is upgraded to HTTPS and the mock proxy
cannot terminate TLS.

Cause: `reqwest::Proxy::all` sends `CONNECT host:443` for HTTPS targets, so a
plain mock proxy never sees an absolute-form GET and cannot return the page.

Fix: record the proxy's request line and reply `403` to the `CONNECT`. The
fetch then fails (and a reader fallback also reaches the same proxy), but the
recorded `public.example:443` target proves routing occurred without any
external network or TLS fixture. The same reasoning means local/private
targets must be tested with `no_proxy()`: an explicit proxy on a dead port
must not affect them.
