// node site/f/test.js — the page's decryption against the Rust side's known answer.
const RIMF = require("./rimf.js");
const hex = "52494d46010000003ee394472cd4b172fa7955dd556f68d324c8303df504605fcfad618901f51e2cb846e3c3c4e2c1c3707446237a9b382a8ec9032d6373016c12efebd65975a2f146a060f42be1bea8237a1f6619e964c9e7ceaa61";
const bytes = Uint8Array.from(hex.match(/../g).map((h) => parseInt(h, 16)));
function reader(data, step) {
  let i = 0;
  return { read: async () => (i >= data.length ? { done: true } : { value: data.slice(i, (i += step)), done: false }) };
}
(async () => {
  const key = new Uint8Array(32).fill(0x11);
  for (const step of [1, 7, 1000]) {
    let out = "";
    const meta = await RIMF.decryptStream(reader(bytes, step), key, async () => {}, async (c) => { out += new TextDecoder().decode(c); });
    if (meta.name !== "hi.txt" || out !== "hello") throw new Error(`step ${step}: ${meta.name} ${out}`);
  }
  let failed = false;
  try { await RIMF.decryptStream(reader(bytes, 50), new Uint8Array(32), async () => {}, async () => {}); } catch (e) { failed = /wrong key/.test(e.message); }
  if (!failed) throw new Error("a wrong key was not noticed");
  try { await RIMF.decryptStream(reader(bytes.slice(0, bytes.length - 3), 50), key, async () => {}, async () => {}); failed = false; } catch (e) { failed = /incomplete/.test(e.message); }
  if (!failed) throw new Error("a cut-off file was not noticed");
  const p = RIMF.parseHash("#https://drop.example.org/f/abc," + Buffer.from(key).toString("base64url"));
  if (!p || p.url !== "https://drop.example.org/f/abc" || p.key[0] !== 0x11) throw new Error("hash");
  if (RIMF.parseHash("#javascript:alert(1)," + Buffer.from(key).toString("base64url"))) throw new Error("accepted a non-https url");
  console.log("ok");
})().catch((e) => { console.error(e); process.exit(1); });
