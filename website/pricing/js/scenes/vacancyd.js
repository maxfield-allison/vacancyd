// Vacancyd: "the motel sign for your hardware", taken literally.
//
// A roadside sign at night carries a rate dial. Three needles, each with a
// motel key fob (Spare Room, Suite, Penthouse), climb clockwise from the $0
// stop at nine o'clock until they peg at ten billion a night and the sign
// flickers to NO VACANCY. Rivet, busy with a brass hex nut from his
// collection, ignores it until the needles slam the peg and make him jump.
// A double take, a look at whoever is watching, and he drops onto the bunched
// fobs. They dip and spring back; he stomps; they give, and his weight rides
// them down to the stop. He hangs the nut on them as a counterweight, hops
// off, and glares them back down when they twitch. VACANCY comes back on.
//
// World units: the needle pivot is the origin, the gauge face has radius 220.

import { Rivet, DEFAULT_POSE } from "../rivet.js";
import { TAU, clamp, lerp, ease, track, rng, rgba, smoothstep, noise1, springResponse } from "../ink.js";
import { breathAt, blinkSchedule, idleProgramme, applyDeltas, applyDoze, crestFollower } from "../behavior.js";
import { loadArt, layerMatrix, coverMatrix, drawPlate, rivetKit } from "../paint.js";

const STORY = 7.0;
const FREE_AT = 4.58;
const FACE_R = 220;
const BEZEL_R = 238;
const NEEDLE_L = 182;
const PEG = (84 * Math.PI) / 180;
const RIVET_SCALE = 1.05;
const FAR_PARALLAX = 0.45; // used when the fit file gives a plate no depth
const PERCH_K = 0.97; // where along the needle Rivet's feet sit while riding
// The sign follows the painted cabinet (art/vacancyd/sign-v1.png), measured at
// 0.6728 units per pixel with the dial's centre on the needle pivot. The
// procedural fallback draws the same shapes.
const BOX = { x0: -190, x1: 190, y0: 44, y1: 104 }; // the VACANCY glass, inside its brass frame
const BOX_RIM_Y = 34; // top of the brass frame: the ledge Rivet lands on
const BOARD = { x0: -279, x1: 277, y0: -492, y1: 153 };
const GROUND_Y = 300;
const BACKDROP = { x0: -780, x1: 780, y0: -940, y1: 580 };
// The distant road where the painted courtyard opens onto the valley, in the
// far plate's world units (its town lights sit at about 860 px of 1254).
const PAINTED_ROAD_Y = 245;

const TIERS = [
  { name: "Spare Room", start: 9, fob: "#e9a23b", tag: "SPARE", len: 182, offset: -0.012 },
  { name: "Suite", start: 29, fob: "#c4543c", tag: "SUITE", len: 176, offset: 0 },
  { name: "Penthouse", start: 99, fob: "#2e6a5a", tag: "PENT", len: 170, offset: 0.012 },
];

const priceAt = (phi) => Math.max(0, Math.pow(10, (10 * clamp(phi, 0, PEG)) / PEG) - 1);
const phiFor = (price) => (PEG * Math.log10(price + 1)) / 10;
const needlePoint = (phi, len) => ({ x: -Math.cos(phi) * len, y: -Math.sin(phi) * len });

// Rivet watches from the top of the cabinet, as jackdaws do from signs, and
// lands at the end on the brass frame of the VACANCY glass.
const START = { x: 150, y: BOARD.y0, slope: 0 };
const LANDING = { x: -94, y: BOX_RIM_Y };
// The neon wordmark fills the plain enamel between the cabinet top and the dial.
const WORDMARK_Y = -400;
const TAGLINE_Y = -338;
const FOB_IDS = ["vacancyd-fob-amber", "vacancyd-fob-rust", "vacancyd-fob-teal"];
const FOB_HEIGHT = 60; // world units, ring to point
const NUT_SIZE = 24; // world units across the flats
const NUT_STRING = 30; // world units of wire from the fob ring to the nut

// The story's beats in seconds; motion, camera, sign and props all key off
// these, so retiming a beat moves everything that belongs to it.
const BEAT = {
  climb: 0.6, // the needles start to creep
  glance: 1.02, // Rivet glances down, then back to his nut
  clang: 1.9, // the needles slam the peg pin and he jumps
  take: 2.3, // double take at the dial
  look: 2.72, // a look at whoever is watching, in a close shot
  ready: 3.1, // he decides
  drop: 3.28, // steps off the cabinet
  land: 3.62, // lands on the bunched fobs; they dip and spring back
  stomp: [3.88, 4.1], // two stomps; the second one gives
  stop: FREE_AT, // the needles hit the $0 stop
  backUp: FREE_AT + 0.1, // he hops back up the needle to make room to bow
  hang: 5.0, // he bows out over the tip and hooks the nut on as a counterweight
  hop: 5.22, // he hops off to the VACANCY frame
  perch: 5.64,
  tug: 5.8, // the needles twitch, the nut holds, he glares
  proud: 6.3,
};

// The sign's geometry in world units, for the paint guides
// (tools/lab/guides.js): what a painted sign must leave room for.
export const LAYOUT = {
  BOARD,
  BOX,
  FACE_R,
  BEZEL_R,
  GROUND_Y,
  START,
  LANDING,
  POSTS: [-173, 173],
  WORDMARK: { x0: -250, x1: 260, y0: -460, y1: -320 },
  READOUTS: { x0: 18, x1: 200, y0: -168, y1: 0 },
};

// ---------------------------------------------------------------- motion

// A damped dip: the bunch gives under an impact and its spring brings it back.
function dip(t, at, depth) {
  if (t < at) return 0;
  const s = t - at;
  return depth * Math.sin(Math.min(1, s / 0.06) * Math.PI / 2) * Math.exp(-Math.max(0, s - 0.06) / 0.08);
}

// The shared angle once Rivet is on the bunch: stubborn, then gravity.
function rideAngle(t) {
  if (t < BEAT.land) return PEG;
  const give = BEAT.stomp[1];
  if (t < give) return PEG - dip(t, BEAT.land, 0.1) - dip(t, BEAT.stomp[0], 0.08);
  if (t < BEAT.stop) {
    const k = (t - give) / (BEAT.stop - give);
    return PEG * (1 - ease.in(k)) - 0.06 * smoothstep(give, give + 0.08, t) * (1 - k);
  }
  const s = t - BEAT.stop;
  const bounce = Math.abs(Math.sin(s * 22)) * Math.exp(-s * 7) * 0.085;
  // His hop off and the spring's late tug lift the needles; the nut holds.
  const hopOff = Math.sin(clamp((t - BEAT.hop) / 0.3) * Math.PI) * 0.04;
  const tug = Math.sin(clamp((t - BEAT.tug) / 0.4) * Math.PI) * 0.1;
  return bounce + hopOff + tug;
}

function needleAngle(i, t) {
  const tier = TIERS[i];
  const start = phiFor(tier.start);
  // Penthouse pegs first so the three rates stay in tier order as they climb.
  const pegAt = BEAT.clang - (TIERS.length - 1 - i) * 0.07;
  if (t < BEAT.climb) return start + tier.offset;
  if (t < pegAt) {
    const k = (t - BEAT.climb) / (pegAt - BEAT.climb);
    // A hesitant first notch, then runaway.
    const notch = Math.sin(clamp(k / 0.18) * Math.PI) * 0.03;
    return lerp(start, PEG, ease.in(k)) + notch + tier.offset;
  }
  if (t < BEAT.land) {
    const s = t - pegAt;
    const slam = Math.abs(Math.sin(s * 26)) * Math.exp(-s * 10) * 0.07;
    const strain = Math.sin(t * 90 + i * 2) * 0.004;
    return PEG - slam + strain + tier.offset;
  }
  return rideAngle(t) + tier.offset * (t < BEAT.stop ? 1 : 0.4);
}

// Where along the needle he stands: out at the fobs while he rides, then a
// hop back up the shaft so the tip is a bow's length in front of him.
const perchK = (t) => lerp(PERCH_K, 0.64, smoothstep(BEAT.backUp, BEAT.backUp + 0.2, t));
const ridePoint = (t) => needlePoint(rideAngle(t), NEEDLE_L * perchK(t));
const fobRing = (t) => needlePoint(needleAngle(0, t), TIERS[0].len);

// Stepping off a ledge to somewhere lower: a small lift as the wings open,
// then a fall that gathers speed, unlike a hop's symmetric arc.
function drop(t, t0, t1, a, b, lift) {
  const k = clamp((t - t0) / (t1 - t0));
  return {
    x: lerp(a.x, b.x, ease.sine(k)),
    y: a.y + (b.y - a.y) * k * k - lift * 4 * k * (1 - k),
  };
}

// Hops are parabolas between two points with a given apex height.
function hop(t, t0, t1, a, b, lift) {
  const k = clamp((t - t0) / (t1 - t0));
  const top = Math.min(a.y, b.y) - lift;
  return {
    x: lerp(a.x, b.x, ease.sine(k)),
    y: lerp(a.y, b.y, k) - (4 * (lerp(a.y, b.y, 0.5) - top)) * k * (1 - k),
  };
}

// In-place jumps: the startle at the clang and the two stomps.
const JUMPS = [[BEAT.clang + 0.02, 0.3, 24], [BEAT.stomp[0] - 0.18, 0.18, 12], [BEAT.stomp[1] - 0.18, 0.18, 14], [BEAT.backUp, 0.2, 12]];
function jumpAt(t) {
  for (const [at, length, height] of JUMPS) {
    if (t > at && t < at + length) return Math.sin(((t - at) / length) * Math.PI) * height;
  }
  return 0;
}

