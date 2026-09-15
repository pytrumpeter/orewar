# Orewar

A real-time multiplayer game on a 3D grid field. Each player commands a **tank**
and a **harvester** from one corner of the map. The harvester mines ore, ore buys
upgrades, and you win by capturing every other player's harvester.

Rust, Bevy 0.19 for the client, and a hand-rolled reliability layer over UDP.

---

## Running it

One machine is the server and manager of the match; everyone else connects to it.

```bash
# On the host machine
cargo run --release -p orewar-server

# On each player's machine
cargo run --release -p orewar-client -- --server 192.168.1.50 --name Ash
```

To try it on a single machine, run the server and then two clients with
different names (the name decides your identity, so each gets its own slot):

```bash
cargo run --release -p orewar-server
cargo run --release -p orewar-client -- --name Ash
cargo run --release -p orewar-client -- --name Bo
```

The match begins when the second player connects. Up to four can play.

### Options

| Server | |
|---|---|
| `--bind ADDR` | interface to listen on (default `0.0.0.0`) |
| `--port N` | UDP port (default `45701`) |
| `--seed N` | world seed; omit for a random map |

| Client | |
|---|---|
| `--server HOST[:PORT]` | who to join (default `127.0.0.1:45701`) |
| `--name NAME` | display name, and your identity |
| `--token N` / `--token-file PATH` | identity, set explicitly |

Host names resolve to IPv4 by preference. `localhost` resolves to `::1` before
`127.0.0.1` on Windows, and the server binds `0.0.0.0` -- IPv4 only -- so
following the resolver's order sends every packet to an address nothing is
listening on. UDP reports no failure for that, so it presents as a client that
renders the empty field forever. To serve IPv6 clients, start the server with
`--bind ::` and give clients that address explicitly.

Your **identity** is what the server uses to recognise you. If you drop out and
come back, you resume the same player: same ore, same upgrades, same vehicles
where you left them.

A name belongs to one player for the length of a match. Because `--name` is what
the identity is derived from, two clients under one name would share a slot and
spend the match kicking each other off it, so the second one is turned away at
the handshake with an error rather than let in. Start it with another name.

---

## Controls

| | |
|---|---|
| `W` `A` `S` `D` | drive |
| mouse | aim the turret |
| left mouse | fire the gun |
| right mouse | fire a missile |
| `Tab` | switch between tank and harvester |
| `B` | build menu, then `1`–`7` to buy |
| `Ctrl-Q` | leave the match |

---

## How it plays

**Harvesting.** Park the harvester on an ore deposit and it draws ore
automatically — it has to be nearly stopped. Drive home to your pad to unload it
into credits. Deposits visibly shrink as they are worked.

Because harvesting is proximity-based rather than something you actively do, you
can leave the harvester working on a deposit, press `Tab`, and go fight in your
tank. That trade-off — ore now, or position now — is the core of the game.

**Combat.** Both vehicles are heavily shielded. Shields absorb damage first and
regenerate after four quiet seconds; hull damage is permanent. A destroyed tank
respawns at your base after eight seconds.

**Capturing.** A harvester is never destroyed. At zero hull it is *disabled* —
dead in the water and takeable. An enemy tank that holds station over it for four
seconds captures it, and its owner goes off the field. But **your own tank can
rescue it**: park over your disabled harvester and it repairs, coming back online
at a quarter hull. A contested wreck cannot be taken.

**Winning.** Last player on the field wins — take everybody else's harvester and
the match is yours. In a two-player game that is one capture.

Losing a harvester is otherwise a setback rather than the end: if somebody else
is still playing, you sit out a minute and come back with fresh vehicles, an
empty bank, and every upgrade you had bought. So a bigger match has a second way
home, for when everyone keeps coming back — three captures wins it outright.

**Gunnery.** A tank shell carries about a tenth of the field and then falls
short. A gunfight is therefore fought at a range where both hulls are already
committed — you cannot stand off and trade — and walking into somebody's corner
means fighting their emplacement on its own terms, since a plain shell and that
gun cover about the same ground. **Long Barrel** doubles your reach, which is
what buys the ability to shell an emplacement from outside its own.

The emplacement and the harvester's auto turret are unaffected by any of this:
their shells are sized from their own engagement ranges rather than from the
tank's, so shortening the tank's gun again cannot leave either one firing at
something it can no longer hit.

**Upgrades.** Radar (a HUD contact circle), Shield Booster, Turbo Drive,
Harvester Armor, Auto Turret (the harvester defends itself), Long Barrel, and
Missile Packs. The map is generated in one quadrant and rotated into the other
three, so every corner faces an identical distribution of ore.

---

## How it is built

```
crates/
  orewar-shared/   wire protocol, UDP reliability, world generation, vehicle physics
  orewar-server/   authoritative simulation, 30 Hz
  orewar-client/   Bevy renderer, prediction, HUD
```

`orewar-shared` has **no dependencies**. The server links only against it, so it
builds and runs headless in seconds without pulling in a renderer.

### The server decides everything

Clients send intent and draw what comes back. Every outcome — who was hit, who
owns which harvester, whether a purchase is affordable — is decided by the
server. Player state is keyed by identity token rather than socket address,
which is what makes disconnects recoverable.

### Reliability over UDP

