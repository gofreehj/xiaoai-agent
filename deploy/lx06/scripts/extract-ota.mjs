import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "../../..");
const ota = path.resolve(
  process.argv[2] ??
    path.join(repoRoot, "deploy/client-patch/assets/mico_all_911151b95_1.94.13.bin"),
);
const reportDir = path.resolve(
  process.argv[3] ?? path.join(repoRoot, "deploy/lx06/analysis/lx06-1.94.13"),
);
const out = `${reportDir}/extracted`;
const bytes = await readFile(ota);
const u16 = (at) => bytes.readUInt16LE(at);
const u32 = (at) => bytes.readUInt32LE(at);
const ascii = (start, length) => bytes.subarray(start, start + length).toString("ascii").replace(/\0.*$/, "");

if (ascii(0, 4) !== "HDR1") throw new Error("Expected HDR1 OTA container.");
const signatureOffset = u32(4);
const image = {
  file: path.basename(ota),
  length: bytes.length,
  magic: ascii(0, 4),
  signatureOffset,
  crc32: `0x${u32(8).toString(16).padStart(8, "0")}`,
  fileType: u16(12),
  modelId: u16(14),
  sha256: createHash("sha256").update(bytes).digest("hex"),
  md5: createHash("md5").update(bytes).digest("hex"),
  segments: [],
};

await mkdir(out, { recursive: true });
for (let i = 0; i < 8; i += 1) {
  const offset = u32(16 + i * 4);
  if (offset === 0) continue;
  if (offset + 48 > signatureOffset) throw new Error(`Segment ${i} header is outside payload.`);
  const flashAddress = u32(offset + 4);
  const length = u32(offset + 8);
  const partition = u32(offset + 12);
  const name = ascii(offset + 16, 32);
  if (!/^[A-Za-z0-9._-]+$/.test(name)) throw new Error(`Unsafe segment name: ${JSON.stringify(name)}`);
  const payloadStart = offset + 48;
  const payloadEnd = payloadStart + length;
  if (payloadEnd > signatureOffset) throw new Error(`Segment ${name} payload is outside signed data.`);
  const payload = bytes.subarray(payloadStart, payloadEnd);
  await writeFile(path.join(out, name), payload, { flag: "wx" });
  image.segments.push({
    index: i,
    name,
    headerOffset: `0x${offset.toString(16)}`,
    flashAddress: `0x${flashAddress.toString(16)}`,
    partition,
    length,
    sha256: createHash("sha256").update(payload).digest("hex"),
    md5: createHash("md5").update(payload).digest("hex"),
  });
}

await writeFile(`${reportDir}/ota-manifest.json`, `${JSON.stringify(image, null, 2)}\n`, { flag: "w" });
console.log(JSON.stringify(image, null, 2));