const B = BEAT;
const tl = {
  // Legacy rig channels: headAngle + tips the bill down, squash + is impact.
  headAngle: track([[0, 0.3], [0.22, 0.3], [0.3, 0.78, "out"], [0.4, 0.3, "out"], [0.48, 0.3], [0.56, 0.8, "out"], [0.68, 0.32, "out"],
    [B.glance, 0.32], [B.glance + 0.05, 0.72, "snap"], [B.glance + 0.24, 0.72], [B.glance + 0.29, 0.18, "snap"], [1.45, 0.05], [1.62, 0.24], [1.82, 0.05],
    [B.clang, 0.05], [B.clang + 0.05, -0.3, "snap"], [B.take, -0.2], [B.take + 0.05, 0.9, "snap"], [B.look, 0.9], [B.look + 0.06, -0.06, "snap"], [B.ready, -0.06], [B.ready + 0.06, 0.62, "snap"],
    [B.drop, 0.4], [B.land, 0.55], [B.land + 0.1, 0.98, "out"], [B.stomp[0] - 0.2, 0.98], [B.stomp[0], 0.7], [B.stomp[1], 0.6],
    [B.stop - 0.2, -0.05], [B.stop, -0.1], [B.stop + 0.06, 0.4, "snap"], [B.hang - 0.16, 0.5], [B.hang - 0.04, 1.05], [B.hang + 0.12, 1.05], [B.hop, 0.3],
    [B.perch, 0.05], [B.tug + 0.04, 0.05], [B.tug + 0.1, 0.4, "snap"], [B.tug + 0.42, 0.4], [B.tug + 0.5, 0, "snap"], [B.proud, -0.12], [STORY, -0.04]]),
  headTurn: track([[0, 0], [B.tug, 0], [B.tug + 0.06, 1, "snap"], [B.tug + 0.42, 1], [B.tug + 0.5, 0, "snap"]]),
  neck: track([[0, 1], [B.glance, 1], [B.glance + 0.06, 1.12, "out"], [B.glance + 0.29, 1], [B.take, 1.02], [B.take + 0.08, 1.3, "out"], [B.look, 1.26], [B.look + 0.06, 1],
    [B.ready, 1], [B.ready + 0.08, 0.84, "out"], [B.drop, 0.92], [B.land, 1], [B.stop, 0.96], [B.hang - 0.12, 1.28], [B.hang + 0.14, 1.28], [B.hop, 1], [B.proud, 1.06], [STORY, 1]]),
  headY: track([[0, 0], [B.hang - 0.16, 0], [B.hang - 0.04, 10], [B.hang + 0.12, 10], [B.hop - 0.05, 0]]),
  brow: track([[0, 0], [B.glance, 0], [B.glance + 0.05, 0.8, "snap"], [B.glance + 0.3, 0], [B.ready, 0], [B.ready + 0.06, -1, "snap"], [B.land + 0.3, -1], [B.stop, 0],
    [B.tug + 0.06, 0], [B.tug + 0.1, -1, "snap"], [B.tug + 0.45, -1], [B.tug + 0.55, 0]]),
  lid: track([[0, 0], [B.look + 0.06, 0], [B.look + 0.12, 0.5, "out"], [B.look + 0.16, 0.5], [B.look + 0.2, 1, "inOut"], [B.look + 0.32, 0.5, "inOut"], [B.ready - 0.06, 0.5], [B.ready, 0, "snap"], [B.tug + 0.08, 0], [B.tug + 0.14, 0.36, "out"], [B.tug + 0.45, 0.36], [B.tug + 0.52, 0]]),
  eyeWide: track([[0, 0], [B.clang, 0], [B.clang + 0.05, 1, "snap"], [B.take - 0.02, 0.45], [B.take + 0.05, 1, "snap"], [B.look, 1], [B.look + 0.06, 0, "snap"],
    [B.ready, 0], [B.ready + 0.06, 0.28], [B.land, 0.28], [B.stop, 0.2], [B.stop + 0.06, 0.75, "snap"], [B.stop + 0.4, 0], [B.proud, 0]]),
  pupil: track([[0, 0], [B.glance, 0], [B.glance + 0.06, -0.55, "out"], [B.glance + 0.3, 0], [B.clang, 0], [B.clang + 0.05, -1, "snap"], [B.look, -1], [B.look + 0.06, -0.45, "snap"],
    [B.ready, -0.45], [B.ready + 0.08, -0.85, "out"], [B.stomp[1], -0.6], [B.stop, -0.3], [B.stop + 0.06, 0.2], [B.hang - 0.1, -0.6], [B.hop, 0], [B.tug + 0.1, -1, "snap"], [B.tug + 0.5, -0.2], [B.proud, 0.35], [STORY, 0.2]]),
  crestRaise: track([[0, 0], [B.clang, 0], [B.clang + 0.05, 1.1, "snap"], [B.take - 0.02, 0.5], [B.take + 0.05, 1.1, "snap"], [B.look, 0.9], [B.look + 0.06, -0.5, "snap"], [B.ready - 0.04, -0.5],
    [B.ready + 0.04, 0.2], [B.stomp[1], 0.3], [B.stomp[1] + 0.1, 0.9], [B.stop, 0.9], [B.stop + 0.06, 1.1, "snap"], [B.stop + 0.5, 0.1], [B.tug + 0.1, -0.7, "snap"], [B.tug + 0.5, -0.4], [B.proud, 0.55], [STORY, 0.2]]),
  beakOpen: track([[0, 0.12], [B.hang + 0.04, 0.12], [B.hang + 0.1, 0.45, "out"], [B.hang + 0.22, 0], [B.proud + 0.2, 0], [B.proud + 0.26, 0.6, "out"], [B.proud + 0.36, 0], [B.proud + 0.48, 0.55, "out"], [B.proud + 0.58, 0]]),
  bodyPitch: track([[0, 0.12], [0.3, 0.2, "out"], [0.4, 0.12], [0.56, 0.2, "out"], [0.68, 0.12], [B.clang, 0.12], [B.clang + 0.05, -0.12, "snap"], [B.take, 0], [B.take + 0.08, 0.3],
    [B.look, 0.28], [B.ready, 0.12], [B.ready + 0.1, 0.32], [B.drop, -0.08], [B.land, 0.05], [B.land + 0.12, 0.28], [B.stomp[1], 0.2], [B.stop - 0.1, -0.1], [B.stop + 0.1, 0.18],
    [B.hang - 0.16, 0.5], [B.hang - 0.04, 1.1], [B.hang + 0.12, 1.1], [B.hop, 0.1], [B.perch, 0.05], [B.tug + 0.1, 0.18, "snap"], [B.tug + 0.5, 0.05], [B.proud, -0.06], [STORY, 0]]),
  crouch: track([[0, 0], [B.clang - 0.04, 0], [B.clang + 0.01, 0.35, "out"], [B.clang + 0.05, -0.15], [B.clang + 0.32, 0.45, "out"], [B.take + 0.2, 0],
    [B.ready, 0], [B.ready + 0.14, 0.78, "out"], [B.drop - 0.02, 0.8], [B.drop + 0.04, -0.2, "out"], [B.drop + 0.16, 0], [B.land, 0], [B.land + 0.05, 0.6, "out"], [B.land + 0.3, 0.2],
    [JUMPS[1][0], 0.55, "out"], [JUMPS[1][0] + 0.03, -0.1], [B.stomp[0], 0.65, "out"], [JUMPS[2][0] + 0.02, 0.55], [JUMPS[2][0] + 0.05, -0.1], [B.stomp[1], 0.7, "out"], [B.stomp[1] + 0.2, 0.35],
    [B.stop, 0.35], [B.stop + 0.05, 0.6, "out"], [B.backUp, 0.2], [B.backUp + 0.02, -0.1], [B.backUp + 0.2, 0.5, "out"], [B.hang - 0.16, 0.3], [B.hang - 0.04, 0.9], [B.hang + 0.12, 0.9], [B.hop - 0.12, 0.3], [B.hop - 0.02, 0.62, "out"], [B.hop + 0.05, -0.1], [B.hop + 0.15, 0],
    [B.perch, 0], [B.perch + 0.05, 0.62, "out"], [B.perch + 0.3, 0], [B.proud, 0], [B.proud + 0.15, 0.15], [STORY, 0]]),
  squash: track([[0, 0], [B.clang + 0.02, 0], [B.clang + 0.06, -0.7, "out"], [B.clang + 0.3, 0], [B.clang + 0.34, 0.6, "out"], [B.take + 0.1, 0],
    [B.drop, 0], [B.drop + 0.05, -0.6, "out"], [B.drop + 0.2, 0], [B.land, 0], [B.land + 0.04, 0.9, "out"], [B.land + 0.22, 0],
    [B.stomp[0], 0], [B.stomp[0] + 0.04, 0.7, "out"], [B.stomp[0] + 0.16, 0], [B.stomp[1], 0], [B.stomp[1] + 0.04, 0.8, "out"], [B.stomp[1] + 0.2, 0],
    [B.stop, 0], [B.stop + 0.04, 0.6, "out"], [B.stop + 0.2, 0], [B.perch, 0], [B.perch + 0.04, 0.8, "out"], [B.perch + 0.2, 0]]),
  lookX: track([[0, 0.2], [B.glance, 0.2], [B.glance + 0.05, 0.4, "snap"], [B.glance + 0.29, 0.1, "snap"], [B.look, 0.3], [B.look + 0.06, -0.35, "snap"], [B.ready, 0], [B.ready + 0.06, 0.3, "snap"],
    [B.stop, 0.5], [B.tug, 0.4], [B.tug + 0.06, 0.6, "snap"], [B.tug + 0.5, 0.2], [B.proud, 0], [STORY, 0]]),
  lookY: track([[0, 0.5], [B.glance, 0.5], [B.glance + 0.05, 0.75, "snap"], [B.glance + 0.29, 0.4, "snap"], [B.clang + 0.05, -0.3, "snap"], [B.take, -0.2], [B.take + 0.05, 0.8, "snap"],
    [B.look, 0.8], [B.look + 0.06, 0, "snap"], [B.ready + 0.06, 0.6, "snap"], [B.land + 0.1, 0.9], [B.stomp[1], 0.6], [B.stop, 0], [B.hang - 0.1, 0.8], [B.hop, 0.2], [B.tug + 0.06, 0.5, "snap"], [B.tug + 0.5, 0.2], [B.proud, -0.1]]),
  tailAngle: track([[0, 0], [B.clang, 0], [B.clang + 0.05, -0.3, "out"], [B.clang + 0.35, 0.2, "out"], [B.take + 0.2, 0], [B.ready, 0], [B.ready + 0.1, -0.2], [B.drop + 0.1, 0.25],
    [B.land + 0.04, 0.3, "out"], [B.land + 0.25, 0], [B.stomp[1] + 0.04, 0.25, "out"], [B.stop - 0.2, -0.25], [B.stop + 0.05, 0.3, "out"], [B.stop + 0.3, 0],
    [B.perch + 0.04, 0.3, "out"], [B.perch + 0.25, 0], [B.proud, -0.12, "out"], [B.proud + 0.25, 0.05], [STORY, 0]]),
  tailSpread: track([[0, 0], [B.clang, 0], [B.clang + 0.06, 0.8], [B.take, 0], [B.drop, 0.2], [B.drop + 0.1, 0.9], [B.land + 0.3, 0.4], [B.stomp[1], 0.6], [B.stop, 0.9], [B.stop + 0.4, 0],
    [B.hop, 0.6], [B.perch, 0.9], [B.perch + 0.35, 0]]),
  puff: track([[0, 0], [B.proud, 0], [B.proud + 0.14, 0.8, "out"], [B.proud + 0.5, 0.1], [STORY, 0]]),
};

