// The mascot appears twice: as the courier floating beside the upload card and as her
// sticker portrait in the logo. Both are decorative, so they carry empty alt text.
import mascotUrl from "./assets/mascot.png";
import mascotLogoUrl from "./assets/mascot-logo.png";

export const brandMarkHtml = `<img class="brand-mark" src="${mascotLogoUrl}" width="192" height="192" alt="" />`;

const n = (value: number) => value.toFixed(1);
const starPath = (cx: number, cy: number, outer: number, inner: number) =>
  `M${Array.from({ length: 10 }, (_, i) => {
    const angle = (i * Math.PI) / 5 - Math.PI / 2;
    const radius = i % 2 ? inner : outer;
    return `${n(cx + Math.cos(angle) * radius)} ${n(cy + Math.sin(angle) * radius)}`;
  }).join("L")}Z`;
const sparklePath = (x: number, y: number, size: number) =>
  `M${n(x)} ${n(y - size)}Q${n(x)} ${n(y)} ${n(x + size)} ${n(y)}Q${n(x)} ${n(y)} ${n(x)} ${n(y + size)}Q${n(x)} ${n(y)} ${n(x - size)} ${n(y)}Q${n(x)} ${n(y)} ${n(x)} ${n(y - size)}Z`;

// The artwork's own envelope was erased from her hand so this one can leave it: it appears
// in her glove, is tossed, and spirals into the galaxy core. A few motes follow it a beat
// behind on the same path. Both sit behind the mascot so her fingers overlap the envelope.
const letterSvg = `<svg viewBox="0 0 33 20" width="33" height="20"><rect x="1" y="1" width="31" height="18" fill="#ffffff" stroke="#5fd4ff" stroke-width="2"/><path d="M1.5 1.5 16.5 11.5 31.5 1.5" fill="none" stroke="#ffc94d" stroke-width="1.6"/><path d="${starPath(16.5, 11, 3.8, 1.6)}" fill="#ffc94d"/></svg>`;
const trail = [1, 2, 3, 4].map((i) => `<span class="letter-trail trail-${i}"></span>`).join("");

export const mascotHtml = `<div class="courier"><span class="letter">${letterSvg}</span>${trail}<img class="mascot" src="${mascotUrl}" width="376" height="680" alt="" /></div>`;

// A loose, painted-looking spiral made from overlapping flecks rather than solid bands.
// All points are generated from a fixed seed, so the artwork is stable across visits.
// The centre remains at (0, 0), where the envelope animation ends.
const random = (() => {
  let seed = 11;
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
})();
const pick = (colors: readonly [string, ...string[]]) =>
  colors[Math.floor(random() * colors.length)] ?? colors[0];
const gaussian = () => (random() + random() + random() + random() - 2) * 1.73;
const polar = (angle: number, radius: number): readonly [number, number] => [
  Math.cos(angle) * radius,
  Math.sin(angle) * radius,
];
const delay = (span: number) => ` style="animation-delay:-${(random() * span).toFixed(2)}s"`;
const blues = ["#173b88", "#2456ae", "#3567bd", "#4c82cc", "#698bc9", "#438ce0"] as const;
const lights = ["#b8dbef", "#e4ecf7", "#c6c8ed", "#f4d9df", "#fce6b7"] as const;

// The varying arm widths and scattered marks make the edges feel like stardust, while
// the blue, blush and warm ivory echo the courier's suit, ears and gold details.
const dust: string[] = [];
const stars: string[] = [];
for (let arm = 0; arm < 2; arm++) {
  for (let i = 0; i < 850; i++) {
    const u = Math.pow(random(), 0.85);
    const radius = 5 + 88 * u;
    const angle = arm * Math.PI + 0.55 + 5.4 * u + gaussian() * (0.12 + 0.21 * u);
    const [x, y] = polar(angle, Math.max(0, radius + gaussian() * (3 + 8 * u)));
    if (Math.hypot(x, y) > 99) continue;
    const isDust = i < 520;
    const color = isDust ? pick(blues) : pick(u < 0.42 ? lights : [...blues, ...lights]);
    const size = isDust ? 0.45 + random() * 1.45 : 0.25 + Math.pow(random(), 2) * 1.15;
    const opacity = isDust ? 0.16 + random() * 0.24 : 0.36 + random() * 0.58;
    const flicker = !isDust && i % 19 === 0 ? ` class="flicker"${delay(3)}` : "";
    const mark = `<ellipse${flicker} cx="${n(x)}" cy="${n(y)}" rx="${n(size * (1 + random() * 0.8))}" ry="${n(size)}" fill="${color}" opacity="${opacity.toFixed(2)}"/>`;
    (isDust ? dust : stars).push(mark);
  }
}

