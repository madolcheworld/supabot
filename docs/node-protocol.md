# SwarmBot node protocol (v1)

How a **node** (the hands: `swarmbot-node`, or any program in any language) talks
to the **brain** (`swarmbot-brain`). Written to be language-neutral: a node can be
implemented from this page alone.

`PROTOCOL_VERSION = 1` (`swarmbot_contracts::PROTOCOL_VERSION`, re-exported by the
crate so there is exactly one definition). The brain also accepts
`PROTOCOL_VERSION - 1`. Binding decisions: [`decisions.md`](decisions.md)
D1.1–D1.5. This is a deliberate subset of OpenBot's node protocol v1
(`../../dbot/docs/node-protocol.md`), wire-compatible in the Effect-RPC dialect
but with SwarmBot's own string prefixes, so a code or signature from another
product never validates here.

**What exists.** `crates/node-protocol` is implemented (6,339 lines of source;
71 lib tests + 2 WebSocket integration tests + 1 doctest = **74**), and so is the
daemon that uses it: `crates/node` (6,186 lines of source; **135 tests, none
ignored**), which dials out, pins the brain key, serves `fs.*` and streaming
`shell.exec` under its own policy, and refuses what it cannot do.

| Module | Owns | Doc section |
| --- | --- | --- |
| `identity.rs` | Ed25519 identities for both sides, 0600 storage, base64url keys | §2 |
| `pairing.rs` | Crockford-base32 codes, normalization, hashing, constant-time compare | §2.1 |
| `auth.rs` | the signed upgrade header, ordered verification, bounded replay cache, `node.hello` signatures, brain-origin derivation, close codes | §3 |
| `wire.rs` | the JSON-RPC 2.0 envelope: frames, type-preserving ids, tagged error causes, direction routing | §4, §7 |
| `mux.rs` | `Peer` over any `Socket`: streaming + Acks, interruption, ping/liveness, **call deadlines and withdrawal**, the `NotAuthenticated` gate; `WsSocket`, `connect_ws*`, `accept_ws*` | §3.4, §3.5, §4 |
| `streams.rs` | one-time data-stream tokens and `StreamBroker` bookkeeping | §8 |
| `rpc.rs` | method names, payload structs, the unimplemented-method refusal table | §5, §6, §9 |
| `testing.rs` | in-memory sockets for tests, public | §10 |

**The brain's half is built too.** `crates/brain/src/gateway/` (`mod`, `socket`,
`session`, `executor`, `wire`) plus `routes/node_gateway.rs` serve
`POST /node/pair` and the signed `GET /node` upgrade, keep the roster of connected
nodes, and route a bot's tool call to the right machine (`NodeDispatcher`). The
end-to-end proof is `crates/node/tests/real_brain.rs`: an in-process brain plus the
**real `swarmbot-node` binary** as a child process, pairing and then serving a bound
bot's `read_file` and `shell` calls over a real WebSocket (§11).

**What does not exist yet:** the data-stream half — framing, stream kinds and the
`/node/stream/:id` endpoint (M7, §8). Token minting and brokering are built; there
is simply nothing to carry yet, and `NodeCall` has no stream-bearing variant, so no
node can be asked for a pty, a tunnel or a file transfer. Everything else in the
reference subset is implemented.