// Wings in mesh terms (wingLift + raises them). Hops and the ride keep them
// raised and fluttering for balance; nothing here sweeps them below the body.
function wingsAt(t) {
  const flutter = (rate, amount, phase = 0) => Math.sin((t + phase) * TAU * rate) * amount;
  const startle = Math.sin(clamp((t - B.clang - 0.02) / 0.3) * Math.PI);
  const air = (a, b) => smoothstep(a - 0.04, a + 0.04, t) * (1 - smoothstep(b - 0.04, b + 0.12, t));
  const drop = air(B.drop + 0.02, B.land + 0.08);
  const balance = smoothstep(B.land, B.land + 0.08, t) * (1 - smoothstep(B.land + 0.25, B.land + 0.45, t)) * 0.7
    + air(JUMPS[1][0], B.stomp[0]) * 0.5 + air(JUMPS[2][0], B.stomp[1]) * 0.55;
  const ride = smoothstep(B.stomp[1], B.stomp[1] + 0.12, t) * (1 - smoothstep(B.stop + 0.1, B.stop + 0.35, t));
  const hopOff = air(B.hop - 0.02, B.perch + 0.1);
  const open = Math.max(startle * 0.6, drop * 0.95, balance, ride * 0.95, hopOff * 0.9);
  const lift = startle * 1.0 + drop * (0.85 + flutter(6.5, 0.4)) + balance * (1.1 + flutter(7, 0.2))
    + ride * (0.95 + flutter(8, 0.32)) + hopOff * (0.9 + flutter(6.5, 0.38));
  const weight = Math.max(startle, drop, balance, ride, hopOff, 1e-3);
  return { open, lift: lift / Math.max(1, weight), sweep: 0.15 * ride, tuck: -0.9 * Math.sin(clamp((t - B.proud - 0.6) / 0.22) * Math.PI) };
}

// Feet targets (rig space; y is down) for each support Rivet is on.
function feetFor(t) {
  if (t < B.drop) return { near: { x: 14, y: -jumpAt(t) * 0.25 }, far: { x: -6, y: -jumpAt(t) * 0.25 }, grip: 0.2 };
  if (t < B.land) {
    const reach = smoothstep(B.drop + 0.1, B.land - 0.02, t);
    return { near: { x: lerp(8, 10, reach), y: lerp(-22, 2, reach) }, far: { x: lerp(-4, -2, reach), y: lerp(-24, 4, reach) }, grip: 0.2 };
  }
  if (t < B.hop) return { near: { x: 5, y: -jumpAt(t) * 0.2 }, far: { x: -3, y: -jumpAt(t) * 0.2 }, grip: 1 };
  if (t < B.perch) {
    const reach = smoothstep(B.hop + 0.15, B.perch - 0.02, t);
    return { near: { x: lerp(9, 16, reach), y: lerp(-24, 2, reach) }, far: { x: lerp(-3, -4, reach), y: lerp(-26, 3, reach) }, grip: 0.2 };
  }
  return { near: { x: 16, y: 0 }, far: { x: -4, y: 0 }, grip: 0 };
}

// Rivet's world placement and facing through the story.
function placeAt(t) {
  if (t < B.drop) return { x: START.x, y: START.y - jumpAt(t), facing: -1, perch: 0 };
  if (t < B.land) return { ...drop(t, B.drop, B.land, START, ridePoint(B.land), 30), facing: -1, perch: 0 };
  if (t < B.hop) {
    const p = ridePoint(t);
    return { x: p.x, y: p.y - jumpAt(t), facing: -1, perch: -Math.min(rideAngle(t), 0.55) };
  }
  if (t < B.perch) {
    // He turns in the air, at the top of the hop.
    return { ...hop(t, B.hop, B.perch, ridePoint(B.hop), LANDING, 40), facing: t < (B.hop + B.perch) / 2 ? -1 : 1, perch: 0 };
  }
  return { ...LANDING, facing: 1, perch: 0 };
}

// ---------------------------------------------------------------- scene

