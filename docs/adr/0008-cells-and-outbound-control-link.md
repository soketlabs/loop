# Scale with many Runtime cells behind a stateless Edge Gateway; remote control dials out

This ADR makes two decisions:

- Loop scales out as many Runtime cells. Each cell owns its Sessions by a
  fenced ownership lease, behind a stateless Edge Gateway, with Postgres and
  object storage.
- Remote control of any Runtime, local or cloud, goes through that Gateway on
  a connection that the Runtime opens. It never uses an inbound listener on
  the user's machine.

We chose this over one large Runtime and over TLS listeners on user machines.
The reasons are below.

## Cells

Loop scales by running **many small Runtime nodes (cells)**, not one large
Runtime.

**Ownership:**

- Each Session has exactly one owning cell at a time.
- A cell holds a Session by an **ownership lease** with a fencing epoch.
- Every write that the cell makes for the Session carries the epoch. The
  store refuses an epoch older than the current one, so a paused or
  partitioned old owner cannot write.

Today's `session_leases` (generation, heartbeat, check in the same
transaction) is the single-node form of this.

**Edge Gateway.** A stateless **Edge Gateway** sits in front of the cells. It
does these jobs:

- it terminates TLS;
- it authenticates;
- it applies rate limits;
- it routes each Session to its owner by reading the lease table.

The Edge Gateway holds no Session state.

**Storage:**

- The Event Log and state go to Postgres, partitioned by `tenant` and Session.
- Artifacts and old log segments go to object storage.

**Hibernation.** An idle Session is unloaded to the log. A later attach or
prompt wakes it on any cell that takes the lease. So cost follows active
turns, not the number of users.

**Failure.** When a cell fails, its leases expire. Another cell takes each
Session and starts from the log. Interrupted turns show as uncertain (the S2
rule).

**Personal use.** The same binary runs as one cell with SQLite.

We chose cells over a single large Runtime for three reasons:

- the single writer and the one broadcast channel are hard limits;
- one process would be the blast radius for every user;
- a large single Runtime cannot fail over.

We chose leases over static hashing of Sessions to nodes, because leases give
failover and rebalancing without a reshuffle. We chose Postgres and object
storage over a distributed log such as Kafka, because the volume targets do
not need one, and self-hosting Orgs and air-gapped sites can run Postgres.

## Remote control by an outbound link

Remote control of a Runtime from a phone or a browser uses the same Edge
Gateway, and the Runtime **dials out**.

1. **The outbound link.** A local or cloud Runtime opens an authenticated
   outbound `wss://` connection to the Edge Gateway. This is the **control
   link**. The Runtime checks the Gateway's certificate with the normal
   WebPKI or the Org's CA. The Runtime authenticates with a Runtime
   credential that it got when it registered.
2. **Pairing.** `loop runtime share` asks the Gateway for a pairing. The
   Runtime then shows a QR code and a URL. They hold a one-time code that
   expires in minutes and is used once. The QR also holds the Runtime's
   public key fingerprint (see the open question about end-to-end
   encryption).
3. **The device credential.** The phone or browser opens the URL and presents
   the code. The Gateway then gives that device a credential bound to this
   Runtime. The Runtime lists the paired devices and can revoke each one.
4. **No inbound port and no TLS certificate on user machines.** The Runtime
   keeps refusing every non-loopback bind, as it does today.
5. **Remote authority.** Traffic through the control link is treated like
   today's WebSocket traffic: remote authority, never host authority. These
   methods are refused through the link:
   - host-only methods (Runtime stop, provider and MCP credentials, tracing,
     saved defaults, usage history);
   - workspace trust writes.

   The Runtime enforces this. The Gateway is not trusted to do it.
6. **No third-party tunnels.** There is no dependency on ngrok, Cloudflare
   Tunnel, Tailscale Funnel or similar services. The Edge Gateway is part of
   open-source Loop. Soket runs one for the public service. An Org or an
   air-gapped site runs its own.
