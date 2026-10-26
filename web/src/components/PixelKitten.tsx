// 14 x 15 sitting kitten, front view. O outline, F fur, I inner ear, E eye,
// N nose, W white muzzle and belly. The tail is fur without an outline.
const SPRITE = [
  ".O........O...",
  ".OO......OO...",
  ".OIO....OIO...",
  ".OFFOOOOFFO...",
  "OFFFFFFFFFFO..",
  "OFFEFFFFEFFO..",
  "OFFEFFFFEFFO..",
  "OFWWWNNWWWFO..",
  ".OFWWWWWWFO...",
  "..OFFFFFFO..F.",
  ".OFFWWWWFFO.F.",
  ".OFFWWWWFFOF..",
  ".OFFWWWWFFF...",
  ".OFWWOOWWFO...",
  "..OOO..OOO....",
];

const COATS = [
  { fur: "#f4a340", patch: "#c96a1b" }, // ginger
  { fur: "#a0a4ab", patch: "#6b7079" }, // grey
  { fur: "#45434c", patch: "#2a2930" }, // black
  { fur: "#f3f1ec", patch: "#d9a46c" }, // white
  { fur: "#e9cfa3", patch: "#b98a52" }, // cream
  { fur: "#8a5a3b", patch: "#5a3824" }, // brown
];
const EYES = ["#3fae5a", "#3b82f6", "#e0a422", "#7c5cd6"];
const PATTERNS = ["solid", "tabby", "patch"] as const;
const TABBY = new Set(["3,2", "3,9", "4,5", "4,6", "10,2", "11,9", "12,2"]);

const INK = "#1d1b22";
const HEADSET = "#4b5563";
const COLOURS: Record<string, string> = { O: INK, I: "#f6a9b8", N: "#f08aa0", W: "#fdfbf6" };

function hash(s: string) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return h >>> 0;
}

function isHeadset(x: number, y: number) {
  return (y === 3 && x >= 4 && x <= 7) || ((y === 5 || y === 6) && (x === 0 || x === 11)) || (y === 7 && x === 1);
}

/** A pixel-art kitten; `seed` fixes its coat, pattern and eye colour. */
export function PixelKitten({
  seed,
  eyesShut = false,
  headset = false,
  size = 3,
}: {
  seed: string;
  eyesShut?: boolean;
  headset?: boolean;
  size?: number;
}) {
  const h = hash(seed);
  const coat = COATS[h % COATS.length];
  const eye = EYES[(h >>> 4) % EYES.length];
  const pattern = PATTERNS[(h >>> 8) % PATTERNS.length];

  const pixels: { x: number; y: number; c: string }[] = [];
  SPRITE.forEach((row, y) =>
    [...row].forEach((ch, x) => {
      if (ch === ".") return;
      let c = COLOURS[ch] ?? coat.fur;
      if (ch === "F") {
        const patched = pattern === "tabby" ? TABBY.has(`${y},${x}`) : pattern === "patch" && x <= 5 && y <= 8;
        // The tail always gets the darker shade so it reads against the body.
        if (patched || (y >= 9 && x >= 11)) c = coat.patch;
      }
      // Open eyes: coloured over a black pupil. Shut: one dark line.
      if (ch === "E") c = eyesShut ? (y === 6 ? INK : coat.fur) : y === 5 ? eye : INK;
      if (headset && isHeadset(x, y)) c = HEADSET;
      pixels.push({ x, y, c });
    }),
  );

  const width = SPRITE[0].length;
  return (
    <svg
      className="kitten"
      width={width * size}
      height={SPRITE.length * size}
      viewBox={`0 0 ${width} ${SPRITE.length}`}
      shapeRendering="crispEdges"
      aria-hidden
    >
      {pixels.map((p) => (
        <rect key={`${p.x},${p.y}`} x={p.x} y={p.y} width={1} height={1} fill={p.c} />
      ))}
    </svg>
  );
}