export function createVacancydScene() {
  const rivet = new Rivet({ seed: 29 });
  const blink = blinkSchedule(41);
  const neon = createNeonCache();
  const idle = idleProgramme(7);
  const flicker = noise1(91);
  const r = rng(12);
  const stars = Array.from({ length: 70 }, () => ({ x: r.range(-1200, 1200), y: r.range(-900, 120), s: r.range(0.6, 1.8), p: r.range(0, TAU) }));
  const moths = [0, 1].map((i) => ({ seed: 3 + i * 9, nx: noise1(70 + i), ny: noise1(80 + i), home: { x: 70 + i * 50, y: 92 - i * 18 } }));
  const crestSource = (t) => tl.headAngle(Math.min(t, STORY)) * 1.2 + tl.squash(Math.min(t, STORY)) * 0.4;
  const crest = crestFollower(crestSource);
  const fobSwing = TIERS.map((_, i) => springResponse((t) => -(needleAngle(i, t + 0.02) - needleAngle(i, t)) * 22, { stiffness: 60, damping: 4 }));
  const washerSwing = springResponse((t) => (t > BEAT.hang + 0.1 && t < BEAT.hang + 0.35 ? 0.6 : 0) + (t > BEAT.tug && t < BEAT.tug + 0.2 ? -0.5 : 0), { stiffness: 40, damping: 2.5 });
  let camera = { s: 1, w: 1, h: 1, zoom: 1, fx: 0, fy: 0, shakeX: 0, shakeY: 0 };
  let layoutMode = "wide";
  let art = null;
  // Camera keys: zoom and focus (world units). Both frames open on the whole
  // sign with Rivet on top, so the nut, the dial and the prices share the
  // shot; they follow him down onto the dial for the ride, then settle on the
  // sign. Phone frames never crop the sign's width: the wordmark may leave
  // the top, but nothing is clipped at the sides.
  // The take plays in the wide shot, so the slammed needle and Rivet's jump
  // share the frame; the camera then pushes in close for his look at the
  // viewer and pulls back out as he drops.
  const CLOSE = { x: 125, y: -575 };
  const shots = {
    wide: {
      zoom: track([[0, 0.86], [BEAT.climb, 0.86], [BEAT.clang, 0.9, "inOut"], [BEAT.take + 0.12, 0.92], [BEAT.look - 0.02, 2.1, "inOut"], [BEAT.ready, 2.1], [BEAT.drop + 0.08, 1.3, "inOut"],
        [BEAT.land, 1.04, "inOut"], [BEAT.stomp[1], 1.08], [BEAT.stop, 1.16], [BEAT.perch, 1.08, "inOut"], [STORY, 1.06]]),
      fx: track([[0, 30], [BEAT.clang, 30], [BEAT.take + 0.12, 40], [BEAT.look - 0.02, CLOSE.x, "inOut"], [BEAT.ready, CLOSE.x], [BEAT.land, -10, "inOut"], [BEAT.stop, -60], [BEAT.perch, -20, "inOut"]]),
      fy: track([[0, -320], [BEAT.clang, -315], [BEAT.take + 0.12, -320], [BEAT.look - 0.02, CLOSE.y, "inOut"], [BEAT.ready, CLOSE.y], [BEAT.drop + 0.08, -470, "inOut"], [BEAT.land - 0.08, -330, "inOut"],
        [BEAT.land + 0.2, -215, "inOut"], [BEAT.stomp[1], -200], [BEAT.stop, -120], [BEAT.perch, -140, "inOut"], [STORY, -138]]),
    },
    tall: {
      zoom: track([[0, 0.92], [BEAT.take + 0.12, 0.92], [BEAT.look - 0.02, 1.7, "inOut"], [BEAT.ready, 1.7], [BEAT.drop + 0.08, 1.2, "inOut"], [BEAT.land, 0.98, "inOut"],
        [BEAT.stop, 0.96], [BEAT.perch, 0.96]]),
      fx: track([[0, 0], [BEAT.take + 0.12, 0], [BEAT.look - 0.02, CLOSE.x + 22, "inOut"], [BEAT.ready, CLOSE.x + 22], [BEAT.land, -6, "inOut"], [BEAT.stop, -40], [BEAT.hop, -52], [BEAT.perch, -12, "inOut"]]),
      fy: track([[0, -300], [BEAT.take + 0.12, -300], [BEAT.look - 0.02, CLOSE.y, "inOut"], [BEAT.ready, CLOSE.y], [BEAT.drop + 0.08, -460, "inOut"], [BEAT.land - 0.08, -320, "inOut"], [BEAT.land + 0.2, -190, "inOut"],
        [BEAT.stop, -150], [BEAT.perch, -160, "inOut"]]),
    },
  };

  const scene = {
    storyDuration: STORY,
    freeAt: FREE_AT,
    idleEndsAt: idle.asleepAt,
    artIds: () => (art ? art.ids() : []),
    onUnavailable: handler => { rivet.onUnavailable = handler; },

    async prepare() {
      await rivet.ready;
      const [loaded] = await Promise.all([
        loadArt((a) => a.id.startsWith("vacancyd-") || (a.piece || "").startsWith("rivet-")),
        document.fonts.load("700 64px Archivo"),
        document.fonts.load("600 16px Archivo"),
        document.fonts.load("500 20px 'IBM Plex Mono'"),
      ]);
      art = loaded;
      if (art?.size) rivet.usePaint(rivetKit(art));
    },

    layout(w, h) {
      const portrait = h > w * 1.05;
      layoutMode = portrait ? "tall" : "wide";
      // Tall frames hold the sign tightly so Rivet, needles and prices stay
      // large on a phone; wide frames add the motel courtyard behind it.
      const view = portrait ? { w: 540, h: 790 } : { w: 1040, h: 720 };
      camera = { ...camera, s: Math.min(w / view.w, h / view.h), w, h };
    },

    render(ctx, frame) {
      const { t, dpr, w, h } = frame;
      const idleT = frame.idle;
      const now = t + idleT; // continuous clock for ambient motion
      const sleepK = smoothstep(idle.awake, idle.asleepAt, idleT);
      // Camera: a slow push-in while the rates climb, a jolt at the peg and at
      // the stop, then a gentle settle.
      const shot = shots[layoutMode];
      const jolt = [BEAT.clang, BEAT.stop].reduce((sum, at) => sum + (t > at ? Math.exp(-(t - at) * 12) : 0), 0) * 3.2;
      camera.zoom = shot.zoom(t);
      camera.fx = shot.fx(t);
      camera.fy = shot.fy(t);
      const k = camera.s * camera.zoom;
      camera.shakeX = Math.sin(t * 61) * jolt * k;
      camera.shakeY = Math.cos(t * 47) * jolt * 0.6 * k;
      const ref = { zoom: shot.zoom(0), fx: shot.fx(0), fy: shot.fy(0) };
      const base = layerMatrix(camera, ref, 1, dpr);
      const glow = signGlow(t, idleT, flicker, sleepK);

      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.fillStyle = "#080b13";
      ctx.fillRect(0, 0, w * dpr, h * dpr);
      const far = art?.get("vacancyd-far");
      neon.setDensity(camera.s * 1.5 * dpr);
      if (far) {
        // The painted night: sky, moon, motel courtyard and wet lot. Only the
        // passing headlights on its distant road are added live.
        const farMatrix = coverMatrix(far, layerMatrix(camera, ref, far.parallax ?? FAR_PARALLAX, dpr), w * dpr, h * dpr);
        drawPlate(ctx, far, farMatrix);
        ctx.setTransform(farMatrix);
        paintTraffic(ctx, now, sleepK, PAINTED_ROAD_Y);
        ctx.setTransform(base);
        paintPosts(ctx);
      } else {
        // Everything that never changes is drawn once into a backdrop sprite.
        // It is allowed to be a touch soft at full zoom: it reads as depth.
        ctx.setTransform(base);
        const backdrop = neon.sprite("backdrop", BACKDROP, (c) => paintBackdrop(c));
        ctx.drawImage(backdrop.canvas, BACKDROP.x0, BACKDROP.y0, BACKDROP.x1 - BACKDROP.x0, BACKDROP.y1 - BACKDROP.y0);
        paintStars(ctx, stars, now, sleepK);
        paintTraffic(ctx, now, sleepK, 252);
      }
      paintPuddleGlow(ctx, glow);
      const sign = art?.get("vacancyd-sign");
      if (sign) {
        drawPlate(ctx, sign, base);
        paintSignLight(ctx, glow);
      } else {
        paintBoard(ctx, glow);
      }
      paintWordmark(ctx, neon, glow);
      paintGauge(ctx, t, glow, Boolean(sign));
      paintNeedles(ctx, t, fobSwing, FOB_IDS.map((id) => art?.get(id)));
      paintVacancy(ctx, neon, t, glow, flicker, Boolean(sign));

      const pose = poseAt(t, frame, idleT, sleepK);
      const light = { dir: { x: 0.15, y: -1 }, rim: "#ffcf86", rimStrength: 0.7, rimWidth: 1, ambient: "#2a3550", ambientStrength: 0.12 };
      // On the thin needles there is no surface to catch a shadow.
      const onNeedles = smoothstep(BEAT.drop + 0.1, BEAT.land, t) * (1 - smoothstep(BEAT.hop, BEAT.hop + 0.2, t));
      rivet.drawShadow(ctx, pose, base, { strength: 0.42 * (1 - onNeedles * 0.85), color: "#05070c", width: 0.8 });
      rivet.draw(ctx, pose, base, light);
      paintNut(ctx, t, rivet.holdMatrix(pose, base), base, washerSwing, art?.get("vacancyd-hex-nut"));
      paintMoths(ctx, moths, now, t, sleepK);

    },
  };

  function poseAt(t, frame, idleT, sleepK) {
    const place = placeAt(t);
    const feet = feetFor(t);
    const sample = Object.fromEntries(Object.entries(tl).map(([k, f]) => [k, f(t)]));
    const wings = wingsAt(t);
    let pose = {
      ...DEFAULT_POSE,
      ...sample,
      x: place.x,
      y: place.y,
      facing: place.facing,
      scale: RIVET_SCALE,
      perchAngle: place.perch * place.facing,
      footNear: tiltFeet(feet.near, place.perch * place.facing),
      footFar: tiltFeet(feet.far, place.perch * place.facing),
      gripNear: feet.grip,
      gripFar: feet.grip,
      wingOpen: wings.open,
      farWingOpen: wings.open * 0.92,
      wingLift: wings.lift,
      farWingLift: wings.lift + 0.08,
      wingSweep: wings.sweep,
      wingTuck: wings.tuck,
      wingFan: wings.open * 0.6,
      breath: breathAt(t + idleT),
      blink: blink(t + idleT, [[BEAT.clang - 0.1, BEAT.perch]]),
      crest: crest(t + 0) * 0.9,
    };
    // The nut goes onto the fob ring: the bill reaches the ring itself.
    const reach = smoothstep(BEAT.hang - 0.16, BEAT.hang - 0.02, t) * (1 - smoothstep(BEAT.hang + 0.1, BEAT.hop - 0.04, t));
    if (reach > 0) {
      const ring = fobRing(t);
      // The nut's wire loops over the needle tip: the bill goes just past it.
      pose = rivet.reachBeak(pose, { x: ring.x - 8, y: ring.y - 18 }, reach);
    }
    if (t >= STORY) {
      const d = idle.at(idleT);
      pose = applyDeltas(pose, d);
      if (d.crestKick) pose.crest += Math.sin(idleT * 30) * 0.25 * d.crestKick;
      pose = gazeAtPointer(pose, frame);
      // Waking eases out of the doze; the programme's own doze still ends
      // the idle that follows, so a woken Rivet dozes off again.
      const wakeFade = frame.woke ? 1 - smoothstep(0, 1.3, idleT) : 0;
      pose = applyDoze(pose, Math.max(wakeFade, d.doze || 0, sleepK));
      pose.breath = breathAt(t + idleT, lerp(1.7, 0.9, pose.sleep));
    }
    pose._time = t + idleT;
    return pose;
  }

  function gazeAtPointer(pose, frame) {
    if (!frame.pointer) return pose;
    const k = camera.s * camera.zoom;
    const wx = (frame.pointer.x - camera.w / 2) / k + camera.fx;
    const wy = (frame.pointer.y - camera.h / 2) / k + camera.fy;
    const dx = (wx - pose.x) * pose.facing;
    const dy = wy - (pose.y - 110);
    const a = clamp(Math.atan2(dy, Math.max(20, dx)), -0.5, 0.45);
    return { ...pose, headAngle: lerp(pose.headAngle, a * 0.7, 0.75), lookX: clamp(dx / 300, -1, 1), lookY: clamp(dy / 300, -1, 1) };
  }

  return scene;
}

function tiltFeet(foot, slope) {
  return { x: foot.x, y: foot.y + Math.tan(slope) * foot.x };
}

// ---------------------------------------------------------------- painting

function signGlow(t, idleT, flicker, sleepK) {
  const strain = smoothstep(1.2, BEAT.clang, t) * (1 - smoothstep(BEAT.stop - 0.07, BEAT.stop + 0.08, t));
  const buzz = flicker((t + idleT) * 9) * 0.5 + 0.5;
  const drop = strain * (buzz > 0.82 ? 0.55 : 0) + (t > BEAT.clang - 0.02 && t < BEAT.clang + 0.08 ? 0.6 : 0);
  const hum = (1 - sleepK) * flicker((t + idleT) * 0.7) * 0.04;
  return clamp(1 - drop + hum, 0.25, 1.05);
}

function paintStars(ctx, stars, now, sleepK) {
  for (const s of stars) {
    const tw = 0.55 + 0.45 * Math.sin(now * (0.6 + s.s * 0.2) * (1 - sleepK) + s.p);
    ctx.fillStyle = `rgba(214,222,240,${0.25 + tw * 0.4})`;
    ctx.beginPath();
    ctx.arc(s.x, s.y, s.s, 0, TAU);
    ctx.fill();
  }
}