7. **A Capability.** `remote_control` is a Capability ([S4](../design/s4-policy.md)).
   It is off until the user pairs a device. Managed Policy or Org Policy can
   turn it off and Lock it.

We chose this over a TLS listener on the user's machine, which was the S3.8b
plan, for four reasons:

- a listener needs a certificate that phones trust;
- it needs an open inbound port through NAT and firewalls;
- it is a listener exposed to the network on a developer laptop;
- an air-gapped site cannot use public certificates.

We chose it over SSH tunnels, today's answer, because a phone cannot use them
with ease. We chose it over third-party tunnels because they add a
dependency, a party that sees the traffic, and a service that air-gapped
sites cannot reach. The same pattern is already in the vision for Connectors
(Relays connect out, ADR-0006). It also matches
`staging:docs/design/protocol-wire.md`: "Phase 2 adds a relay for URL/QR
control".

## Consequences

- The S3.8b rows in `roadmap.md` change. "Binding beyond loopback with TLS" is
  replaced by the control link and pairing. The roadmap update is a separate
  task.
- `vision.md` says "stateless Runtime nodes". It should say: Runtime nodes
  that hold no state except in-memory live Sessions under a lease. The vision
  update is a separate task.
- A paired device gets today's WebSocket Session authority. This includes
  `session/shell` and mode changes. That is a lot of power for a phone.
  Narrower device roles are an open question.
- The protocol needs location-free Session addresses, so the Gateway can route
  a Session without knowing where it ran before.
- The Gateway needs a pairing store, device credentials and revocation. The
  Runtime needs a control-link client with reconnect and backoff.
- An auto-started Runtime with a control link is not idle while a paired
  device is attached. The link alone does not keep it alive.
- Every record that arrives through the link carries the principal of the
  person who paired the device, with `via: device:<id>`
  ([ADR-0009](0009-identity-fields-from-s4.md)). A device is not a principal.
- **The Gateway is trusted for access.** The Gateway issues device
  credentials, and a device can reach `session/shell`. So whoever controls the
  Gateway can run commands on every Runtime linked to it. Three things limit
  this:
  - a Runtime accepts a device only after a pairing that the Runtime itself
    started;
  - it keeps its own list of paired devices and checks each request against
    that list;
  - end-to-end encryption, if adopted, would also keep the Gateway from
    reading or forging traffic.

  The remaining risk is stated plainly. A user who does not trust the hosted
  Gateway can run their own Gateway, or leave `remote_control` off.
- The cluster parts are proposals until their probes pass:
  - **N3:** fenced leases, 0 writes from a stale owner;
  - **N4:** Postgres throughput;
  - **N5:** wake time;
  - **N6:** microVM cost;
  - **H9:** partition and lease fencing.

## Open questions

- **End-to-end encryption.** Should the Gateway see plaintext? The proposal
  is that the QR carries the Runtime's key, and the device and the Runtime
  run an end-to-end channel inside the control link. Then a hosted Gateway
  sees only routing metadata. Cloud Runtimes behind the same operator may
  not need it. Does v1 ship this, or TLS to the Gateway only?
- **Device roles.** Should a paired device default to "watch and answer
  Escalations", with `session/shell`, mode changes and Session Policy changes
  needing a second, explicit grant?
- **Cells and the Gateway.** Should cells in a cluster also dial out to the
  Gateway, with one mechanism for both, or should the Gateway connect to
  cells inside the cluster network?
- **Names.** GLOSSARY uses **Relay** for a Connector inside an Org network
  and **Lease** for a Swarm claim. This ADR uses "control link",
  "Edge Gateway", "cell" and "ownership lease" to avoid a clash. Add these
  terms to GLOSSARY.
- **Registration.** How does a local Runtime register with a Gateway and get
  its Runtime credential: on first `loop runtime share`, or with a sign-in?
  Which Gateway does `share` use by default: Soket's hosted Gateway, or none
  until one is configured?
- **Direct TLS listener.** Is a direct `wss://` listener still needed
  anywhere? One example is an Org server behind its own reverse proxy.
