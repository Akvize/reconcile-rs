// Drives Negentropy over a synthetic (n, d) instance and counts the *refinement* columns
// `benches/protocol.rs` counts, by parsing the emitted wire messages per docs/negentropy-protocol-v1.md.
// Nothing here reads Negentropy internals: the parser is written from the published spec, so it
// doubles as a conformance check on our reading of that spec.

const { Negentropy, NegentropyStorageVector } = require('./Negentropy.js');
const crypto = require('crypto');

// --- spec parser (docs/negentropy-protocol-v1.md) ---------------------------------------------
// Varint := <Digit+128>* <Digit>   (base-128, most significant digit first)
function readVarint(buf, o) {
    let v = 0;
    for (;;) {
        const b = buf[o.i++];
        v = v * 128 + (b & 127);
        if ((b & 128) === 0) return v;
    }
}

// Range := <upperBound (Bound)> <mode (Varint)> <payload>
// Bound := <encodedTimestamp (Varint)> <length (Varint)> <idPrefix (Byte)*>
// mode 0 = Skip (no payload), 1 = Fingerprint (16 B), 2 = IdList (<len varint> <32 B ids>*)
function parseMessage(buf) {
    const o = { i: 0 };
    const version = buf[o.i++];
    if (version < 0x60 || version > 0x6f) throw new Error('bad protocol version byte');
    const out = { version, ranges: [] };
    while (o.i < buf.length) {
        const start = o.i;
        readVarint(buf, o);                    // bound: timestamp
        const prefixLen = readVarint(buf, o);  // bound: idPrefix length
        o.i += prefixLen;                      // bound: idPrefix
        const boundBytes = o.i - start;
        const mode = readVarint(buf, o);
        let ids = 0;
        if (mode === 0) {
            // Skip: no payload
        } else if (mode === 1) {
            o.i += 16;                         // Fingerprint := Byte{16}
        } else if (mode === 2) {
            ids = readVarint(buf, o);
            o.i += ids * 32;                   // Id := Byte{32}
        } else {
            throw new Error('unknown mode ' + mode);
        }
        out.ranges.push({ mode, bytes: o.i - start, boundBytes, ids });
    }
    if (o.i !== buf.length) throw new Error('trailing bytes: parser and spec disagree');
    return out;
}

// --- instance ---------------------------------------------------------------------------------
// Modeling choice, recorded in the fixture: our store is keyed by u64 and ordered by key;
// Negentropy orders by (timestamp, id). To make the two refine over the *same logical ordering of
// n items*, each item gets a distinct ascending timestamp equal to its index, and a deterministic
// 32-byte id. This isolates refinement structure from any timestamp-clustering effect.
function idOf(k) {
    return crypto.createHash('sha256').update(Buffer.from(String(k))).digest();
}

function buildStorage(n, skip) {
    const s = new NegentropyStorageVector();
    for (let k = 0; k < n; k++) {
        if (skip.has(k)) continue;
        s.insert(k, idOf(k));
    }
    s.seal();
    return s;
}

async function run(n, d) {
    // `d` elements that peer B lacks, spread evenly — `benches/protocol.rs`'s `Clustering::Spread`.
    const missing = new Set();
    for (let j = 0; j < d; j++) missing.add(Math.floor(((j + 0.5) * n) / d));

    const a = new Negentropy(buildStorage(n, new Set()), 0);   // has everything
    const b = new Negentropy(buildStorage(n, missing), 0);     // lacks `d`
    a.wantUint8ArrayOutput = true;
    b.wantUint8ArrayOutput = true;

    const cost = {
        messages: 0, bytes: 0, ranges: 0,
        fingerprint_ranges: 0, fingerprint_bytes: 0,
        skip_ranges: 0, skip_bytes: 0,
        idlist_ranges: 0, idlist_bytes: 0, idlist_ids: 0,
    };
    const tally = (buf) => {
        const m = parseMessage(Buffer.from(buf));
        cost.messages += 1;
        cost.bytes += buf.length;
        cost.ranges += m.ranges.length;
        for (const r of m.ranges) {
            if (r.mode === 1) { cost.fingerprint_ranges++; cost.fingerprint_bytes += r.bytes; }
            else if (r.mode === 0) { cost.skip_ranges++; cost.skip_bytes += r.bytes; }
            else { cost.idlist_ranges++; cost.idlist_bytes += r.bytes; cost.idlist_ids += r.ids; }
        }
    };

    let msg = await a.initiate();
    tally(msg);
    for (let round = 0; msg !== null && round < 128; round++) {
        const [reply] = await b.reconcile(msg);       // B answers
        if (reply === null) break;
        tally(reply);
        const [next] = await a.reconcile(reply);      // A answers
        msg = next;
        if (msg !== null) tally(msg);
    }
    return cost;
}

(async () => {
    const n = parseInt(process.argv[2] || '1000000', 10);
    const d = parseInt(process.argv[3] || '1', 10);
    const t0 = Date.now();
    const cost = await run(n, d);
    console.log(JSON.stringify({
        n, d,
        elapsed_ms: Date.now() - t0,
        ...cost,
        fingerprint_bytes_per_range: cost.fingerprint_ranges
            ? +(cost.fingerprint_bytes / cost.fingerprint_ranges).toFixed(2) : null,
    }, null, 2));
})();