// A car on the far road every so often; the road goes quiet when Rivet dozes.
function paintTraffic(ctx, now, sleepK, roadY) {
  const period = 11;
  const k = ((now + 3) % period) / 4.2;
  if (k >= 1 || sleepK >= 0.5) return;
  const x = lerp(-900, 900, k);
  const g = ctx.createRadialGradient(x, roadY, 1, x, roadY, 60);
  g.addColorStop(0, "rgba(255,240,210,0.85)");
  g.addColorStop(0.15, "rgba(255,220,160,0.25)");
  g.addColorStop(1, "rgba(255,220,160,0)");
  ctx.fillStyle = g;
  ctx.fillRect(x - 60, roadY - 52, 120, 100);
}

function paintPuddleGlow(ctx, glow) {
  const pg = ctx.createRadialGradient(0, GROUND_Y + 40, 10, 0, GROUND_Y + 40, 380);
  pg.addColorStop(0, `rgba(255,170,80,${0.24 * glow})`);
  pg.addColorStop(1, "rgba(255,170,80,0)");
  ctx.fillStyle = pg;
  ctx.fillRect(-420, GROUND_Y, 840, 160);
}

// The unchanging night: sky, moon, clouds, hills, the motel, the office, a
// telephone pole, the lot and the sign's posts.
function paintBackdrop(c) {
  const B = BACKDROP;
  const sky = c.createLinearGradient(0, B.y0, 0, GROUND_Y);
  sky.addColorStop(0, "#04060c");
  sky.addColorStop(0.5, "#0a0e18");
  sky.addColorStop(0.85, "#141a2a");
  sky.addColorStop(1, "#2a2230");
  c.fillStyle = sky;
  c.fillRect(B.x0, B.y0, B.x1 - B.x0, B.y1 - B.y0);
  // Moon with a soft halo, high and to the left of the sign.
  const moon = { x: -455, y: -455, r: 40 };
  const halo = c.createRadialGradient(moon.x, moon.y, moon.r, moon.x, moon.y, moon.r * 5);
  halo.addColorStop(0, "rgba(232,226,206,0.22)");
  halo.addColorStop(1, "rgba(232,226,206,0)");
  c.fillStyle = halo;
  c.fillRect(moon.x - 260, moon.y - 260, 520, 520);
  const disc = c.createRadialGradient(moon.x - 14, moon.y - 12, 6, moon.x, moon.y, moon.r);
  disc.addColorStop(0, "#f6f0dc");
  disc.addColorStop(1, "#c9c0a4");
  c.fillStyle = disc;
  c.beginPath();
  c.arc(moon.x, moon.y, moon.r, 0, TAU);
  c.fill();
  c.fillStyle = "rgba(150,140,115,0.35)";
  for (const [dx, dy, rr] of [[-12, -8, 9], [14, 10, 7], [4, -20, 5], [-18, 16, 6]]) {
    c.beginPath();
    c.arc(moon.x + dx, moon.y + dy, rr, 0, TAU);
    c.fill();
  }
  // Thin cloud banks built from soft puffs, warmed underneath by the town.
  const r = rng(8);
  for (let i = 0; i < 6; i += 1) {
    const cy = r.range(-380, -150);
    const cx = r.range(-760, 680);
    const span = r.range(220, 420);
    for (let j = 0; j < 14; j += 1) {
      const x = cx + (j / 13 - 0.5) * span + r.signed(18);
      const y = cy + r.signed(8);
      const rad = r.range(26, 52) * (1 - Math.abs(j / 13 - 0.5) * 0.9);
      const puff = c.createRadialGradient(x, y + rad * 0.3, 1, x, y, rad);
      puff.addColorStop(0, "rgba(96,82,92,0.16)");
      puff.addColorStop(0.6, "rgba(64,62,82,0.08)");
      puff.addColorStop(1, "rgba(40,44,64,0)");
      c.fillStyle = puff;
      c.fillRect(x - rad, y - rad, rad * 2, rad * 2);
    }
  }
  // Hills and town glow.
  const town = c.createLinearGradient(0, 120, 0, GROUND_Y);
  town.addColorStop(0, "rgba(90,62,44,0)");
  town.addColorStop(1, "rgba(120,78,48,0.45)");
  c.fillStyle = town;
  c.fillRect(B.x0, 100, B.x1 - B.x0, GROUND_Y - 100);
  c.fillStyle = "#0b1019";
  c.beginPath();
  c.moveTo(B.x0, GROUND_Y);
  for (let x = B.x0; x <= B.x1; x += 40) c.lineTo(x, 228 - Math.sin(x * 0.004) * 26 - Math.sin(x * 0.011 + 1) * 10);
  c.lineTo(B.x1, GROUND_Y);
  c.closePath();
  c.fill();
  for (let i = 0; i < 40; i += 1) {
    c.fillStyle = r.next() < 0.7 ? "rgba(255,196,120,0.6)" : "rgba(170,200,255,0.5)";
    c.fillRect(r.range(B.x0, B.x1), r.range(232, 262), 2.2, 2.2);
  }
  paintPole(c);
  paintMotelWing(c);
  paintOffice(c);
  // The lot: asphalt, painted stall lines, wheel stops.
  const lot = c.createLinearGradient(0, GROUND_Y, 0, B.y1);
  lot.addColorStop(0, "#151925");
  lot.addColorStop(1, "#090b11");
  c.fillStyle = lot;
  c.fillRect(B.x0, GROUND_Y, B.x1 - B.x0, B.y1 - GROUND_Y);
  c.strokeStyle = "rgba(220,214,190,0.18)";
  c.lineWidth = 3;
  for (let x = -760; x <= 760; x += 150) {
    c.beginPath();
    c.moveTo(x, GROUND_Y + 20);
    c.lineTo(x - 40, B.y1);
    c.stroke();
  }
  paintPosts(c);
}

function paintPole(c) {
  // A telephone pole on the right with wires sagging across the sky.
  c.fillStyle = "#0c0f17";
  c.fillRect(612, -560, 16, GROUND_Y + 560);
  c.fillRect(560, -540, 120, 10);
  c.fillRect(574, -500, 92, 8);
  c.strokeStyle = "rgba(10,12,18,0.9)";
  c.lineWidth = 2;
  for (const [y0, sag] of [[-536, 90], [-532, 120], [-496, 70]]) {
    c.beginPath();
    c.moveTo(BACKDROP.x0, y0 + 60);
    c.quadraticCurveTo(-60, y0 + sag + 60, 620, y0);
    c.stroke();
  }
}

function paintMotelWing(c) {
  // The end of the motel wing on the left: overhang, posts, siding, doors,
  // a curtained window with its light on, and an ice machine's cold glow.
  const x0 = -900;
  const x1 = -330;
  const roof = 60;
  c.fillStyle = "#151c2c";
  c.fillRect(x0, roof, x1 - x0, GROUND_Y - roof);
  c.strokeStyle = "rgba(255,255,255,0.03)";
  c.lineWidth = 1;
  for (let y = roof + 8; y < GROUND_Y; y += 10) {
    c.beginPath();
    c.moveTo(x0, y);
    c.lineTo(x1, y);
    c.stroke();
  }
  // Overhang and its shadow line.
  c.fillStyle = "#0c111b";
  c.fillRect(x0, roof - 26, x1 - x0 + 40, 30);
  c.fillStyle = "rgba(0,0,0,0.35)";
  c.fillRect(x0, roof + 4, x1 - x0, 18);
  c.fillStyle = "#0e1320";
  for (const x of [-860, -640, -420]) c.fillRect(x, roof + 4, 10, GROUND_Y - roof - 4);
  // Room 108: door and lit window.
  c.fillStyle = "#0a0e17";
  c.fillRect(-760, GROUND_Y - 150, 56, 150);
  c.fillStyle = "rgba(255,190,110,0.55)";
  c.font = "600 13px Archivo, sans-serif";
  c.textAlign = "left";
  c.fillText("108", -752, GROUND_Y - 162);
  const win = { x: -680, y: GROUND_Y - 140, w: 92, h: 62 };
  const wl = c.createLinearGradient(0, win.y, 0, win.y + win.h);
  wl.addColorStop(0, "#ffd08a");
  wl.addColorStop(1, "#e3953f");
  c.fillStyle = wl;
  c.fillRect(win.x, win.y, win.w, win.h);
  c.fillStyle = "rgba(120,60,30,0.55)";
  c.beginPath();
  c.moveTo(win.x, win.y);
  c.quadraticCurveTo(win.x + 30, win.y + 30, win.x + 18, win.y + win.h);
  c.lineTo(win.x, win.y + win.h);
  c.closePath();
  c.moveTo(win.x + win.w, win.y);
  c.quadraticCurveTo(win.x + win.w - 26, win.y + 34, win.x + win.w - 14, win.y + win.h);
  c.lineTo(win.x + win.w, win.y + win.h);
  c.closePath();
  c.fill();
  c.strokeStyle = "#0a0e17";
  c.lineWidth = 4;
  c.strokeRect(win.x, win.y, win.w, win.h);
  const spill = c.createRadialGradient(win.x + win.w / 2, win.y + win.h, 4, win.x + win.w / 2, win.y + win.h, 170);
  spill.addColorStop(0, "rgba(242,180,91,0.3)");
  spill.addColorStop(1, "rgba(242,180,91,0)");
  c.fillStyle = spill;
  c.fillRect(win.x - 170, win.y - 120, win.w + 340, 330);
  // Room 109: dark.
  c.fillStyle = "#0a0e17";
  c.fillRect(-540, GROUND_Y - 150, 56, 150);
  c.fillStyle = "#1a2337";
  c.fillRect(-470, GROUND_Y - 140, 92, 62);
  c.fillStyle = "rgba(255,190,110,0.4)";
  c.fillText("109", -532, GROUND_Y - 162);
  // Ice machine at the end of the walkway.
  c.fillStyle = "#c9d6e6";
  c.fillRect(-372, GROUND_Y - 112, 34, 112);
  c.fillStyle = "#e9f2ff";
  c.fillRect(-368, GROUND_Y - 106, 26, 30);
  c.fillStyle = "#3a6db0";
  c.font = "700 8px Archivo, sans-serif";
  c.fillText("ICE", -362, GROUND_Y - 88);
  const ice = c.createRadialGradient(-355, GROUND_Y - 80, 4, -355, GROUND_Y - 80, 120);
  ice.addColorStop(0, "rgba(170,210,255,0.22)");
  ice.addColorStop(1, "rgba(170,210,255,0)");
  c.fillStyle = ice;
  c.fillRect(-480, GROUND_Y - 200, 250, 220);
}