`shared/src/net.rs` implements exactly the two delivery guarantees a game needs:

- **Unreliable** — one payload per packet, never retransmitted. Snapshots and
  inputs. A lost snapshot is obsolete 33 ms later, so resending it would only
  add latency to the packet that replaces it.
- **Reliable-ordered** — a numbered stream, redelivered until acknowledged and
  handed over strictly in order. Handshake, purchases, captures.

Every packet carries its sequence number, the highest sequence seen from the
peer, and a 32-bit history of the ones before it, so a single packet
acknowledges up to 33 and acks survive heavy loss without dedicated traffic.
Reliable messages ride along in outgoing packets until an ack proves they landed.
There is no fragmentation: a reliable message must fit one packet, enforced when
it is queued.

The unreliable payload is written to a packet *first*, so a backlog of reliable
messages can never starve the snapshot stream that keeps the game moving.

Snapshots are budgeted to stay inside one 1200-byte datagram: angles are
quantized to 16 bits, health to 16, capture progress to 8; maximums are never
sent because the client derives them from the power-up mask; the world travels as
a *seed* rather than a deposit list; and projectiles are ranked by distance from
the viewer and cut to a fixed count. A test asserts the worst case fits, so
adding a field fails the build rather than fragmenting packets in a live match.

### Smooth remote players, responsive local ones

Two different jobs, deliberately kept apart:

- Other players and projectiles are **interpolated** — rendered 100 ms in the
  past, sliding between the two snapshots that bracket that moment.
- The vehicle you are driving is **predicted** — the client runs the same
  `sim::step_vehicle` the server does, in `FixedUpdate` at the same 30 Hz with
  the same `dt`, so prediction and authority agree by construction rather than
  by luck. Each snapshot re-anchors on the authoritative state and replays every
  input the server has not yet acknowledged. Residual error is held as a separate
  offset that decays away, so corrections read as a slight drift instead of a
  jump; anything large enough to be a respawn is taken instantly.

### Tests

```bash
cargo test
```

The ones worth knowing about:

- `reliable_stream_survives_a_lossy_reordering_link` — 500 messages across a
  link dropping 30% and reordering, asserting exact-once, in-order delivery.
- `an_empty_ack_does_not_retire_an_unreceived_message` — a peer that has heard
  nothing must not be able to retire our reliable messages. This was a real bug:
  an empty ack field reads as "packet 0 acknowledged", which retires a message
  that never arrived and blocks the ordered stream permanently.
- `reliable_delivery_survives_sequence_wraparound` — 70,000 packets, past the
  point where 16-bit sequence numbers wrap.
- `replayed_inputs_reproduce_the_server_state` — prediction lands exactly where
  it started after a correction and replay.
- `a_glancing_bullet_cannot_tunnel_through_a_tank` — projectiles are swept
  segments, not points; a bullet crosses 3 units per tick and a glancing shot
  traverses a chord under 2, so a point test would score a clean miss on a
  visible hit.
- `a_worst_case_snapshot_fits_one_packet` — the MTU budget.
- `rendered_heading_matches_simulation_heading` and `sim_perp_is_screen_right` —
  the sim-to-world mapping and which way is right. Sim yaw turns `+X` toward
  `+Y`, but a positive rotation about Bevy's `+Y` carries `+X` toward `−Z`; the
  conventions run opposite ways, so the angle is negated and increasing yaw
  turns the vehicle to its *right*. Steering and the radar both depend on it.
- `input_expires_when_a_client_goes_quiet` — input arrives unreliably and a
  client that lags out simply stops sending. Without an expiry the server keeps
  applying the last frame, so a dropped player drives on at full throttle
  forever.

### Toolchain

Bevy 0.19 needs rustc 1.95+. `rust-toolchain.toml` pins 1.98.1 for this project
rather than changing the machine's default.

### One hardware workaround

The camera is spawned with `NoIndirectDrawing` (`client/src/camera.rs`).

Bevy 0.19 builds draw calls on the GPU and issues them indirectly. The Qualcomm
Adreno X1 this was developed on advertises full support for that and then issues
no draws at all: the HUD rendered perfectly over an empty sky, with no error
reported anywhere. `NoIndirectDrawing` switches to direct draw calls and the
scene appears. It must be present when the camera is first spawned.

The cost is losing GPU culling, which this scene is far too small to need. If
you are on hardware where indirect drawing works, removing that one line is
safe.

Two things that look like fixes for this but are not, in case you go hunting:
disabling GPU light clustering, and `PbrPlugin { use_gpu_instance_buffer_builder:
false }`. The latter actively breaks the opaque pass -- it floods the log with
`Dynamic uniform batch sets should be used when GPU preprocessing is off` and
drops every opaque draw while transparent surfaces keep rendering.

---

## Where to take it next

Seams that were left deliberately clean:

- **Captured harvesters currently leave the field.** The capture path is a single
  function (`Game::apply_capture`); transferring the vehicle to the captor and
  giving it simple harvesting AI would go there.
- **Only the driven vehicle is predicted.** Enough for LAN. Prediction is keyed
  by slot in `state::Prediction` if both are ever wanted.
- **Buildings.** The economy supports it — `PowerUp` is a flat list, and adding a
  variant is a cost, a name, and an effect.
