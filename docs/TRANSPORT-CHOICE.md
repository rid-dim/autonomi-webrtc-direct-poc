# Transport choice: WebRTC-Direct now, WebTransport only as the escape hatch

Two browser transports can reach a bare IP address with a self-signed certificate and no
CA. This page states the recommendation and its reasoning plainly, because the certificate
model — not throughput, not API elegance — is what actually decides between them.

**Recommendation: WebRTC-Direct with a static certificate.** WebTransport is the fallback
to keep in the drawer, and if it is ever taken out of the drawer, certificate
pre-generation (below) is not an optimization but an absolute requirement.

## Why WebRTC-Direct wins on simplicity

Remember the frame from [ARCHITECTURE.md](ARCHITECTURE.md): the transport is an untrusted
tunnel, morally equivalent to UDP. All security lives above it. That reframes the
certificate question entirely — the certificate is not a security instrument here, it is a
**connection identifier**. And for a connection identifier, you want exactly one property:
that it never changes.

WebRTC-Direct delivers that:

- **One static self-signed certificate, valid indefinitely.** Generate it once, keep it
  next to the node's identity key, done. The certificate's hash (`certhash`) is stable for
  as long as the node keeps its key.
- **The node's address becomes a durable fact.** `ip:port + certhash` can be written down,
  put in a bootstrap list, hard-coded in a page, handed to someone on paper — and it still
  works months later. This is exactly how Autonomi's `ip:port` bootstrap entries already
  behave; the certhash just comes along for the ride.
- **Zero certificate operations.** No rotation, no re-publishing, no clock-sensitive
  validity windows, no infrastructure that has to stay awake so that connections keep
  working. A node that was offline for a year comes back with the same address.
- **No extra port.** QUIC and WebRTC can share the node's existing UDP port, demultiplexed
  by first byte (RFC 9443, see [FINDINGS.md](FINDINGS.md)). DTLS and QUIC are separate
  stacks with separate TLS configurations, so the node keeps raw public keys and
  post-quantum crypto on the QUIC side while showing browsers an ordinary self-signed
  certificate. One firewall rule, existing bootstrap lists valid unchanged.
- **Shipping in every current browser** — Chrome, Firefox, Safari — and proven against the
  live Autonomi network by this proof of concept.

The known objection is that WebRTC-Direct depends on SDP munging, which the standard
frowns upon. That risk is real but abstract: no browser has announced a restriction, no
timeline exists, and libp2p's entire browser story sits on the same mechanism — there
would be loud, early warning. Meanwhile the layer model makes the cost of being wrong
small: if browsers ever did restrict it, the tunnel gets swapped and nothing above it
changes.

## WebTransport: the escape hatch, and what going there would demand

WebTransport reaches a bare IP the same way (`serverCertificateHashes`), rides on real
QUIC, and is standards-track — on paper the better-behaved option. The catch is a single
constraint with large operational consequences:

> A pinned WebTransport certificate must be **ECDSA P-256 and valid for at most two
> weeks.**

So the connection identifier expires, by design, every 14 days — forever. A node's
"address" is no longer a durable fact but a moving target that must be continuously
re-published. And that breaks precisely the scenario a decentralized network must survive:
a client that has been offline for longer than two weeks holds only expired hashes,
**cannot connect to anything**, and has to fetch fresh bootstrap contacts by hand from
some out-of-band source. For a network whose bootstrap model is "an `ip:port` list that
stays valid", that is a regression, not a detail.

### If you go this way anyway: pre-generated certificate schedules are a MUST

There is a way to make WebTransport survivable, and if the WebTransport route is ever
taken seriously, this is not optional hardening — it is a hard prerequisite:

1. **Each node pre-generates its certificates far into the future** — one to two years'
   worth, i.e. roughly 26–52+ consecutive ECDSA P-256 certificates, each with a ≤14-day
   validity window, windows overlapping slightly so there is never a gap. The keypairs and
   certificates exist *now*; only their validity windows lie in the future.
2. **The full hash schedule is published as part of the node's contact info** — the list
   of `(certhash, notBefore, notAfter)` tuples travels wherever `ip:port` travels today:
   into the bootstrap cache, into peer exchange between nodes.
3. **Nodes keep each other's schedules fresh.** As part of normal gossip, nodes refresh
   the tail of the schedules they hold, so any healthy participant always carries a year+
   of future reachability for its peers.
4. **A returning client just picks the hash that is valid *today*.** Offline for a month
   or a year — irrelevant, as long as the outage is shorter than the pre-generated
   horizon. It looks up the current window in its cached schedule and connects, no manual
   bootstrap copying, no out-of-band rescue.

This works because browsers accept a *list* of certificate hashes, and because nothing
requires a certificate's validity window to start at generation time. But be clear about
what it costs: every node now runs certificate lifecycle machinery forever, the bootstrap
cache format grows a time dimension, gossip carries and refreshes schedules, and a
node that loses its pre-generated keys loses its future addresses. All of that
infrastructure buys back a property — durable addresses — **that WebRTC-Direct has for
free with one static certificate.**

## Decision in one line

Treat the transport as the untrusted UDP-equivalent it is: take the transport with the
permanent address and zero moving parts (WebRTC-Direct, static certificate), keep
`LinkTransport` as the seam, and hold WebTransport-with-pre-generated-schedules in reserve
for a browser-policy change that is currently neither announced nor dated.