function paintOffice(c) {
  // The office on the right: blinds lit from inside and a small neon sign.
  const x0 = 330;
  const roof = 110;
  c.fillStyle = "#151c2c";
  c.fillRect(x0, roof, 520, GROUND_Y - roof);
  c.fillStyle = "#0c111b";
  c.fillRect(x0 - 20, roof - 24, 560, 28);
  const win = { x: 372, y: GROUND_Y - 150, w: 170, h: 92 };
  const wl = c.createLinearGradient(0, win.y, 0, win.y + win.h);
  wl.addColorStop(0, "#ffcf86");
  wl.addColorStop(1, "#e09a48");
  c.fillStyle = wl;
  c.fillRect(win.x, win.y, win.w, win.h);
  c.fillStyle = "rgba(110,62,26,0.45)";
  for (let y = win.y + 6; y < win.y + win.h; y += 9) c.fillRect(win.x, y, win.w, 3);
  c.strokeStyle = "#0a0e17";
  c.lineWidth = 5;
  c.strokeRect(win.x, win.y, win.w, win.h);
  c.fillStyle = "#0a0e17";
  c.fillRect(566, GROUND_Y - 150, 58, 150);
  const spill = c.createRadialGradient(457, GROUND_Y - 60, 4, 457, GROUND_Y - 60, 220);
  spill.addColorStop(0, "rgba(242,180,91,0.28)");
  spill.addColorStop(1, "rgba(242,180,91,0)");
  c.fillStyle = spill;
  c.fillRect(240, GROUND_Y - 260, 440, 300);
  // OFFICE in neon over the door: drawn with the glow baked into the backdrop.
  c.save();
  c.font = "700 22px Archivo, sans-serif";
  c.textAlign = "center";
  c.textBaseline = "middle";
  c.lineJoin = "round";
  c.shadowColor = "#ff8a5c";
  c.shadowBlur = 14;
  c.strokeStyle = "#ff8a5c";
  c.lineWidth = 2.4;
  c.strokeText("OFFICE", 595, GROUND_Y - 178);
  c.shadowBlur = 4;
  c.strokeStyle = "#fff0e0";
  c.lineWidth = 0.9;
  c.strokeText("OFFICE", 595, GROUND_Y - 178);
  c.restore();
}

function paintPosts(ctx) {
  for (const x of LAYOUT.POSTS) {
    const g = ctx.createLinearGradient(x - 16, 0, x + 16, 0);
    g.addColorStop(0, "#0c1018");
    g.addColorStop(0.5, "#2a3142");
    g.addColorStop(1, "#0c1018");
    ctx.fillStyle = g;
    ctx.fillRect(x - 14, BOARD.y1 - 10, 28, GROUND_Y - BOARD.y1 + 10);
  }
}

function roundRect(ctx, x0, y0, x1, y1, r) {
  ctx.beginPath();
  ctx.moveTo(x0 + r, y0);
  ctx.arcTo(x1, y0, x1, y1, r);
  ctx.arcTo(x1, y1, x0, y1, r);
  ctx.arcTo(x0, y1, x0, y0, r);
  ctx.arcTo(x0, y0, x1, y0, r);
  ctx.closePath();
}

function paintBoard(ctx, glow) {
  // Sign cabinet: painted steel, a bevel, and rivets along the frame.
  roundRect(ctx, BOARD.x0, BOARD.y0, BOARD.x1, BOARD.y1, 34);
  const g = ctx.createLinearGradient(0, BOARD.y0, 0, BOARD.y1);
  g.addColorStop(0, "#1c2333");
  g.addColorStop(1, "#121827");
  ctx.fillStyle = g;
  ctx.fill();
  ctx.lineWidth = 6;
  ctx.strokeStyle = "#2a3245";
  ctx.stroke();
  const lg = ctx.createRadialGradient(0, WORDMARK_Y, 20, 0, WORDMARK_Y + 60, 520);
  lg.addColorStop(0, `rgba(255,180,84,${0.22 * glow})`);
  lg.addColorStop(1, "rgba(255,180,84,0)");
  ctx.fillStyle = lg;
  roundRect(ctx, BOARD.x0, BOARD.y0, BOARD.x1, BOARD.y1, 34);
  ctx.fill();
  // Brushed steel and an inset frame give the cabinet a surface.
  ctx.save();
  roundRect(ctx, BOARD.x0, BOARD.y0, BOARD.x1, BOARD.y1, 34);
  ctx.clip();
  ctx.strokeStyle = "rgba(255,255,255,0.025)";
  ctx.lineWidth = 1;
  for (let x = BOARD.x0; x < BOARD.x1; x += 5) {
    ctx.beginPath();
    ctx.moveTo(x, BOARD.y0);
    ctx.lineTo(x + 6, BOARD.y1);
    ctx.stroke();
  }
  ctx.restore();
  roundRect(ctx, BOARD.x0 + 14, BOARD.y0 + 14, BOARD.x1 - 14, BOARD.y1 - 14, 24);
  ctx.strokeStyle = "rgba(0,0,0,0.35)";
  ctx.lineWidth = 3;
  ctx.stroke();
  roundRect(ctx, BOARD.x0 + 16, BOARD.y0 + 16, BOARD.x1 - 16, BOARD.y1 - 16, 22);
  ctx.strokeStyle = "rgba(255,200,130,0.08)";
  ctx.lineWidth = 1;
  ctx.stroke();
  ctx.fillStyle = "#3a4458";
  for (let x = BOARD.x0 + 26; x <= BOARD.x1 - 20; x += 46) {
    for (const y of [BOARD.y0 + 16, BOARD.y1 - 16]) {
      ctx.beginPath();
      ctx.arc(x, y, 3.2, 0, TAU);
      ctx.fill();
    }
  }
}

// Neon spill on the painted enamel: warm light that rises and dies with the
// tubes, so the painting (made with the neon off) takes the sign's flicker.
function paintSignLight(ctx, glow) {
  ctx.save();
  roundRect(ctx, BOARD.x0, BOARD.y0, BOARD.x1, BOARD.y1, 40);
  ctx.clip();
  ctx.globalCompositeOperation = "lighter";
  const g = ctx.createRadialGradient(0, WORDMARK_Y, 30, 0, WORDMARK_Y + 40, 420);
  g.addColorStop(0, `rgba(255,170,80,${0.2 * glow})`);
  g.addColorStop(1, "rgba(255,170,80,0)");
  ctx.fillStyle = g;
  ctx.fillRect(BOARD.x0, BOARD.y0, BOARD.x1 - BOARD.x0, BOARD.y1 - BOARD.y0);
  const box = ctx.createRadialGradient(0, (BOX.y0 + BOX.y1) / 2, 10, 0, (BOX.y0 + BOX.y1) / 2, 260);
  box.addColorStop(0, `rgba(255,170,80,${0.12 * glow})`);
  box.addColorStop(1, "rgba(255,170,80,0)");
  ctx.fillStyle = box;
  ctx.fillRect(BOARD.x0, BOX.y0 - 160, BOARD.x1 - BOARD.x0, 320);
  ctx.restore();
}

// Neon glow is far too expensive to blur every frame (it halved the frame
// rate in measurement), so each tube's glow is rendered once per resolution
// into a sprite and composited with its live intensity.
const MAX_SPRITE_PIXELS = 5e6;

function createNeonCache() {
  const sprites = new Map();
  const probe = document.createElement("canvas").getContext("2d");
  let density = 0;
  return {
    probe,
    setDensity(d) {
      if (Math.abs(d - density) / Math.max(d, 1e-6) < 0.02) return;
      sprites.clear();
      density = d;
    },
    sprite(key, bounds, paint) {
      if (!sprites.has(key)) {
        // Large sprites are capped so a phone never allocates a huge bitmap.
        const area = (bounds.x1 - bounds.x0) * (bounds.y1 - bounds.y0);
        const d = Math.min(density, Math.sqrt(MAX_SPRITE_PIXELS / area));
        const canvas = document.createElement("canvas");
        canvas.width = Math.max(1, Math.ceil((bounds.x1 - bounds.x0) * d));
        canvas.height = Math.max(1, Math.ceil((bounds.y1 - bounds.y0) * d));
        const c = canvas.getContext("2d");
        c.setTransform(d, 0, 0, d, -bounds.x0 * d, -bounds.y0 * d);
        paint(c, d);
        sprites.set(key, { canvas, bounds });
      }
      return sprites.get(key);
    },
  };
}

function drawGlow(ctx, sprite, alpha) {
  if (alpha <= 0.02) return;
  const b = sprite.bounds;
  ctx.save();
  ctx.globalCompositeOperation = "lighter";
  ctx.globalAlpha = clamp(alpha);
  ctx.drawImage(sprite.canvas, b.x0, b.y0, b.x1 - b.x0, b.y1 - b.y0);
  ctx.restore();
}