One fossil worth knowing about, because it looks like a contradiction:
`routes/node_gateway.rs` still exports `NOT_YET` ("node pairing arrives in
milestone M6") and `CLOSE_CODE = 1011`. **No route returns them any more.** They
survive because `crates/node/tests/brain_end_to_end.rs` builds its *fake* brain's
refusal body from the brain's own `error_body(NOT_YET)`, so that test cannot drift
from the envelope shape a real brain sends; a brain-side test pins the two
constants until it is rewritten.

---

## 1. Overview

```
node ──(1) POST /node/pair {code, publicKey, …}──────────────────────▶ brain   once
node ──(2) WebSocket /node   Authorization: SwarmBot-Node v1 …───────▶ brain   control socket
         ◀── node.hello {brainSig} ──   (brain calls, node serves)
         ── node.requestApproval / node.event / node.heartbeat ──▶
                                                              (node calls, brain serves)
node ──(3) WebSocket /node/stream/:id  Authorization: Bearer <token>─▶ brain   (§8, tokens only)
```

* One **control socket** per node, always dialed **out** by the node
  (`mux::connect_ws` / `connect_ws_with_headers`), so NAT and firewalls are not a
  problem. It carries JSON-RPC 2.0 in both directions over one `mux::Peer` (§4).
* The brain's own machine is the **local node** (`nod_local`): the same
  `tools::LocalExecutor`, in process, no socket. A remote node is not special — it
  is the same executor behind this wire.
* Bulk data never rides the control socket; it gets its own socket under a
  one-time token. The token bookkeeping is built (§8); the framing and the
  endpoint are not.

## 2. Identity and pairing

Implemented: `identity.rs`, `pairing.rs`, and the brain's `POST /node/pair` route.

### 2.1 The pairing code

* `pairing::mint_code() -> Result<PairCode, PairError>` draws `CODE_LEN = 8`
  characters from `CODE_ALPHABET = 0123456789ABCDEFGHJKMNPQRSTVWXYZ` using the OS
  CSPRNG. The alphabet has exactly 32 entries and `256 % 32 == 0`, so taking the
  low 5 bits of each random byte is uniform — no rejection sampling, no modulo
  bias. A failed RNG is a typed error, never a weak fallback.
* `PairCode { display: "ABCD-EFGH", raw: "ABCDEFGH" }` — `display` is what the
  owner reads and types (`format_display`), `raw` is what is hashed.
* `normalize_code(input)` upper-cases, strips spaces and dashes, and maps
  `I`/`L` → `1`, `O` → `0`; `hash_code(normalized)` is
  `sha256("swarmbot-pairing-v1|" + code)`, hex — `PAIRING_HASH_PREFIX` keeps a code
  from another product from ever matching. Comparison is `constant_time_eq`.
* The brain stores only the hash, in a `contracts::PairingRecord`: single use,
  `is_live(now)` = `!used && now < expires_at`, with the 10-minute TTL of D1.3 set
  by whoever mints it: `brain::auth::PAIRING_CODE_TTL_SECS = 600`, so nothing in
  this crate decides the lifetime.

### 2.2 Keys

* `NodeIdentity::generate(...)` / `BrainIdentity::generate(origin)` make an
  Ed25519 keypair; `save(path)` writes JSON at mode **0600**, `load(path)` reads it
  back. The node's private key never leaves the node.
* Public keys travel as **base64url without padding** (`public_key_base64url`,
  `parse_public_key_base64url`), raw 32 bytes. A wrong length or an invalid point
  is a typed `IdentityError`, not a panic.
* **Deviation from the reference protocol:** OpenBot stores the node key as
  base64url **PKCS#8** (WebCrypto's export format). We store the raw 32-byte
  Ed25519 **secret seed**, which is what `ed25519-dalek` uses natively. The two are
  trivially convertible but the files are not byte-identical — a node written
  against the upstream doc must convert.
* `NodeIdentity::from_pair_response(...)` pins the brain key from the pairing
  answer; every later verification uses that pinned key.

### 2.3 The pairing exchange

`contracts::PairRequest` / `PairResponse` are the bodies (camelCase):

```json
{ "code": "ABCD-EFGH",
  "publicKey": "<base64url raw 32-byte key>",
  "hostname": "box", "platform": "linux",
  "arch": "x86_64", "nodeVersion": "0.1.0",
  "protocol": { "min": 1, "max": 1 } }
```

`arch`, `nodeVersion` and `protocol` are optional (`protocol` defaults to
`ProtocolRange::current()`; `negotiate` picks the highest common version, `None`
when the ranges do not overlap). Response:

```json
{ "nodeId": "nod_0192…",
  "brainPublicKey": "<base64url raw 32-byte key>",
  "brainOrigin": "http://127.0.0.1:8787" }
```

The node pins `brainPublicKey` and keeps `nodeId` and `brainOrigin`. Static bearer
tokens are not used: a stolen URL must not be enough to drive somebody's machine
(D1.3). `auth::brain_origin_from_url` derives the origin that gets signed
(`ws`→`http`, `wss`→`https`, `scheme://host[:port]`); a URL it cannot reduce to an
origin is `AuthError::BadOrigin`.

### 2.4 Redemption at the brain

`POST /node/pair` is **not** behind the owner's auth gate, and that is not an
oversight: a node has no credential until it has paired, and its credential
afterwards is a signature over a fresh nonce rather than a bearer token. What
protects the route is the code itself plus a fixed order:

1. normalize the code (§2.1) — `abcd efgh`, `ABCD-EFGH` and `abcd-efgh` are one code;
2. look the hash up; **one refusal message covers "never minted here", "expired"
   and "already used"**, because an answer that distinguishes them is an oracle for
   guessing codes. Which of the three it was goes to the log, since the owner is
   the only person who can act on the difference;
3. re-compare the stored hash in constant time — cheap, and it turns a store bug
   into a refusal instead of a pairing;
4. parse `publicKey` as 32 raw base64url bytes;
5. negotiate `protocol`; no overlap → a refusal naming both ranges;
6. **burn the code before creating anything** (`used = true`, `node_id` recorded).
   A crash halfway through must leave an unusable code behind, never a reusable
   one — and, symmetrically, a pairing that fails at steps 1–5 has *not* burned
   anything, so a node that mistyped its hostname can retry with the same code;
7. store the public key, mint the `NodeInfo` (the hostname becomes the label until
   the node's own event arrives with its `--label`), publish it to the roster.

## 3. Control socket: signed upgrade and `node.hello`

Implemented: all of `auth.rs`, the `require_hello` gate in `mux::PeerConfig`, and
the axum route that mounts them (`brain/src/routes/node_gateway.rs` +
`gateway/session.rs`).

### 3.1 Upgrade

The node opens `ws://<brain>/node` (`wss://` behind a proxy) with:

```
Authorization: SwarmBot-Node v1 id=<nodeId>,ts=<unix ms>,nonce=<base64url 16 bytes>,sig=<base64url sig>
```

`AUTH_SCHEME` is exactly `SwarmBot-Node v1`. `auth::upgrade_auth(node_id,
signing_key, brain_origin)` builds the header with a fresh nonce and the current
`unix_ms_now()`; `make_upgrade_auth(..., ts, nonce)` is the explicit-arguments form
used by tests. `sig` is Ed25519 over the UTF-8 string

```
swarmbot-node-v1|<nodeId>|<ts>|<nonce>|<brainOrigin>
```

— `NODE_SIGN_PREFIX`, fields separated by `|`, in that order, with the nonce in
its base64url text form. Every value in the header must match `[A-Za-z0-9_-]+`
(`ts` decimal digits), and the nonce must decode to exactly `NONCE_BYTES = 16`
bytes — checked before the replay cache sees it, so garbage cannot fill the cache.

`verify_upgrade_auth(header, brain_origin, lookup, replay, now)` checks, **in this
order**, each with its own error:

| Step | Check | Failure |
| --- | --- | --- |
| 1 | header shape: scheme, `id`/`ts`/`nonce`/`sig`, no trailing fields, nonce decodes to 16 bytes | `MalformedHeader{reason}` |
| 2 | `lookup(&node_id)` — a `&dyn Fn(&str) -> KeyLookup` | `UnknownNode` / `NodeRevoked` |
| 3 | `\|now − ts\| ≤ MAX_SKEW_MS` (60 000) | `TimestampSkew{ts, now}` |
| 4 | the nonce is not in `ReplayCache` | `ReplayedNonce` |
| 5 | Ed25519 `verify_strict` over the canonical message | `BadSignature` |

The lookup is **tri-state** (`KeyLookup::{Unknown, Revoked, Active(VerifyingKey)})`
rather than an `Option`, so "never paired" and "paired then revoked" produce
different errors — the difference matters to an operator, and `Revoked` is what
tells a node to stop redialing (§3.3). Verification uses `verify_strict`, which
rejects malleable and non-canonical encodings.

The nonce is **recorded only after the signature verifies** (step 5), so an
attacker cannot burn a legitimate node's nonces with garbage headers.
`ReplayCache` is bounded: `NONCE_WINDOW_MS = 120 000` (twice the maximum skew, so
an expired nonce can never become valid again) and `REPLAY_CACHE_CAPACITY = 4096`
— when full, expired entries go first, then the oldest. It uses a `std::sync::Mutex`
because verification is synchronous; never hold it across an await.

On success the brain gets `VerifiedUpgrade { node_id, ts, nonce }` — and it is that
`nonce` the hello signature covers. On any failure it answers **401 without
upgrading**, which is what `mux::accept_ws_hdr` exists for (§12). A request a node
sends later gets `x-swarmbot-node-id` **stamped by the brain**; anything the node
itself sends under `x-swarmbot-*` is removed, and brain handlers never read a node
id out of a payload.

### 3.2 `node.hello` (mutual authentication)

The first RPC on every connection is the **brain calling the node**. Params are
`rpc::HelloParams`:

```json
{ "jsonrpc": "2.0", "id": 0, "method": "node.hello",
  "params": { "protocol": { "min": 1, "max": 1 },
              "brainVersion": "0.1.0",
              "brainSig": "<base64url>" } }
```

`brainSig` is `auth::sign_hello(brain_key, node_id, nonce, brain_origin)` over

```
swarmbot-brain-v1|<nodeId>|<nonce from this connection's upgrade>|<brainOrigin>
```

(`BRAIN_SIGN_PREFIX`). The node checks it with `verify_hello` against the **pinned**
brain key. `PeerConfig::require_hello` is the gate: while it is set and no
`node.hello` handler has returned `Ok`, the peer answers *every* request with the
typed error `NotAuthenticated`; if verification fails the socket closes with
`CLOSE_HELLO_FAILED`. A DNS or proxy hijack therefore cannot drive a node. It is a
constructor flag, not a runtime guess — the brain side leaves it `false`.

The success value is `rpc::NodeHello`:

```json
{ "nodeId": "nod_…", "version": "0.1.0",
  "hostname": "box", "platform": "linux", "arch": "x86_64",
  "protocol": 1,
  "roots": [{ "id": "work", "path": "/home/u/swarmbot", "mode": "read_write" }],
  "capabilities": ["fs@1", "shell@1"],
  "tunnels": [],
  "policy": { "ceiling": "ask", "remoteManaged": false },
  "brainVerified": true }
```

* `protocol` is the negotiated version (`ProtocolRange::negotiate`); no overlap →
  close `CLOSE_NO_PROTOCOL` (4426).
* `roots[].mode` is `read_only` | `read_write` — `contracts::RootMode` keeps its
  own spelling, **not** the reference protocol's `rw`/`ro` (§4 rule 10).
* `capabilities` is `contracts::Capabilities`: a plain JSON array of versioned
  names, a bare name meaning `@1`. This build declares **`fs@1` and `shell@1`**
  (`NodeInfo::local`) and nothing else. The brain uses
  **effective = declared ∩ approved** (`Capabilities::effective`), so a newly
  declared capability stays off until the owner approves it; declaring fewer never
  needs approval.
* `policy` is `rpc::HelloPolicy { ceiling, remoteManaged }` — the reference
  protocol's shape, one overall `PolicyCeiling` (`never`/`ask`/`always`), **not**
  the per-surface `contracts::PolicyMap` the brain keeps internally. A node's own
  per-surface ceilings stay local; what it tells the brain is the single ceiling
  the brain may not exceed, and the brain still applies `min(brain, node)` per
  surface.
* `tunnels` is optional and empty here (no tunnelling in this build).

**The brain's hello has a deadline: `HELLO_DEADLINE = 10 s`**
(`brain/src/gateway/session.rs`). A node that dials, upgrades and keeps its socket
alive with pings while never answering `node.hello` would otherwise hold its
registry slot — and the brain's session task — forever, because the liveness
timeout below only notices a *silent* socket. On expiry the brain withdraws the
call (it is sent `@effect/rpc/Interrupt`, and a late answer is ignored), logs the
timeout and closes with `CLOSE_HELLO_FAILED` (4403), so the slot is released and a
node that redials gets a fresh handshake. Ten seconds is far more than a hello
needs — one signature check and one frame on the node's side.

### 3.3 Close codes

`auth.rs` constants, used by `Socket::close(code, reason)`:

| Code | Constant | Meaning |
| --- | --- | --- |
| `4401` | `CLOSE_REVOKED` | node revoked or unknown key. Do not redial until re-paired. |
| `4403` | `CLOSE_HELLO_FAILED` | `node.hello` failed |
| `4426` | `CLOSE_NO_PROTOCOL` | no common protocol version |
| `4000` | `CLOSE_REPLACED` | the brain asked the node to reconnect (a newer connection replaced this one) |

### 3.4 Liveness and redial

`PeerConfig` defaults are the reference values:

* `ping_interval = 5 s` — this side sends `@effect/rpc/Ping`; an inbound `Ping` is
  answered with `@effect/rpc/Pong` by the mux driver, never by a user handler.
* `liveness_timeout = 20 s` — the socket is closed (1001, "liveness timeout") after
  that long without **any** inbound frame. Any decoded frame counts as liveness, so
  a busy connection never pings itself to death. This is half-open detection: the
  brain marks the node offline, the node redials.
* **Liveness is not a call deadline.** It bounds a *silent socket*, and nothing
  else. A node that keeps pinging while a handler hangs — `fs.read` on a dead
  network mount is the canonical case — is perfectly "live" and would hold the
  caller forever. The bound on the *work* is separate and belongs to the caller;
  see §3.5.
* Application-level pings only: WebSocket-level ping/pong is left to tungstenite.
* The node redials with exponential backoff from 250 ms to 30 s, ±50 % jitter,
  resetting only after a connection stayed up 10 s (`crates/node/src/dial.rs`;
  `tests/redial.rs` pins the backoff and the permanent stop on `4401`).
* RPC streams do not survive a disconnect: in-flight calls fail, and the node
  interrupts in-flight handlers, killing their process groups (§5).
* **Dropping every `Peer` handle closes the connection.** The driver holds a
  `WeakUnboundedSender` for its command channel, so when the last handle goes the
  channel empties and the driver exits through `Exit::HandlesGone` (regression test
  `dropping_every_handle_closes_the_connection`; decision D9.5). It used to hold a
  strong clone, which made that branch dead code and leaked one task and one socket
  per disconnected node. `close(code, reason)` is still how you send a *close code*
  — drop is the leak-proof fallback, not the polite goodbye.

### 3.5 Call deadlines and withdrawal

A call's caller sets its own bound; the protocol does not. `mux::Peer` offers
`call_with_id_deadline` and `call_stream_with_id_deadline` next to the unbounded
`call_with_id` / `call_stream_with_id`, because the mux cannot know what a caller
is willing to wait for and must not guess.

**What the brain uses** (`brain/src/gateway/executor.rs`, `deadline_for`):

| Call | Deadline | Why |
| --- | --- | --- |
| `fs.*` | `FS_CALL_DEADLINE = 60 s` | A hung filesystem call is a dead mount or a stalled server: it never answers. Long enough for a `fs.grep` over a large tree on a slow disk, short enough that a bot's single turn survives one stuck call. |
| `shell.exec` | the caller's own `timeoutMs` + `SHELL_DEADLINE_GRACE = 30 s` | A command may legitimately run minutes, so the budget on the wire is the caller's, not a brain constant. The grace covers what the node still does after that timeout fires: kill the process group, drain the pipes, send the `Exit` chunk. Without it every timed-out command would be reported as a node failure instead of as the timed-out run it was. |
| `node.hello` | `HELLO_DEADLINE = 10 s` | §3.2. |

The deadline covers the **whole stream** for a streaming call, not the gap between
chunks: a stream that dribbles chunks forever is exactly the shape a deadline
exists to stop.

**On expiry the call is withdrawn**, not merely abandoned. One driver command does
both, in this order:

1. the pending entry for that id is removed — so a reply that arrives afterwards
   finds nothing, is logged and ignored, and cannot resurrect a call whose caller
   has gone (and the dropped waiter cannot panic the driver);
2. `@effect/rpc/Interrupt` is queued for that id (§4.7), which is the peer's cue to
   stop the work;
3. the caller gets `ProtocolError::CallTimedOut { method, after }` — deliberately
   **not** `LivenessTimeout` (the socket was alive) and **not** a `Remote` error
   (the peer said nothing). The brain turns that into a `ToolError::Failed` naming
   the node and the kind of call. A timeout is a failure, never a denial: nothing
   was refused, so `PolicyRefused` would be a lie about what happened.

Withdrawing also drops the stream's chunk sink, which is what lets a collector
finish instead of waiting for a stream that will never end.

**A deadline is not a cancellation guarantee.** It bounds how long *this side
waits*, and it *asks* the peer to stop. This build's node answers an interrupt by
dropping the handler future, which runs its `Drop` guards and kills the process
group (§4.7, §5.1) — but the brain cannot kill a process on another machine, and a
peer that ignores the frame may still be running the command when the brain has
given up. Its answer, if it ever comes, is discarded. Every error message and doc
comment says so rather than implying the work was stopped, and nothing reports a
success, an empty result or a finished job the brain cannot vouch for.

## 4. Wire format (JSON-RPC 2.0, Effect-RPC dialect)

Implemented in `wire.rs` (`Frame`, `Id`, `parse`, `render`, `direction`,
`CauseError`, `TypedError`) and driven by `mux::Peer`.

1. **One JSON-RPC message per WebSocket text frame.** A JSON array is a batch:
   batches are part of JSON-RPC but not of this dialect, so `parse` refuses them
   with the typed `ProtocolError::BatchUnsupported` rather than half-serving them.
2. **Direction.** `wire::direction(frame)` routes **only** a user-RPC `Request`
   frame to the `Server` half. Everything else — `Response`, `Error`, `Chunk`, and
   the control frames `Ack`, `Interrupt`, `Ping`, `Pong` — is `Client`.

   > This differs from the reference doc, which routes `Ack`/`Interrupt`/`Ping` by
   > "has a `method`" and therefore to the server side. Behaviour is identical:
   > the mux driver handles those three against **server-side state** before any
   > handler could see them — an `Ack` releases the `ChunkSink` that is waiting to
   > send the next chunk, an `Interrupt` cancels the running handler, a `Ping` is
   > answered with a `Pong` — and they never reach a user `RpcHandler`. Routing
   > them to the client half is what keeps that state in one place. A foreign peer
   > written against the reference rule still interoperates, because the frames on
   > the wire are the same.
3. **Request.** `{"jsonrpc":"2.0","id":2,"method":"shell.exec","params":{…},"headers":[["k","v"]]}`.
   `headers` is optional. `traceId`, `spanId` and `sampled` are **accepted and
   ignored**: there is no trace propagation yet, and a frame carrying them must not
   be rejected.
4. **Ids keep their JSON type.** `Id` is `Number(i64)` or `Text(String)` and
   round-trips as the same JSON type, because `@effect/rpc/Ack` must echo the
   request id exactly — a number answered as a string silently stalls a stream.
   The two directions have independent id spaces, allocated by the side that sends
   the request, so both may count `0,1,2,…`.
5. **Result.** `{"jsonrpc":"2.0","id":2,"result":<value>}`; `result: null` ends a
   stream or answers a void RPC.
6. **Typed error.**

```json
{ "jsonrpc": "2.0", "id": 1,
  "error": { "_tag": "Cause", "code": 0, "message": "…",
             "data": [{ "_tag": "Fail", "error": { "_tag": "PathDenied", "path": "/etc/x", "reason": "outside roots" } }] } }
```

   `data` is the flat list of `CauseReason`s: `Fail(TypedError)` renders
   `{"_tag":"Fail","error":{…}}`, `Die(String)` renders `{"_tag":"Die","message":…}`,
   `Interrupt` renders `{"_tag":"Interrupt"}`. An **unknown `_tag` decodes to
   `Die("unknown cause reason `<tag>`")`** instead of failing the frame, so a newer
   peer's reason degrades into a defect rather than killing the connection; a
   reason with no `_tag` at all is malformed. `code` is kept for wire fidelity (the
   reference implementation sends `0`).
7. **Streams.** Only `shell.exec` streams (`rpc::is_stream_method`). Each chunk is
   `{"jsonrpc":"2.0","chunk":true,"id":2,"result":[<value>, …]}`; the end is a
   normal result (`result: null`) or an error frame with the same id. **After every
   chunk the receiver must send**
   `{"jsonrpc":"2.0","method":"@effect/rpc/Ack","params":{"requestId":2}}`, and the
   sender waits for it before sending the next chunk — strict alternation, one
   chunk in flight, which is what keeps a slow brain from being flooded by a fast
   node. A chunk arriving for a request that is no longer served is
   `ProtocolError::StreamGone`.
8. **Interrupt.** `{"jsonrpc":"2.0","method":"@effect/rpc/Interrupt","params":{"requestId":2}}`
   cancels a call. The server answers with an error frame whose `data` holds an
   `Interrupt` reason, which surfaces from the original `call`/`call_stream` as
   `ProtocolError::Remote`; the node kills the work's process group before
   answering.

   The brain also sends an `Interrupt` **without waiting for that answer** when one
   of its own call deadlines expires: it withdraws the call (pending entry removed,
   so the late answer is ignored) and then asks the peer to stop (§3.5). That is
   the one interrupt whose answer nobody reads, and it is honest about it — the
   interrupt *asks*; the brain cannot kill a process on another machine, so the
   work may outlive the call.
9. **Payload-less RPCs** omit `params` or send `null`. `{}` is refused with the
   typed `ProtocolError::ExpectedNullParams` — the reference implementation fails
   these with "Expected null", and matching that keeps a foreign peer's error
   handling intact.
10. **Field spelling is deliberately mixed.** RPC **payload structs** in `rpc.rs`
    are `camelCase` on the wire, matching the reference protocol's §5.1 shapes
    (`expectedSha256`, `mtimeMs`, `brainVersion`, …). Types that come from
    `swarmbot-contracts` keep their contracts spelling instead of being renamed for
    the wire:

    * `Decision` is `allow_once` / `allow_always` / `deny` (**not** `allow-once`);
    * `RootMode` is `read_only` / `read_write` (**not** `rw` / `ro`);
    * `inv` is the full `contracts::Invocation`, which has no `rename_all`, so it
      crosses the wire in **snake_case**:
      `{"id","bot_id","conversation_id","turn_id","task_id","tool_call_id","node_id"}`
      — not the reference protocol's `{invocationId, botId, runId, toolCallId?}`.
      Our ids are typed and richer, and that struct is exactly what our node policy
      engine and audit log consume; a test pins `inv.bot_id` as the proof.

    A bridge to the upstream spelling must rename these explicitly, not guess.
11. **Unparseable frames are logged and ignored.** There is no id to answer under
    in this dialect, so killing a live connection over one bad frame would be
    worse: the driver `tracing::warn!`s and carries on.
12. **There are no id-less notifications.** `Peer::notify(method, params)` sends a
    normal request frame (with an allocated id) and discards whatever the peer
    answers; `Ok` means the frame was queued. A peer that expects a JSON-RPC
    notification (no `id`) will see a request it must answer.

## 5. RPCs served by the node

Method names are `rpc.rs` constants and the **params** are the `camelCase` structs
listed here. Payload structs are decoded with `rpc::decode_params`, whose failure
mode is deliberate: **malformed params become
`PolicyRefused { action: <method>, reason: "malformed params: …", retry: false }`**,
so a node never answers a shape problem with an untyped error and a model is told
retrying the same call cannot help.

The **results** are not the `rpc::Fs*Result` structs of the reference protocol.
A successful `fs.*` answer is the serialized `contracts::ToolResult` the
`tools::LocalExecutor` produced — `{"content":[{"type":"text","text":"…"}],
"isError":false,"meta":{…}}` — because those structs cannot be built from a
`ToolResult` without implementing every filesystem handler a second time, and two
implementations of `fs.read` is exactly the drift this design exists to avoid
(decision D9.7). The brain's remote `NodeExecutor` therefore deserializes straight
into the type its own port returns. Consequences, stated plainly:

* `fs.mkdir`, `fs.move` and `fs.remove` return a **`ToolResult`, not `null`**;
* `fs.stat` returns the tools text for a path that exists, and `null` **only** for a
  genuine NotFound;
* `shell.exec` still ends with `null` after its chunks;
* the `rpc::Fs*Result` / `FileStat` / `ListEntry` / `GrepMatch` structs remain in
  the crate as the wire vocabulary a foreign node may use, and as the shape the
  reference doc promises — but this build's node does not send them.

Every bot-originated, side-effecting call carries `inv: contracts::Invocation`
(§4 rule 10) for node policy, idempotency and the audit log.

| RPC | Params (`?` = optional) | Result | Notes |
| --- | --- | --- | --- |
| `node.hello` | `HelloParams {protocol, brainVersion, brainSig}` | `NodeHello` | §3.2; the only brain→node call that precedes authentication |
| `fs.read` | `FsRead {inv, path, offset?, limit?, encoding?}` | `ToolResult` | `encoding: "utf8"` only — a `base64` read is **refused**, because the local implementation reads text and saying so beats returning replacement characters |
| `fs.write` | `FsWrite {inv, path, content, encoding?, expectedSha256?, createParents?}` | `ToolResult` | atomic (temp + rename); `expectedSha256` **crosses and is enforced** (§5.1); `encoding: base64` refused |
| `fs.edit` | `FsEdit {inv, path, oldString, newString, replaceAll?, expectedSha256?}` | `ToolResult` | a non-unique match without `replaceAll` → `EditFailed` naming the count; **`expectedSha256` is refused**, not dropped (§5.1, D9.8) |
| `fs.stat` | `FsStat {inv, path}` | `ToolResult`, or `null` for a genuine NotFound | |
| `fs.list` | `FsList {inv, path, depth?, includeHidden?, limit?}` | `ToolResult` | `limit` is `u64` on the wire and saturates to `u32::MAX` |
| `fs.glob` | `FsGlob {inv, pattern, cwd, limit?}` | `ToolResult` | `cwd` is **required**; empty means the node's workspace |
| `fs.grep` | `FsGrep {inv, pattern, path?, glob?, ignoreCase?, literal?, contextLines?, outputMode?, limit?}` | `ToolResult` | `outputMode` is `content` \| `files_with_matches` \| `count`; empty `path` means the workspace |
| `fs.mkdir` | `FsMkdir {inv, path, recursive?}` | `ToolResult` | |
| `fs.move` | `FsMove {inv, from, to, overwrite?}` | `ToolResult` | `overwrite: false` is enforced by an existence pre-check → `FsError { code: "EEXIST" }`, "nothing was moved" |
| `fs.remove` | `FsRemove {inv, path, recursive?}` | `ToolResult` | |
| `shell.exec` **(stream)** | `ShellExec {inv, command, cwd, timeoutMs, env?, outputLimitBytes?}` | chunks of `ShellChunk`, then `null` | `cwd` and `timeoutMs` are **required**; see below |

### 5.1 Mapping `tools::NodeCall` onto these params

The wire payloads and `tools::NodeCall` are two shapes of the same intent and are
**not** field-for-field identical. `crates/node/src/handlers.rs::to_node_call` is
the one place that bridges them, and its rule is decision D9.8: **where the wire can
express something `NodeCall` cannot, the call is refused, never silently
downgraded** — dropping a compare-and-set would turn a safety check into a clobber.

| Wire | `NodeCall` | What the bridge does |
| --- | --- | --- |
| `FsWrite.expectedSha256` | `FsWrite.expected_sha256` | **maps straight through and is enforced**: a stale hash leaves the file untouched and answers `EditFailed`. Conditional writes *do* cross |
| `FsEdit.expectedSha256` | *(no such field)* | **refused**: `PolicyRefused { retry: false, reason: "a conditional edit (`expectedSha256`) cannot be expressed as a local `NodeCall` yet, so this node refuses it rather than dropping the check" }`. Both the node and the brain refuse; neither drops |
| `FsWrite.encoding: base64`, `FsRead.encoding: base64` | `content: String` | **refused** — a base64 body cannot cross a `String` field, and pretending otherwise would let a caller believe binary content was written or read |
| `FsGlob.cwd` (required) | `FsGlob.path` (optional) | an empty/whitespace `cwd` becomes `None`, which already means "the node's workspace" — the two line up without a guess |
| `FsGrep.path` (optional) | `FsGrep.path` (optional) | empty string → `None`, same rule |
| `FsMkdir.recursive` | `FsMkdir.parents` | the same flag under two names, not two behaviours |
| `FsMove.overwrite: false` | *(no such field)* | enforced by the caller as an existence pre-check that answers `FsError { code: "EEXIST" }` before anything moves; `true`/absent is what `rename` does anyway |
| `FsList.limit`, `FsGlob.limit`, `FsGrep.limit` (`u64`) | `u32` | `limit_u32` saturates (`u64::MAX` → `u32::MAX`), pinned by a test |
| every params struct | *(no `inv`)* | `inv` is lifted out and carried beside the call: the node needs it for its own policy, idempotency and audit line |
| a method with no local counterpart | — | `PolicyRefused { reason: "`<method>` carries no `NodeCall`" }` |

`ShellChunk` is `#[serde(tag = "_tag", rename_all_fields = "camelCase")]` — the
**tag values keep their PascalCase variant names** while the fields inside each
variant are camelCase, so the wire shape of a finished run is exactly:

```json
{"_tag":"Exit","code":0,"signal":null,"timedOut":false,"durationMs":12,"outputTruncated":false}
```

* `Started {pid, pgid}` — the child's pid and its process group, so the brain knows
  what an interrupt has to kill;
* `Output {stream, data, seq}` — `stream` is `stdout` | `stderr`, `data` is **UTF-8
  text** (not base64: a shell that emits invalid UTF-8 is lossily converted rather
  than stalling the stream), `seq` is a per-invocation counter so the brain can
  reorder chunks that arrived interleaved on one socket;
* `Exit {code, signal, timedOut, durationMs, outputTruncated?, spillPath?}` —
  **`code` is `Option<i32>` and is `null` when the process was killed by a
  signal**, in which case `signal` carries it.

Two invariants the node side keeps (`crates/node`, tested), because they are what
make the wire safe rather than merely functional:

* **paths are the caller's unresolved strings.** A remote node has its own
  filesystem and its own `tools::PathPolicy`, so resolution happens there, never on
  the sending side (`tools::NodeCall`'s doc comment states the rule);
* **on `Interrupt` or a socket drop, kill the process group.** `crates/node/src/shell.rs`
  runs children with `process_group(0)` **and** `kill_on_drop(true)`, and a
  `ProcGuard`'s `Drop` kills the group — which is what runs when the mux drops the
  future on an `Interrupt`. Every test in `tests/shell_stream.rs` asserts
  `process_group_gone(pgid)` after the call returned, because a shell test that
  leaves a process behind is the failure mode that matters.

## 6. RPCs served by the brain

| RPC | Params | Result |
| --- | --- | --- |
| `node.requestApproval` | `RequestApproval {invocationId, botId, action, target, reason}` | `ApprovalAnswer {decision, by}` |
| `node.event` | `NodeEvent {type, data}` | `null` |
| `node.heartbeat` | `Heartbeat {at, activeInvocations, load?, freeMemBytes?}` | `null` |

* `action` is the surface name (`shell`, `write_file`, …) and `target` the concrete
  command line or path — the same pair `tools::ApprovalAsk` carries, so the brain
  can match it against a `contracts::ApprovalRule` without translation.
  `reason` is a required string (empty when there is none).
* `decision` is `contracts::Decision`, so it is spelled **`allow_once` /
  `allow_always` / `deny`** (§4 rule 10) — not the reference protocol's
  `allow-once`. `by` names who decided (the owner, or a rule id).
* `at` is **Unix milliseconds, `i64`** (`auth::unix_ms_now` is the shared clock).
* `NodeEvent.type` is serialized as `"type"`; well-known types are `audit` (carrying
  a `tools::AuditEntry`), `node.roots-changed`, `browser.lease-changed`,
  `browser.closed`. Unknown types are **ignored, not rejected**.
* `node.requestApproval` is how a node obeys an `ask` ceiling without holding a
  human. The brain decides first (rules, cards, `approval_expiry_secs`); the node
  decides finally (`PathPolicy`, its own `Never` ceilings). A brain that says
  "allow" cannot make a node do what its own policy says it never does (D1.4).

## 7. Typed errors

`wire::TypedError`, carried inside a `Fail` cause reason. All twelve are defined so
a node can be honest about *why*, and so a model can be told whether retrying could
help:

| `_tag` | Fields | Raised by |
| --- | --- | --- |
| `PathDenied` | `path`, `reason` | roots/deny-list/read-only violations (`reason`: `outside roots`, `deny-list`, `read-only root`) |
| `PolicyRefused` | `action`, `reason`, `retry` | a `Never` ceiling, an unimplemented method (§9), malformed params (§5). `retry: false` = tell the model not to retry |
| `EditFailed` | `path`, `reason` | `fs.write` conditional-write mismatch, ambiguous `fs.edit` |
| `FsError` | `op`, `path`, `code`, `reason` | errno-style `code`: `ENOENT`, `EISDIR`, … |
| `TunnelFailed` | `kind`, `reason` | not raised in this build (no tunnelling) |
| `StreamRefused` | `kind`, `reason` | `StreamBroker::take` refusals (§8) |
| `NotAuthenticated` | `reason` | any request before `node.hello` succeeded |
| `BrowserUnavailable` | `reason` | defined for wire compatibility; nothing raises it here |
| `BrowserActionFailed` | `action`, `reason` | idem |
| `LeaseHeld` | `botId`, `holder`, `reason` | idem |
| `DesktopUnavailable` | `message`, `missing?` | idem |
| `NodeUnavailable` | `nodeId`, `reason` | brain-side only: the node is offline or gone |

The last four exist so a foreign peer's errors decode instead of failing the frame;
this build never produces them (D0.4: no browser, desktop, screen or lease).

There is **no `ShellFailed` tag**, and adding one would break wire compatibility.
A `shell.exec` that cannot even spawn is therefore reported as a non-retryable
`PolicyRefused` carrying the real reason (`crates/node/src/handlers.rs::shell_failure`)
— typed, and honest in its own text about not being a policy decision. Filesystem
failures use `FsError` with an errno-style `code` translated from the OS
(`ENOENT`, `EEXIST`, `EACCES`, `EPERM`, `EISDIR`, `ENOTDIR`, `ENOTEMPTY`, `EXDEV`,
`ELOOP`, `ENAMETOOLONG`, `ENOSPC`, `EROFS`, `EBUSY`, `EMFILE`/`ENFILE`, else `EIO`).

## 8. Data streams `/node/stream/:id`

**Token bookkeeping is implemented** (`streams.rs`); framing, stream kinds and the
endpoint are **not** (M7). Nothing in this build pretends otherwise:
`tools::NodeCall` has no stream-bearing variant, so a node cannot be asked for a
pty, a tunnel or a file transfer, and `node.openStream` is in the refused table
(§9).

* `mint_stream() -> Result<StreamGrant, StreamError>` draws from the OS CSPRNG:
  `stream_id` is 96 bits as **24 hex characters** (it appears in the URL path),
  `token` is 128 bits as **22 base64url characters** (the secret), and `expires_at`
  is `STREAM_TOKEN_TTL = 30 s` out. A failed RNG is a typed error, never a weak
  fallback.
* `StreamBroker::register(&grant, kind)` records the grant **with its kind**;
  `take(stream_id, token) -> Result<String /* kind */, TypedError>` consumes it.
  Registering prunes expired entries first.
* `take` is **single use** (the entry is removed on success) and answers
  `StreamRefused` with a distinct reason for `unknown stream id`, `stream token
  expired` and `wrong stream token`. Tokens are compared with `constant_time_eq`,
  and a **wrong token deliberately does not remove the entry** — a guess must not
  burn the legitimate token.
* `forget(stream_id)` drops a grant without consuming it, for the RPC that promised
  the stream but failed, so nobody dials a dead id. `len()` prunes then counts.
* The endpoint shape (M7): the node dials `ws://<brain>/node/stream/<streamId>`
  with `Authorization: Bearer <streamToken>`; the brain splices frames verbatim and
  each kind defines its own framing (`pty`, `transfer` with ≤ 1 MiB chunks, a
  `{"done":true,"sha256":…,"size":N}` terminator and `{"ack":N}` at least every
  1 MiB, so neither side buffers a whole file).

## 9. Refusals: what this build answers for what it does not do

An unimplemented capability is a typed refusal, never a fake success and never a
silent drop (D0.4):

| Request | Answer |
| --- | --- |
| a method the reference protocol defines but this build does not implement | `rpc::unimplemented(method)` → `PolicyRefused { action: <method>, reason: "not implemented by this node", retry: false }` |
| a method nobody defines | the dispatcher's own `PolicyRefused { reason: "unknown method" }` — a different message on purpose, so "this team cannot do that" is distinguishable from "that is not a thing" |
| a capability the node did not declare, or the owner did not approve | the tool is not offered to the model at all (`ToolRegistry::visible_specs`), and a call that arrives anyway is `NodeUnavailable` on the brain side |
| a path outside the roots, or inside the deny list | `PathDenied` with a reason that reads on its own |

The refused set is `rpc::UNIMPLEMENTED_METHODS` — `fs.download`, `fs.upload`,
`pty.open`, `tunnel.open`, `service.start`, `service.stop`, `forward.listen`,
`browser.ensure`, `browser.action`, `browser.screencast`, `browser.input`,
`lease.set`, `desktop.ensure`, `desktop.stop`, `computer.act`, `computer.stream`,
`screen.ensure`, `screen.stream`, `screen.input`, `node.openStream` — and the
implemented set is `rpc::IMPLEMENTED_METHODS` (§5, §6). A test
(`every_documented_method_is_implemented_or_explicitly_refused`) pins every name in
the reference doc's tables as **implemented XOR refused**, and asserts neither list
contains a name the doc does not — so the tables cannot drift silently in either
direction.

## 10. Writing a node in another language

1. Generate an Ed25519 key; `POST /node/pair` with the code; pin `brainPublicKey`,
   keep `nodeId` and `brainOrigin` at mode 0600. Store the **raw seed**, or convert
   from PKCS#8 (§2.2).
2. Dial `/node` with the signed `Authorization` header (a fresh 16-byte nonce per
   dial); keep the nonce — `node.hello` signs over it.
3. Run a JSON-RPC 2.0 endpoint over the socket following §4: route frames by
   `direction`, Ack every chunk you receive, answer `Ping` with `Pong`, return
   `null` for void results, and keep an id's JSON type identical in the Ack.
4. Refuse everything with `NotAuthenticated` until `node.hello` arrives with a
   valid `brainSig`; answer it with `NodeHello`.
5. Implement the RPCs for the capabilities you declare. Enforce your own roots,
   deny list and ceilings; call `node.requestApproval` for `ask`; audit every
   invocation.
6. On `@effect/rpc/Interrupt` or a socket drop, kill the work's process group. A
   brain call deadline arrives this way (§3.5): the brain has already stopped
   waiting and will not read your answer, so an interrupt is the *only* chance to
   stop the work — treat it as urgent, not as advisory.
7. Redial with backoff; stop on close code `4401`.

**Testing without sockets.** `node_protocol::testing` is public test support, not
`#[cfg(test)]`-only, precisely so `brain` and `node` can test the wire in-process:

* `LineSocket` — newline-delimited JSON frames over any byte duplex
  (`tokio::io::duplex`, a TCP loopback pair, …). Rendered frames never contain raw
  newlines (`serde_json` escapes them), so line splitting is lossless;
* `duplex_pair()` — two `LineSocket`s wired back to back, for mux tests;
* `ScriptSocket` — a canned inbound script plus a `SentLog` of everything the peer
  sent, for wire-level assertions with no second peer (this is how the
  chunk/Ack strict alternation and the `Pong` answer are tested);
* `mux::Socket` itself is only three methods (`send_text`, `recv_text`, `close`
  with a default body), so a foreign transport is a small impl, not a fork.

## 11. What is tested today

Three layers, and the point of the third is that the first two cannot fake each
other.

**The wire crate** — `node-protocol`, 71 lib tests + 2 in `tests/ws.rs` + 1 doctest
= **74**: the header round trip and every verification failure mode (bad shape,
unknown node, revoked node, skew, replayed nonce, bad signature, and that a failed
signature does *not* burn the nonce), hello sign/verify, pairing codes
(normalization, hashing, constant-time compare, display vs raw), identity
save/load/0600 and key-encoding errors, frame parse/render including ids that keep
their JSON type, batches, `{}` params, trace fields, unknown cause `_tag`s, the mux
(chunk/Ack strict alternation, interrupt, ping/liveness, the `require_hello` gate,
and that dropping every handle closes the connection), **call deadlines and
withdrawal** (§3.5: a call that is never answered fails at its deadline and is
interrupted, a call answered in time still succeeds, a late answer neither panics
nor breaks the connection, and a withdrawn stream's late chunk never reaches its
collector), stream-token single-use/expiry/wrong-token behaviour, and the
method-table XOR coverage.

**The daemon** — `crates/node`, 71 lib + 64 integration = **135, none ignored**:

| File | Tests | What it pins |
| --- | --- | --- |
| `hello_gate.rs` | 9 | nothing runs before `brainSig` verifies against the key pinned at pairing (D1.3) |
| `ceilings.rs` | 10 | the node decides finally: a `never` ceiling is never an ask, and an unreachable or refusing brain means **deny** (D1.4) |
| `fs_calls.rs` | 18 | the payload → `NodeCall` bridge, this node's own path policy, symlink escape, and the typed error each denial becomes |
| `shell_stream.rs` | 8 | real chunks, a real pid and pgid, `Started → Output… → Exit`, a timeout that kills the group and says so, and `process_group_gone` after every call |
| `pairing.rs` | 7 | `POST /node/pair` against a hand-written fake brain on `127.0.0.1:0`, and what lands on disk afterwards |
| `redaction.rs` | 3 | the private key exists in exactly one place (`identity.json`, 0600) and one private field |
| `redial.rs` | 3 | backoff on a refused connection, permanent stop on close `4401` |
| `refusals.rs` | 1 | an unimplemented capability is a typed `PolicyRefused { retry: false }`, never a fake success (§9) |
| `websocket.rs` | 1 | one run over a **real WebSocket**: a hand-written brain on `127.0.0.1:0` and `dial::serve`, nothing mocked below the socket |
| `brain_end_to_end.rs` | 1 | a pairing **refusal** — any non-2xx, from any brain — is reported verbatim and nothing is written to disk; the expected envelope is built by the brain's own `error_body`, so neither half can drift |
| `real_brain.rs` | 3 | **the two real halves together** — see below |

**The gateway** — `crates/brain`, 134 lib + 57 integration = **191**, of which
`tests/node_gateway.rs` (**8**) drives the whole remote path — pairing, signed
upgrade, `node.hello`, `fs.*`, streamed `shell.exec`, revoke → `4401` — against the
real protocol library on a real loopback socket, with a *fake* node. Its lib tests
pin the two brain-side deadlines over an in-memory pair (§3.5, §3.2): a hanging node
fails a tool call with a `ToolError::Failed` naming the node and the method — never
a `PolicyRefused`, because nothing was refused — while the turn's `exec` returns;
and a node that never answers `node.hello` releases its slot instead of holding it.

### 11.1 What `real_brain.rs` adds

Each half above was proven separately, and each has a hole the other fills: a fake
node cannot show that the real daemon behaves, and a separate brain *process* cannot
be handed a scripted model turn, so the two-process run
(`target/two-process/run.sh`: pair → hello → heartbeats → revoke → 4401 → the node
stops redialing) never made a remote tool call. That script lives under `target/`:
it is a hand-run artifact, not a cargo target, so `cargo clean` deletes it — D9.13's
replacement comment in `crates/node/tests` cites it as the place the old recipe's
two-process step is covered. `real_brain.rs` combines the halves — the
brain composed **in process** (`server::serve` on a self-bound `127.0.0.1:0` with
`ServeOptions::in_process()`, which is what makes its scripted `Fake` backend
reachable) and the node as the **real binary** cargo built, via
`CARGO_BIN_EXE_swarmbot-node` (no test shells out to `cargo`: cargo holds the
target-dir lock while tests run, so doing that would deadlock). Three tests,
~0.4 s, always run:

1. **pairing and a real remote call.** The child pairs (exit 0, `identity.json`
   0600, node id on stdout), appears online with `approved == ["fs@1","shell@1"]`,
   and a bot bound to it issues `read_file` and `shell` calls that cross a real
   WebSocket. Which machine ran them is not taken on trust: `marker.txt` in the
   node's `--root` says `from the node` and a **decoy** in the brain's workspace
   says `from the brain`, so a silent local fallback (the D9.11 bug) hands back the
   wrong string and fails. The shell result is exactly
   `exit=0\n--- stdout ---\nfrom the node`, i.e. the `Started`/`Output`/`Exit`
   chunk stream reassembled, and the node's own audit JSONL gained both invocations
   with `decision:"run"`, `outcome:"ok"` and the right `bot_id`.
2. **the node's ceiling beats the brain's.** With `--ceiling shell=never` on the
   node and the brain's own `shell` ceiling at `always`, the same scripted call is
   refused by the node in its own words ("this node never allows `shell` actions",
   "retrying cannot help"), reaches the model as `ERROR: …`, is audited as
   `decision:"refuse"`, executes nothing on either machine — and a `read_file` in
   the same turn still succeeds on the same socket (D1.4).
3. **revocation ends it.** `DELETE /api/nodes/{id}` → 204, roster
   `revoked:true, online:false, approved:[]`, the child exits **by itself** with
   code 1 naming 4401, and a redial is refused with 401 *before* any upgrade.

The old `#[ignore]`d `pair_and_serve_against_a_real_brain` — an empty body with a
six-step recipe in a comment — was **deleted**, because an empty test is a lie that
reads as coverage (D9.13). A comment now names where each of its six steps is
actually tested, and what none of them covers (§13).

## 12. Mounting it: axum, and the tungstenite version split

`mux::WsSocket<S>` is generic over anything that sinks and streams tungstenite
`Message`s — **as long as the tungstenite versions agree**, and in this workspace
they do not:

| Crate | tokio-tungstenite |
| --- | --- |
| `swarmbot-node-protocol` (workspace pin) | `0.30` |
| `axum 0.8.9`'s `ws` feature | `0.29` |

So `WsSocket<axum's WebSocket>` cannot compile. The brain's answer is
**`gateway/socket.rs`: `GatewaySocket` implements the mux's three-method `Socket`
trait directly over axum's `WebSocket`** (`send_text`, `recv_text`, `close`), which
keeps `/node` inside the axum router — one listener, one shutdown path, and the
envelope, trace and body-limit middleware still cover it.

The rejected alternative was `mux::accept_ws_hdr` on a raw `TcpStream`. It works,
and it is still the right tool for a *non*-axum host, but it means answering the
HTTP upgrade by hand and therefore leaving the router.

Two related facts a future reader will need:

* **401 without upgrading** is not a trick: axum's `WebSocketUpgrade` extractor
  only parses headers, and the `101` is built later by `on_upgrade()` — which a
  refused request never reaches. `verify_upgrade_auth` therefore runs in the handler
  and returns a plain `401` body, so a node can report "the brain refused me"
  instead of "the socket died".
* **The node side has its own adapter for one reason.** `node_protocol::WsSocket`
  answers `Ok(None)` for *any* `Close` frame, which discards the peer's close code —
  and `4401` is only observable *from* that code. `crates/node/src/dial.rs`
  therefore wraps the socket in `NodeSocket`, which records the code before
  returning `Ok(None)`. Everything else about it is the same adapter. If
  `WsSocket` ever grows a close-code channel, `NodeSocket` can go away.

## 13. Known debt and unverified paths

Recorded here because a future reader will look for them in this file, not because
they are acceptable forever.

**Debt.**

1. ~~`crates/node/src/shell.rs` mirrors `exec_shell`.~~ **Closed.** The streaming
   implementation is now `tools::handlers::shell::exec_shell_streaming`, public,
   with `ShellEvent`/`ShellEventSink`/`ShellOptions`/`ShellRun` re-exported from
   `swarmbot_tools`. `crates/node/src/shell.rs` is a thin adapter: it contributes
   the node's own policy, spill dir and audit line, and maps `ShellEvent` onto the
   wire's `ShellChunk` — the spawn/pump/`killpg`/spill half is one body of code
   again. One deliberate difference remains, and it is stated rather than hidden:
   `exec_shell` (buffered, one exit → `ToolResult`) and `exec_shell_streaming`
   (feeds an emitter, can be cut short by its transport) keep separate run loops,
   side by side in that one file, both spawning through `build_shell_command` —
   so process group, scrubbed environment and the clamped group timeout cannot
   drift across the crate boundary. Tests: `the_streaming_run_sends_started_before_any_output`,
   `a_cancelled_streaming_run_kills_the_process_group`,
   `output_beyond_the_cap_is_spilled_to_a_file_the_result_names` (tools) and the
   pre-existing `crates/node/src/shell.rs` suite.
2. ~~The node's spill dir is not inside a root.~~ **Closed.** `NodeConfig::default()`
   and `NodeConfig::new_in()` now have **two** roots: `<data>/workspace` (read-write,
   still first) and `<data>/spill` (**read-only**). Output the model is told about
   can be read back with `fs.read`; the spill directory is not writable, so it
   cannot be used as free storage. An operator who passes `--root` still gets those
   roots appended, and an explicit `[[roots]]` in `node.toml` replaces the defaults
   as before. Tests: `the_default_roots_are_the_workspace_then_a_read_only_spill_directory`,
   `a_spilled_file_can_be_read_back_but_the_spill_directory_is_not_writable`.
3. **A refused *redial* loses the brain's reason.** When a revoked node dials again,
   the brain answers `401` before upgrading, and tungstenite surfaces that as
   `HTTP error: 401 Unauthorized` — the JSON body ("node … is revoked") is never
   read, so `SessionEnd::Failed { reason }` and the operator's log say "unreachable"
   where the truth is "revoked". The *live-socket* case is correct: a `4401` close
   produces `StopReason::Revoked` with its re-pair message, and the child in
   `real_brain.rs` exits 1 naming 4401. Only the dial-refused path is blunt. Fixing
   it means reading the response body out of tungstenite's `Error::Http` in
   `dial::connect` — and updating the assertion in `real_brain.rs` that currently
   pins the blunt wording (`line.contains("cannot reach the brain")`).
4. ~~`NodeExecutor::exec` does not carry the `Invocation`.~~ **Closed (D9.12).**
   The port is `exec(&self, node: &NodeId, invocation: &Invocation, call: NodeCall)`,
   so attribution is a fact from the caller rather than a guess: `ToolContext`
   already held the invocation, and it is what a node audits against and echoes in
   `node.requestApproval`. `NodeDispatcher::attribute()` — the `running_turns()`
   scan that fell back to the `bot_unknown`/`cnv_unknown` sentinel whenever two
   turns ran on one node — is deleted. `unattributed()` survives only for the
   `node.requestApproval` path in `gateway/session.rs`, where a node names a bot
   this brain cannot place; the dispatch path cannot reach it.
   Test: `two_turns_on_one_node_each_carry_their_own_invocation_to_the_node`
   (mutation-checked against the old give-up path) and
   `forward_remote_hands_the_callers_invocation_to_the_executor`.
5. **A call deadline stops the wait, not the work** (§3.5). On expiry the brain
   withdraws the call and sends `@effect/rpc/Interrupt`, and that is the end of
   what it can guarantee: it cannot kill a process on another machine, so a node
   that ignores the frame (or is wedged below its event loop) may still be running
   the command. Two deliberate consequences: the socket is **not** closed on a
   per-call timeout — one hung call must not fail the other calls in flight on the
   same connection — and a late answer is discarded rather than reported. Closing
   the socket would be a bigger hammer that turns one stuck call into an outage;
   the interrupt is the honest middle. The limit is stated in the tool error the
   model sees, not only here, because that is where somebody will read it first.
6. **The embedded CLI cannot reach a remote node, and that is a property of the
   mode, not a missing wire.** `LocalExecutor` knows which node it is and
   **refuses** a call addressed to any other one instead of absorbing it —
   absorbing meant a bot bound to a remote node had its tools run on the brain's
   machine, inside the brain's roots and under the brain's ceilings, invisibly (the
   tool-call entry still named the remote node). `swarmbot-brain` puts its
   `NodeDispatcher` in front, so remote calls really travel. `core::compose` does
   not, and a dispatcher there would have nothing to dispatch *to*: a node dials
   the brain's gateway (`GET /node`), and the embedded CLI has no listener and no
   gateway — no node can pair with it, so none can be connected. A bot whose config
   names a foreign node therefore gets a clear typed refusal. Wiring an executor
   into `core::compose` without also giving the CLI a gateway would produce a
   dispatcher that can only ever answer "not connected"; the honest fix, if remote
   nodes from the terminal are wanted, is to run the brain (`--brain`) — which is
   the documented way.

**Unverified — no test, and no local way to run one.**

| Path | Why it is unverified |
| --- | --- |
| TLS: `wss://` control sockets and `https://` origins | the URL rewriting (`http`→`ws`, `https`→`wss`) and rustls are wired and unit-tested as *strings*, but there is no TLS brain here to dial |
| the CLI's terminal token prompt | needs a real tty; tests pass `stdin_is_tty = false`, which is the branch that must not prompt |
| a remote tool call from a bot turn in **two separate OS processes, driven by a real model** | `real_brain.rs` composes the brain *in process* so its scripted `Fake` backend is reachable, and the two-process run (`target/two-process/run.sh`) has no way to be handed a scripted turn. So the two halves are each proven and their union is proven with one process shared; a fully separated pair with a live model has not been run |
| SIGINT, and the second-signal force exit (`registry.kill_all()` then `std::process::exit(0)`) | signal delivery to a supervised child is not something a unit test can fake honestly |
| `LiveEvent::Resync` caused by a **lagged** broadcast | the stale-cursor and wrong-epoch resyncs are tested (`brain/tests/live.rs`, `brain/tests/client_contract.rs`); the mid-stream `Lagged` branch of `LiveHub::subscribe` has no deterministic trigger |
| Windows | `crates/node` calls `libc::gethostname` and `killpg` unconditionally, so the daemon is unix-only in practice whatever `Cargo.toml` says |
| the per-call deadline against a **real hung handler on the real daemon** | the brain-side tests drive an in-memory peer whose far end deliberately never answers, and `crates/node`'s own tests prove an interrupt kills a process group — but nothing joins the two, because hanging a real handler deterministically (a `fs.read` on a dead mount) needs a mount the test cannot create. What *is* proven end to end: `real_brain.rs` runs a real daemon through `node.hello` and real calls, so the deadline path is not exercised there only because nothing hangs |
