# `principal` and `tenant` go into the protocol, the Event Log and Policy decisions in S4

From S4, Loop records two identity fields:

- **`principal`**: the person or Service Identity on whose behalf something
  happens.
- **`tenant`**: the isolation boundary the data belongs to.

The fields go into:

- the protocol;
- every Event Log record;
- every Policy decision record.

**The Runtime stamps both fields** from the authenticated connection, as it
already does for host authority. It never takes them from a request field.

**Shape:**

- A principal is `{ kind: person | service, issuer, id }`. The pair
  `(issuer, id)` is stable, so a later OIDC `(iss, sub)` fits without a
  change.
- A tenant is an opaque string.

OIDC sign-in, SCIM, Groups, RBAC and Admin stay in roadmap step 5. Only the
fields and their plumbing move forward.

We chose this over adding identity in step 5. Adding it later means:

- a protocol break;
- a backfill of logs that cannot say who acted;
- a review of every query for tenant scope at the worst time.

We chose it over a free-form `user` string, because a free-form string cannot
carry a Service Identity or an issuer. We chose it over a tenant that exists
only as the database or file path, because then a cross-tenant bug leaves no
trace in the records.

## Who the principal is

| Case | `principal` | `tenant` |
|---|---|---|
| Unix socket or embedded (host authority) | `{person, local, uid:<uid>}` of the OS user that owns the Runtime | `local` |
| Loopback WebSocket with the bearer token | the same local person, `via: token` | `local` |
| A device paired through the control link ([ADR-0008](0008-cells-and-outbound-control-link.md)) | the local person who paired it, `via: device:<id>` | `local` |
| Web v1 with Google sign-in | `{person, https://accounts.google.com, <sub>}` | one per account |
| A Subagent, a `/workflow` task, an ACP agent | the principal of the person who started the parent Session ("agents act as the person who started the Session"); `actor` names the Session or agent | inherited |
| Unattended work (later) | `{service, <issuer>, <id>}` | the owning tenant |
| An Escalation answer | the principal of the Surface that answered (`answered_by`), which can differ from the Session's principal | the Session's tenant |

`via` and `actor` are attributes of the record. They are not principals. A
device or an agent never becomes a principal of its own.

## Consequences

- **Protocol.** The `initialize` result tells the client its `principal` and
  `tenant`. The fields are optional on the wire, so an old client works with
  a new Runtime, and a new client works with an old one (N8). A request field
  that names a principal is ignored.
- **Event Log.** Every Event gets `principal`, `tenant`, and optional `via`
  and `actor`. A migration adds the columns, and old rows read as `local`.
  Postgres partitions by `tenant` (ADR-0008). The store API takes the tenant
  on every read, so a query without a tenant does not compile.
- **Policy.** Every decision record carries both fields
  ([S4 §12](../design/s4-policy.md#12-the-decision-record)). The Org and Group
  layers will be keyed by tenant and Group when they get a source.
- **Ownership.** A Session belongs to its principal. In S4 there is one local
  person, so nothing changes for users. With more than one principal, attach
  by another principal is refused unless the Session is shared. Sharing comes
  later.
- **Privacy.** Principal ids are personal data. They follow the retention and
  export rules of the Event Log. Logs and SIEM export show the `(issuer, id)`
  pair, not names or emails.
- **Cost now.** The cost is small: two fields stamped in one place per
  transport, one migration, and tests that every record has them (the E3
  harness checks decision records).

## Open questions

- The local principal id: `uid:<n>`, the user name, or a random id made at
  install? A uid is not stable across machines.
- Should `tenant` be in every `UpdateEnvelope` on the wire, or only in the
  `initialize` result and the stored records?
- Does a paired device need a principal of its own, for example a shared
  family tablet, or is `via` enough until multi-user Runtimes exist?