function neonText(ctx, neon, text, x, y, size, color, intensity, { weight = 700, family = "Archivo" } = {}) {
  const font = `${weight} ${size}px ${family}, sans-serif`;
  ctx.save();
  ctx.font = font;
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.lineJoin = "round";
  ctx.lineCap = "round";
  // Off tubes stay faintly visible: glass and dust.
  ctx.strokeStyle = "rgba(120,96,64,0.35)";
  ctx.lineWidth = size * 0.09;
  ctx.strokeText(text, x, y);
  ctx.restore();
  neon.probe.font = font;
  const half = neon.probe.measureText(text).width / 2 + size;
  const bounds = { x0: x - half, x1: x + half, y0: y - size * 1.4, y1: y + size * 1.4 };
  const sprite = neon.sprite(`${text}|${size}|${color}|${x}|${y}`, bounds, (c, density) => {
    c.font = font;
    c.textAlign = "center";
    c.textBaseline = "middle";
    c.lineJoin = "round";
    c.lineCap = "round";
    c.shadowColor = color;
    c.shadowBlur = size * 0.55 * density;
    c.strokeStyle = color;
    c.lineWidth = size * 0.1;
    c.strokeText(text, x, y);
    c.shadowBlur = size * 0.2 * density;
    c.strokeStyle = "#fff4dc";
    c.lineWidth = size * 0.035;
    c.strokeText(text, x, y);
  });
  drawGlow(ctx, sprite, intensity);
}

function paintWordmark(ctx, neon, glow) {
  // The vacancyd hanging-sign mark, in neon.
  const dy = WORDMARK_Y + 334; // the mark is drawn about its own centre at y -334
  const sprite = neon.sprite("mark", { x0: -270, x1: -120, y0: -420 + dy, y1: -260 + dy }, (c, density) => {
    c.shadowColor = "#ffb454";
    c.shadowBlur = 18 * density;
    c.strokeStyle = "#ffb454";
    c.lineWidth = 6;
    c.lineCap = "round";
    c.lineJoin = "round";
    c.beginPath();
    c.moveTo(-232, -385 + dy);
    c.lineTo(-160, -385 + dy);
    c.moveTo(-218, -385 + dy);
    c.lineTo(-218, -372 + dy);
    c.moveTo(-174, -385 + dy);
    c.lineTo(-174, -372 + dy);
    c.stroke();
    roundRect(c, -236, -372 + dy, -156, -296 + dy, 14);
    c.stroke();
    c.beginPath();
    c.moveTo(-216, -352 + dy);
    c.lineTo(-196, -316 + dy);
    c.lineTo(-176, -352 + dy);
    c.stroke();
  });
  drawGlow(ctx, sprite, glow);
  neonText(ctx, neon, "vacancyd", 46, WORDMARK_Y, 82, "#ffb454", glow);
  neonText(ctx, neon, "SPARE COMPUTE · ON YOUR TERMS", 0, TAGLINE_Y, 15, "#ffe2b0", glow * 0.85, { weight: 600 });
}

function paintGauge(ctx, t, glow, isPainted) {
  ctx.save();
  if (isPainted) {
    // The painted enamel is the face; the neon above still warms it.
    ctx.beginPath();
    ctx.arc(0, 0, FACE_R, Math.PI, TAU);
    ctx.closePath();
    const warm = ctx.createRadialGradient(0, -230, 20, 0, -60, 320);
    warm.addColorStop(0, `rgba(255,200,120,${0.16 * glow})`);
    warm.addColorStop(1, "rgba(255,200,120,0)");
    ctx.fillStyle = warm;
    ctx.fill();
  } else {
    paintFace(ctx, glow);
  }
  ctx.clip();
  paintDialMarkings(ctx, t);
  ctx.restore();
  // Stop pins.
  for (const a of [-0.035, PEG + 0.03]) {
    const p = { x: -Math.cos(a) * (NEEDLE_L + 6), y: -Math.sin(a) * (NEEDLE_L + 6) };
    ctx.fillStyle = "#8c6a33";
    ctx.beginPath();
    ctx.arc(p.x, p.y, 4.2, 0, TAU);
    ctx.fill();
  }
  if (isPainted) return;
  ctx.fillStyle = "#1b1f28";
  ctx.fillRect(-BEZEL_R, 0, BEZEL_R * 2, 16);
}

// The procedural bezel and enamel face, leaving the face as the current path.
function paintFace(ctx, glow) {
  ctx.beginPath();
  ctx.arc(0, 0, BEZEL_R, Math.PI, TAU);
  ctx.lineTo(BEZEL_R, 14);
  ctx.lineTo(-BEZEL_R, 14);
  ctx.closePath();
  const bz = ctx.createLinearGradient(0, -BEZEL_R, 0, 14);
  bz.addColorStop(0, "#6d6a63");
  bz.addColorStop(0.08, "#2b2f38");
  bz.addColorStop(1, "#1b1f28");
  ctx.fillStyle = bz;
  ctx.fill();
  // Enamel face, warmed by the neon above it.
  ctx.beginPath();
  ctx.arc(0, 0, FACE_R, Math.PI, TAU);
  ctx.closePath();
  const face = ctx.createRadialGradient(0, -230, 20, 0, -60, 320);
  face.addColorStop(0, mixGlow("#fff1d2", glow));
  face.addColorStop(0.7, "#e9dcbd");
  face.addColorStop(1, "#cdbb94");
  ctx.fillStyle = face;
  ctx.fill();
}

// Rate zones, ticks, labels, the warning and the readouts, inside the face.
function paintDialMarkings(ctx, t) {
  // Rate zones along the left quadrant.
  const zones = [
    [0, phiFor(100), "#d8c69c"],
    [phiFor(100), phiFor(1e5), "#e7b45e"],
    [phiFor(1e5), phiFor(1e7), "#e08938"],
    [phiFor(1e7), PEG + 0.02, "#c5452f"],
  ];
  for (const [a0, a1, color] of zones) {
    ctx.beginPath();
    ctx.arc(0, 0, FACE_R - 18, Math.PI + a0, Math.PI + a1);
    ctx.strokeStyle = color;
    ctx.lineWidth = 14;
    ctx.stroke();
  }
  // Ticks and decade labels.
  ctx.strokeStyle = "#2f2416";
  ctx.fillStyle = "#2f2416";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  for (let d = 0; d <= 10; d += 1) {
    const a = Math.PI + (PEG * d) / 10;
    const major = d % 2 === 0;
    ctx.lineWidth = major ? 2.4 : 1.2;
    ctx.beginPath();
    ctx.moveTo(Math.cos(a) * (FACE_R - 4), Math.sin(a) * (FACE_R - 4));
    ctx.lineTo(Math.cos(a) * (FACE_R - (major ? 34 : 26)), Math.sin(a) * (FACE_R - (major ? 34 : 26)));
    ctx.stroke();
    if (!major) continue;
    const label = ["$0", "$100", "$10K", "$1M", "$100M", "$10B"][d / 2];
    ctx.font = "600 14px Archivo, sans-serif";
    ctx.fillText(label, Math.cos(a) * (FACE_R - 52), Math.sin(a) * (FACE_R - 52));
  }
  // Curved warning in the red zone.
  ctx.save();
  ctx.font = "700 10px Archivo, sans-serif";
  ctx.fillStyle = "#fff1e0";
  const msg = "PLEASE STOP";
  let a = Math.PI + phiFor(1.6e7);
  for (const ch of msg) {
    ctx.save();
    ctx.rotate(a + Math.PI / 2);
    ctx.fillText(ch, 0, -(FACE_R - 18));
    ctx.restore();
    a += 0.038;
  }
  ctx.restore();
  // Fine print and the face's title.
  ctx.font = "700 13px Archivo, sans-serif";
  ctx.fillStyle = "#4a3a24";
  ctx.textAlign = "left";
  ctx.fillText("NIGHTLY RATES", 30, -166);
  ctx.font = "500 9px 'IBM Plex Mono', monospace";
  ctx.fillText("per idle machine · owner first", 30, -152);
  paintReadouts(ctx, t);
}

function mixGlow(hex, glow) {
  return glow > 0.95 ? hex : lerpHex(hex, "#cdbf9d", 1 - glow);
}

function lerpHex(a, b, k) {
  const pa = [1, 3, 5].map((i) => parseInt(a.slice(i, i + 2), 16));
  const pb = [1, 3, 5].map((i) => parseInt(b.slice(i, i + 2), 16));
  return `rgb(${pa.map((v, i) => Math.round(lerp(v, pb[i], clamp(k)))).join(",")})`;
}

function paintReadouts(ctx, t) {
  const rows = [-118, -72, -26];
  TIERS.forEach((tier, i) => {
    const y = rows[i];
    // The mechanism can rebound after the stop; the free offer stays settled.
    const value = t >= FREE_AT ? 0 : priceAt(needleAngle(i, t) - tier.offset);
    ctx.fillStyle = tier.fob;
    ctx.beginPath();
    ctx.moveTo(30, y - 13);
    ctx.lineTo(36, y - 7);
    ctx.lineTo(30, y - 1);
    ctx.lineTo(24, y - 7);
    ctx.closePath();
    ctx.fill();
    ctx.fillStyle = "#3a2c19";
    ctx.textAlign = "left";
    ctx.font = "600 15px Archivo, sans-serif";
    ctx.fillText(tier.name, 42, y - 7);
    ctx.textAlign = "right";
    ctx.font = "500 19px 'IBM Plex Mono', monospace";
    const text = value >= 9.99e9 ? "$9,999,999,999" : "$" + Math.round(value).toLocaleString("en-US");
    ctx.fillStyle = value > 1e8 ? "#a3311f" : value < 0.5 ? "#1f5a3b" : "#2a1f12";
    ctx.fillText(text, 184, y + 14);
  });
  // Once every rate reads $0, a rubber stamp lands across the readouts.
  const k = clamp((t - FREE_AT - 0.04) / 0.16);
  if (k > 0) {
    const slam = 1 + (1 - ease.out(k)) * 0.9;
    ctx.save();
    ctx.translate(112, -152);
    ctx.rotate(-0.16);
    ctx.scale(slam * 0.6, slam * 0.6);
    // Break only the stamp's ink. Erasing from the scene would also punch
    // through the painted dial and the prices underneath it.
    ctx.beginPath();
    ctx.rect(-90, -40, 180, 80);
    const r = rng(77);
    for (let i = 0; i < 40; i += 1) {
      ctx.rect(r.range(-78, 78), r.range(-30, 30), r.range(2, 7), r.range(1, 2.5));
    }
    ctx.clip("evenodd");
    ctx.globalAlpha = 0.82 * ease.out(k);
    ctx.strokeStyle = "#1d6a43";
    ctx.lineWidth = 4;
    roundRect(ctx, -78, -30, 78, 30, 8);
    ctx.stroke();
    ctx.lineWidth = 1.5;
    roundRect(ctx, -71, -23, 71, 23, 5);
    ctx.stroke();
    ctx.fillStyle = "#1d6a43";
    ctx.font = "800 38px Archivo, sans-serif";
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    ctx.fillText("FREE", 0, 2);
    ctx.restore();
  }
}

