# Orewar

A real-time multiplayer game on a 3D grid field. Each player commands a **tank**
and a **miner** from one corner of the map. The miner mines ore, ore buys
upgrades, and you win by capturing every other player's miner.

Rust, Bevy 0.19 for the client, and a hand-rolled reliability layer over UDP.

---

## Running it

One machine is the server and manager of the match; everyone else connects to it.

```bash
# On the host machine
cargo run --release -p orewar-server

# On each player's machine
cargo run --release -p orewar-client
```

Started with nothing to go on, the client opens on a **connect screen**: type
the host's address and the name you want to play under, press Enter, and watch
the handshake. Whatever the server says about it lands there -- a name somebody
is already using, a server that is not answering -- with the fields still in
front of you to correct. What worked last time is filled in for you the next
time you start it.

Everything on that screen can also be given on the command line, which fills it
in and presses the button, so a scripted launch still goes straight to the
field:

```bash
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

Any one of these skips the connect screen. With none of them the client asks.

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
| `W` `A` `S` `D` | drive; in the air, `W`/`S` are the yoke and `A`/`D` bank |
| `1`–`5` | in the air, the throttle: `1` slowest, `3` cruise, `5` full |
| mouse | aim the turret |
| left mouse *or* `Space` | fire the gun |
| right mouse *or* `F` | fire a missile; in the air, the cannon |
| `Tab` | switch between the vehicles you have |
| `G` | call up a bombing run and take its controls; once you own the **Bomber** |
| `B` | build menu, then `1`–`8` to buy |
| `Alt-C` | cheat mode on or off, for everybody in the match |
| `O` | overview camera; right-drag to pan, `+`/`-` to zoom |
| `Ctrl-Q` | leave the match |

Everything keeps working with the overview up — you can fly, drive, shoot, buy
and call up a sortie while looking at the whole field. The mouse is the one
thing that changes hands: up there it drags the map and reaches the miner's
mode buttons, so the two triggers come off it and `Space` and `F` do the firing.
Aim still follows the cursor, which from that height means aiming at a place on
the map rather than at a point just ahead of the hull.

Flying is different from driving. `A` and `D` do not steer the bomber, they
**bank** it, and a banked aircraft comes round on its own — the further over it
is, the tighter the turn. Let go and the bank stays where you put it, so a turn
continues until you roll back level. `W` and `S` trim the airspeed either side
of the cruise — never near a stop, since held hard back it still outruns a tank
under Turbo — and because the bombs are thrown ahead by the airspeed, a slower
run drops them some twenty-two units shorter. Left mouse releases bombs.

---

## How it plays

**Mining.** Park the miner on an ore deposit and it draws ore
automatically — it has to be nearly stopped. Drive home to your pad to unload it
into credits. Deposits visibly shrink as they are worked.

Because mining is proximity-based rather than something you actively do, you
can leave the miner working on a deposit, press `Tab`, and go fight in your
tank. That trade-off — ore now, or position now — is the core of the game.

**Combat.** Both vehicles are heavily shielded. Shields absorb damage first and
regenerate after four quiet seconds; hull damage is permanent. A destroyed tank
respawns at your base after eight seconds.

**Capturing.** A miner is never destroyed. At zero hull it is *disabled* —
dead in the water and takeable. An enemy tank that holds station over it for four
seconds captures it, and its owner goes off the field. But **your own tank can
rescue it**: park over your disabled miner and it repairs, coming back online
at a quarter hull. A contested wreck cannot be taken.

**Winning.** Last player on the field wins — take everybody else's miner and
the match is yours. In a two-player game that is one capture.

Losing a miner is otherwise a setback rather than the end: if somebody else
is still playing, you sit out a minute and come back with fresh vehicles, an
empty bank, and every upgrade you had bought. So a bigger match has a second way
home, for when everyone keeps coming back — three captures wins it outright.

**Gunnery.** A tank shell carries about a tenth of the field and then falls
short, so a gunfight is fought at a range where both hulls are already
committed — you cannot stand off and trade. **Long Barrel** doubles your reach
to a fifth of the field.

**The emplacement**, though, reaches a *third* of the field. Nothing a tank can
bring outranges it, so somebody's corner is ground you cross under fire and take
by wearing the gun down, not by standing off and shelling it. What the range
does not buy it is accuracy: it aims where a target is, and its shell takes
nearly two seconds to cross that distance, so far out it punishes what is
parked, approaching, or leaving rather than what is crossing. There is no ring
drawn on the ground — you learn where it reaches by being shot at.

Missiles are the way to answer one without standing in front of it: they seek,
they will lock a standing emplacement, and they fly far enough that the tank
launching them can sit outside its reach.

Both it and the miner's auto turret size their shells from their own
engagement ranges rather than from the tank's, so retuning the tank's gun
cannot leave either one firing at something it can no longer hit.

**The bomber** is the one thing in the game that reaches ground a tank cannot.
It costs 1000 ore — more than everything else on the list put together — and
what it buys is the aircraft, not a sortie. Press `G` and it comes in over your
own corner with sixty seconds of fuel — three crossings of the field, or one
crossing and a fight at the far end — and you are flying it the moment it
arrives. `G` again is the way back to it if you have switched away, `Tab` reaches
it like anything else, and when the fuel goes it is gone and the next one is
forty-five seconds away.

**Flying it** is not driving it. `A` and `D` do not steer — they bank, and a
banked aircraft comes round on its own at a rate set by how far over it is. Let
go and the bank stays where you put it, so a turn has to be rolled out of as
well as into. The throttle is not a key you hold either: `1` to `5` are detents,
`3` being the plain cruise, and the aircraft holds whichever you select.

`W` and `S` are the yoke, and they work like a control column rather than a
scrolled screen: push forward to go down, pull back to come up. The aircraft
flies anywhere between ten units off the ground — low enough to be among the
hills, though still clear of them — and a hundred, and it holds whatever height
you leave it at. Height is not just a view. It changes where the bombs go: the
sight ring sits almost under the aircraft down on the floor and out past twice
that from the ceiling, because a bomb dropped from higher up falls for longer and
is thrown further ahead. Watching that ring slide out as you climb is the whole
trade in one picture.

Nothing on the ground can shoot it down. Every gun down there fires along the
ground, and a shell that took an aeroplane down would be one you watched pass
underneath it. What can reach you is another aircraft. What a bomb costs you instead is the run itself: it leaves with
the aircraft's velocity and falls, landing well ahead of where you let it go, so
a run is a line you commit to and fly through — and one you have to bank into
early, because rolling level again takes as long as rolling over did. You cannot
fly off the map: near a wall the aircraft rolls itself back toward the middle of
the field, so the fuel is the only thing that ends a sortie. The band is narrow
enough to leave a run at a corner base alone. The ring on the ground is where the
next bomb will land. Hills are cover from everything else in
the game and nothing at all to a bomb, and a blast fades from its centre out, so
a near miss still costs the target something.

A hit is worth the wait. Square on, a bomb takes a miner's shield entirely
and half of the hull under it, and a tank that has not bought a Shield Booster
does not survive one at all. That is deliberate: a bomb is released a long way
before it lands, from an aircraft committed to a line and unable to stop,
out of a sortie that comes round every forty-five seconds at best — against
anything moving and paying attention, most of them miss.

**Dogfighting.** Two aircraft in the air are the only things in the game that
can reach each other, and the cannon on the right mouse button is the only
weapon that works up there. It has no ammunition and no turret: it fires where
the nose is pointed, at the height you are flying, so getting a hit is entirely
a question of flying — which is what makes the bank worth understanding and the
yoke worth using.

Height is the whole of it. A round flies level at the altitude it left at, so
climbing out of somebody's guns genuinely takes you out of reach, and coming
back down to theirs is the price of shooting at them. The same is true of the
aircraft themselves: two at different heights pass straight through each other,
and two at the same height that meet take half of everything off both of them.
Nobody wins that exchange — it does not matter who flew into whom — so ramming
is a way to trade, and two clean meetings take both aircraft down. What it costs
comes back with your next sortie, because a sortie is a fresh aircraft rather
than a repaired one.

Seven cannon hits will take an aircraft down from full, which at four and a half
rounds a second is about a second and a half of somebody holding a bead on you.
Its panel appears at the bottom right whenever a sortie is up — including while
you are back in the tank, because an aircraft can be shot down whether or not
you are watching it.

Ore in a blast is not reduced, it is **gone**. A deposit is the one thing on the
field that cannot be driven out of the way, and one bomb empties it — which is
the reason to spend a sortie on open ground rather than on somebody's hull.

**Upgrades.** Radar (a HUD contact circle), Shield Booster, Turbo Drive,
Miner Armor, Auto Turret (the miner defends itself), Long Barrel,
Missile Packs, and the Bomber. The map is generated in one quadrant and rotated
into the other three, so every corner faces an identical distribution of ore.

**Cheat mode** is `Alt-C`. Everything on the build list becomes free and a
sortie never runs out of fuel, so the bomber can be flown without mining a
match's worth of ore for one first. Any player can toggle it and it lands on
everybody — a cheat that applied only to whoever pressed the key would be an
advantage rather than a way to look at something quickly. Flying off the map
still ends a sortie; that is the one rule it does not lift.

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
owns which miner, whether a purchase is affordable — is decided by the
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

- **Captured miners currently leave the field.** The capture path is a single
  function (`Game::apply_capture`); transferring the vehicle to the captor and
  giving it simple mining AI would go there.
- **Only the driven vehicle is predicted.** Enough for LAN. Prediction is keyed
  by slot in `state::Prediction` if both are ever wanted.
- **Buildings.** The economy supports it — `PowerUp` is a flat list, and adding a
  variant is a cost, a name, and an effect.