// A handful of uneven, tiny highlights keep the spiral magical without drawing
// perfect rings or competing with the bright detail on the character.
const sparkles: string[] = [];
for (let i = 0; i < 14; i++) {
  const u = 0.2 + random() * 0.72;
  const [x, y] = polar((i % 2) * Math.PI + 0.55 + 5.4 * u + gaussian() * 0.15, 5 + 88 * u);
  const size = 1.3 + random() * 2;
  sparkles.push(
    `<g class="sparkle"${delay(2.8)}><path d="${sparklePath(x, y, size)}" fill="${pick(["#fff5e3", "#d9efff", "#f7d9e8"])}"/></g>`,
  );
}

// A few flecks drift inward, giving the spiral movement between envelope drops.
const infall = [0, 1, 2, 3]
  .map((group) => {
    const marks = Array.from({ length: 3 }, () => {
      const [x, y] = polar(random() * Math.PI * 2, 55 + random() * 35);
      return `<path d="${sparklePath(x, y, 0.7 + random() * 1.2)}" fill="${pick(lights)}"/>`;
    });
    return `<g class="infall" style="animation-delay:-${group * 1.75}s">${marks.join("")}</g>`;
  })
  .join("");

// A cluster of small warm stars suggests the bright heart of the galaxy without
// making the landing point look like a separate object.
const coreDust = Array.from({ length: 90 }, () => {
  const [x, y] = polar(random() * Math.PI * 2, Math.pow(random(), 1.8) * 17);
  return `<circle cx="${n(x)}" cy="${n(y)}" r="${n(0.35 + random() * 1.5)}" fill="${pick(lights)}" opacity="${(0.35 + random() * 0.55).toFixed(2)}"/>`;
}).join("");
const core = `<g class="galaxy-core">${coreDust}<path d="${sparklePath(0, 0, 3.5)}" fill="#fff4dc"/></g>`;

// Impact effects share the envelope's animation clock. Short, irregular streaks
// feel more like scattered starlight than concentric mechanical rings.
const burst = Array.from({ length: 10 }, (_, i) => {
  const [dx, dy] = polar((i / 10) * Math.PI * 2 + 0.3, 35 + (i % 3) * 12);
  return `<g class="burst" style="--dx:${n(dx)}px;--dy:${n(dy)}px"><path d="${sparklePath(0, 0, 2.5 + (i % 2) * 1.3)}" fill="${pick(lights)}"/></g>`;
}).join("");
const shockwave = (extra: string) =>
  `<g class="shockwave${extra}" fill="none" stroke="#d9efff" stroke-width="1" stroke-linecap="round" vector-effect="non-scaling-stroke"><path d="M-9 -3 Q-7 -8 -3 -9 M4 -9 Q8 -7 9 -3 M8 5 Q6 8 3 9 M-5 8 Q-8 6 -9 3"/></g>`;
const impact = `<path class="core-flash" d="${sparklePath(0, 0, 12)}" fill="#fff4dc"/>${shockwave("")}${shockwave(" shockwave-two")}${burst}`;

export const galaxyHtml = `<div class="galaxy"><svg class="galaxy-spin" viewBox="-100 -100 200 200">${dust.join("")}${stars.join("")}</svg><svg class="galaxy-fx" viewBox="-100 -100 200 200">${sparkles.join("")}${infall}${core}${impact}</svg></div>`;