function paintNeedles(ctx, t, fobSwing, fobs) {
  // Back to front: Penthouse sits beneath Spare Room.
  for (let i = TIERS.length - 1; i >= 0; i -= 1) {
    const tier = TIERS[i];
    const phi = needleAngle(i, t);
    const tip = needlePoint(phi, tier.len);
    ctx.save();
    ctx.lineCap = "round";
    // Counterweight tail and blade.
    const tail = needlePoint(phi, -34);
    ctx.strokeStyle = "#16130f";
    ctx.lineWidth = 7;
    ctx.beginPath();
    ctx.moveTo(tail.x, tail.y);
    ctx.lineTo(0, 0);
    ctx.stroke();
    ctx.fillStyle = "#1b1712";
    ctx.beginPath();
    const n = { x: -Math.sin(phi), y: Math.cos(phi) };
    ctx.moveTo(n.x * 4, n.y * 4);
    ctx.lineTo(tip.x, tip.y);
    ctx.lineTo(-n.x * 4, -n.y * 4);
    ctx.closePath();
    ctx.fill();
    ctx.strokeStyle = "rgba(255,220,170,0.35)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(n.x * 2.5, n.y * 2.5);
    ctx.lineTo(tip.x, tip.y);
    ctx.stroke();
    paintFob(ctx, tip, tier, fobSwing[i](t), fobs[i]);
    ctx.restore();
  }
  // Hub.
  const hub = ctx.createRadialGradient(-3, -3, 1, 0, 0, 13);
  hub.addColorStop(0, "#f1d28f");
  hub.addColorStop(1, "#7a5a26");
  ctx.fillStyle = hub;
  ctx.beginPath();
  ctx.arc(0, 0, 12, 0, TAU);
  ctx.fill();
}

function paintFob(ctx, tip, tier, swing, sprite) {
  if (sprite) {
    paintPaintedFob(ctx, tip, tier, swing, sprite);
    return;
  }
  ctx.save();
  ctx.translate(tip.x, tip.y);
  ctx.strokeStyle = "#b39253";
  ctx.lineWidth = 2;
  ctx.beginPath();
  ctx.arc(0, 4, 5, 0, TAU);
  ctx.stroke();
  ctx.translate(0, 9);
  ctx.rotate(clamp(swing, -0.9, 0.9));
  // The classic diamond key tag.
  ctx.beginPath();
  ctx.moveTo(0, 0);
  ctx.quadraticCurveTo(14, 12, 15, 24);
  ctx.quadraticCurveTo(14, 36, 0, 48);
  ctx.quadraticCurveTo(-14, 36, -15, 24);
  ctx.quadraticCurveTo(-14, 12, 0, 0);
  ctx.closePath();
  ctx.fillStyle = tier.fob;
  ctx.fill();
  ctx.strokeStyle = "rgba(20,14,8,0.8)";
  ctx.lineWidth = 1.2;
  ctx.stroke();
  ctx.fillStyle = "rgba(255,255,255,0.25)";
  ctx.beginPath();
  ctx.ellipse(-5, 16, 3, 8, -0.3, 0, TAU);
  ctx.fill();
  ctx.fillStyle = "rgba(255,248,232,0.92)";
  ctx.font = "700 6.5px Archivo, sans-serif";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.fillText(tier.tag, 0, 25);
  ctx.restore();
}

function paintVacancy(ctx, neon, t, glow, flicker, isPainted) {
  if (!isPainted) {
    roundRect(ctx, BOX.x0, BOX.y0, BOX.x1, BOX.y1, 12);
    ctx.fillStyle = "#0f141f";
    ctx.fill();
    ctx.strokeStyle = "#2c3446";
    ctx.lineWidth = 4;
    ctx.stroke();
  }
  // NO buzzes on as the rates climb and dies when they return to zero.
  const noOn = t > 1.2 && t < FREE_AT ? (flicker(t * 14) > -0.4 || t > 1.5 ? 1 : 0.2) : 0;
  const spark = t > FREE_AT && t < FREE_AT + 0.1 ? (Math.sin(t * 160) > 0 ? 0.8 : 0) : 0;
  neonText(ctx, neon, "NO", -135, (BOX.y0 + BOX.y1) / 2, 30, "#ff6a4a", Math.max(noOn, spark) * glow);
  const vacancyPulse = 1 + smoothstep(FREE_AT, FREE_AT + 0.13, t) * (1 - smoothstep(FREE_AT + 0.13, FREE_AT + 1, t)) * 0.4;
  neonText(ctx, neon, "VACANCY", 40, (BOX.y0 + BOX.y1) / 2, 40, "#ffb454", glow * vacancyPulse);
}

// A brass hex nut: in Rivet's beak until he hangs it on the needles.
// A painted enamel fob: its brass ring hangs on the needle tip and the tag
// swings below it. The room name stays live text on the enamel.
function paintPaintedFob(ctx, tip, tier, swing, sprite) {
  const h = FOB_HEIGHT;
  const w = (h * sprite.image.width) / sprite.image.height;
  ctx.save();
  ctx.translate(tip.x, tip.y);
  ctx.rotate(clamp(swing, -0.9, 0.9));
  ctx.drawImage(sprite.image, -w / 2, -3, w, h);
  ctx.fillStyle = "rgba(255,248,232,0.9)";
  ctx.font = "700 6.5px Archivo, sans-serif";
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  ctx.fillText(tier.tag, 0, h * 0.58);
  ctx.restore();
}

function paintNut(ctx, t, held, base, washerSwing, sprite) {
  // In the beak until he lets go at the fob ring; then it drops on its wire
  // to hang below the tag, the counterweight that keeps the rates at $0.
  const hanging = t >= BEAT.hang + 0.08;
  let m;
  let wire = 0;
  if (!hanging) {
    m = held;
  } else {
    const tip = fobRing(t);
    wire = NUT_STRING * ease.back(clamp((t - BEAT.hang - 0.08) / 0.22));
    m = base.translate(tip.x + 2, tip.y + 10).rotate((washerSwing(t) * 180) / Math.PI).translate(0, wire);
  }
  ctx.save();
  ctx.setTransform(m);
  // In the beak the nut is drawn in rig units; hanging, in world units.
  const s = hanging ? 1 : 1 / RIVET_SCALE;
  ctx.scale(s, s);
  if (hanging) {
    ctx.strokeStyle = "#8c7a52";
    ctx.lineWidth = 1.2;
    ctx.beginPath();
    ctx.moveTo(0, -wire);
    ctx.lineTo(0, -9);
    ctx.stroke();
  }
  if (sprite) {
    const w = NUT_SIZE;
    const h = (w * sprite.image.height) / sprite.image.width;
    ctx.drawImage(sprite.image, -w / 2, -h / 2, w, h);
    ctx.restore();
    return;
  }
  const g = ctx.createLinearGradient(-9, -9, 9, 9);
  g.addColorStop(0, "#ffe7a3");
  g.addColorStop(0.45, "#d4a443");
  g.addColorStop(1, "#6f4f18");
  ctx.fillStyle = g;
  ctx.beginPath();
  for (let i = 0; i < 6; i += 1) {
    const a = (i / 6) * TAU + Math.PI / 6;
    ctx[i ? "lineTo" : "moveTo"](Math.cos(a) * 10, Math.sin(a) * 10);
  }
  ctx.closePath();
  ctx.moveTo(4.2, 0);
  ctx.arc(0, 0, 4.2, 0, TAU, true);
  ctx.fill("evenodd");
  ctx.strokeStyle = "rgba(40,26,8,0.85)";
  ctx.lineWidth = 0.9;
  ctx.stroke();
  // Thread inside the bore and a facet highlight.
  ctx.strokeStyle = "rgba(90,62,20,0.7)";
  ctx.lineWidth = 0.6;
  ctx.beginPath();
  ctx.arc(0, 0, 3.2, 0, TAU);
  ctx.stroke();
  ctx.strokeStyle = "rgba(255,240,200,0.6)";
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(-8.6, -5);
  ctx.lineTo(0, -10);
  ctx.stroke();
  ctx.restore();
}

function paintMoths(ctx, moths, now, t, sleepK) {
  if (sleepK > 0.95) return;
  const appear = smoothstep(BEAT.perch, BEAT.perch + 1, t);
  if (appear <= 0) return;
  for (const m of moths) {
    const x = m.home.x + m.nx(now * 0.9) * 70;
    const y = m.home.y + m.ny(now * 1.1) * 30 - 20;
    const beat = Math.abs(Math.sin(now * 38 + m.seed));
    ctx.save();
    ctx.translate(x, y);
    ctx.globalAlpha = appear * (1 - sleepK);
    ctx.fillStyle = "rgba(255,226,170,0.85)";
    ctx.beginPath();
    ctx.ellipse(-3, 0, 4, 2 + beat * 3, -0.5, 0, TAU);
    ctx.ellipse(3, 0, 4, 2 + beat * 3, 0.5, 0, TAU);
    ctx.fill();
    ctx.fillStyle = "rgba(90,70,40,0.9)";
    ctx.fillRect(-0.8, -2.5, 1.6, 5);
    ctx.restore();
  }
}
