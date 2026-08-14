# The cost of censorship resistance in homomorphic range-based set reconciliation

**Paper skeleton, v0.** Working title. The porteur is **not** the "trilemma" (refuted below by ECMH);
it is the **three-costs map**. Planning scaffolding for the *next* paper, not part of the fingerprint
correction on this branch — a candidate to split into its own branch/PR; committed here only so the
work is tracked. Load-bearing gates **S3 (ECMH security) and S3′/E6 (ECMH compute cost) are now
resolved** (§0); the remaining open risk is S6 (novelty of the RBSR-specific framing).

**Thesis (one line).** RBSR wire cost factorizes as `C = |T| · w_fp`, with local cost `T_loc`
alongside. In the *homomorphic-summary* family, censorship resistance is not free and not a single
tax: it must be paid in one of {communication `w_fp`, computation `T_loc`, universality `U`}. The
popular additive-mod-`2^w` combiner (reconcile-rs, Negentropy) is the unique point that appears to
pay none — and therefore silently fails censorship resistance (Wagner). Non-homomorphic RBSR
(Meyer–Scherer) escapes the family via a fourth route, paying with tree-structure freedom.

---

## 0. Status of every load-bearing claim

| # | Claim | Status |
|---|---|---|
| S1 | Wire cost factorizes `C = \|T\| · w_fp` | ✅ definitional |
| S2 | Additive mod `2^w` unkeyed is Wagner-forgeable at `w`=256 (~2³¹) | ✅ Meyer §5.2 + **E3 executed** (`rbsr/tests/wagner_false_convergence.rs`) |
| S3 | Wagner needs a quotient chain; **prime-order EC groups have none**, so ECMH is narrow + C + U + homomorphic | ✅ **confirmed** — ECMH (arXiv:1601.06502v?) §3–§4: EC DL "no known algorithms faster than generic Θ(√\|G\|)"; 256-bit curve = 128-bit security; §4.4 point compresses to `m+1` bits ≈ **32 B on the wire** (so ECMH is `N` too — the skeleton's earlier `¬N` was wrong) |
| S3′ | ECMH's compute cost over additive (the price of the escape) | ✅ **measured — E6** (`scratchpad/e6`, Ristretto): combine **67×** (2.84 ns → 191 ns), lift **126×** (80 ns → 10.1 µs). Paper's optimized GLS254 ≈ 2.4× end-to-end (3 M elt/s). So the corner costs **~2.4× (hand-optimized) to ~100× (off-the-shelf) compute**, at **zero wire cost** |
| S4 | Honest-model false convergence ≤ `2\|T\|(2^{-w}+2^{-τ})`; count-exactness | ◐ Theorem 1/2, routine; the open half Amparore §6.1 defers |
| S5 | `\|T\| = O(b·d·log_b(n/d))`; instance-sensitive in Δ's ordered shape is open | ✅ Meyer §3.2.2 + Amparore §8 (investigation's axis) |
| S6 | The censorship-cost map is novel for the RBSR setting | ❌ **fails** — Meyer §5.2 already owns it: names ECMH ([MSTA17]), states the width-vs-compute tradeoff, and **explicitly asks for the benchmark** ("requires benchmarking to make an informed choice"). See the Verdict below |

## Verdict (S6 closed, 2026-08-14): there is no novel *theory* paper here

Meyer 2212.13567 §5 contains the whole censorship axis: the two-honest-node threat (§5.1), the Wagner
weakness of additive-mod-`2^w` (§5.2), **ECMH as the elliptic-curve option** ([MSTA17], §5.2), the
**width-vs-compute tradeoff** verbatim, and an **explicit call for the benchmark** this skeleton's E6
answers. Amparore §6.1 defers only the honest-model bound (Theorem 1/2 — routine). So Factor B is not
a research contribution; it is *executing what Meyer specified*.

**What genuinely remains, and its honest genre — engineering + measurement, not theory:**

- **E3** — the executable attack against a *shipped* RBSR driver. Meyer's is analysis; nobody had run
  it. Evidence, not a theorem.
- **E6** — the benchmark Meyer §5.2 explicitly asked for (additive vs ECMH combiner cost). A number,
  not a theorem.
- **The reconcile-rs fix** — key the lift (#337 Fix A) or switch to ECMH (Fix B). Real repo value; a
  live vulnerability closed. This is the deliverable that matters.

**Where to take it:** land the fix and the measurement as repo work (PR #338, #337, the analysis
docs), optionally an experience/measurement note — *not* a theory paper. The only genuinely-open
*theoretical* seam is **Factor A** (instance-sensitive `|T|` / split distribution, Amparore §8 +
Vogel) — the **investigation's** axis, not this one. If a paper is wanted, it is there, not here.

---

## 1. Model and the factorization (§ spine)

RBSR over RSOS (Amparore Def. 3.4–3.9). A **fingerprint scheme** = (lift `φ: U → G`, summary
`Σ = ⊕φ`, comparison map `f_p`). Three cost coordinates, not one:

```
C_wire = |T| · w_fp          T_loc  (local compute)          R = rounds
         └────┘  └──┘
       Factor A  Factor B   ← censorship also pushes here
   (split policy) (combiner)
```

- **Factor A** — `|T|`, the refinement-tree size — governed by the split distribution.
  *The investigation's axis.* We take its bound as given and contribute only the join (§4).
- **Factor B** — `w_fp` and `T_loc` — governed by the combiner. *Our axis.*

---

## 2. Factor B — the censorship-cost map (the core contribution)

**Properties.** `N` narrow (`w_fp = O(λ)` bits); `C` censorship-resistant vs a chosen-input
(writing) adversary — split into `C_out` (outsiders) and `C_in` (insiders, i.e. peers you reconcile
with); `U` universal (peer-independent precompute — Meyer–Scherer); `S` fast (`O(log n)` query,
`O(polylog)` update).

| combiner (homomorphic) | N | C_out | C_in | U | S | who |
|---|:--:|:--:|:--:|:--:|:--:|---|
| additive mod `2^w`, **unkeyed** | ✓ | ✗ | ✗ | ✓ | ✓ | **reconcile-rs default**, Negentropy |
| additive mod `2^w`, **shared cluster key** | ✓ | ✓ | ✗ | ✓ | ✓ | §7 repair — fine *iff* cluster = trust domain |
| additive mod `2^w`, **per-peer key** | ✓ | ✓ | ✓ | ✗ | ✓ | — (pays `U`: `O(peers)` precompute) |
| **wide** secure additive (`k`·10³ bits) | ✗ | ✓ | ✓ | ✓ | ✓ | MGS15 (pays `N`: 2688–4160 bits) |
| **prime-order / ECMH** | ✓ | ✓ | ✓ | ✓ | ✗ | Maitin-Shepard — pays only `S`: **2.4×–100× compute** (E6), **0 wire** |
| — *non-homomorphic* (SHA + HI tree) | ✓ | ✓ | ✓ | ✓ | ✓* | Meyer–Scherer (escapes the family) |

`*` non-homomorphic `S` is `O(log n)` normally but `O(n)` on adversarially-degenerate inputs, and it
**requires a history-independent tree** — losing the backing-structure freedom the homomorphic family
keeps.

**Result B (statement to prove).** *No unkeyed homomorphic combiner over a group carrying a
Wagner-exploitable quotient chain (`ℤ/2^w`, smooth `ℤ/N`, `GF(2)^n`) achieves `C` at width `O(λ)`.*
The escapes each pay one coordinate: **width** (wide additive, `¬N`), **universality** (keying, `¬U`
— or only `C_in` if the cluster is a trust domain), or **compute** (prime-order/ECMH, `¬S`). ECMH is
the surprising one: it keeps `N` (§4.4: point compresses to ~32 B, *narrower* than reconcile-rs's
40 B aggregate) and pays **only** `S`. The additive-mod-`2^w` point pays none of the four, hence
`¬C`. **The homomorphic ceiling** (ECMH §3): any homomorphic hash caps at `√|G|` collision security
(second-preimage ≤ collision); ECMH *reaches* it at 256 bits, additive *falls below* it (Wagner) — so
within the family, additive is **strictly dominated by ECMH on security-per-wire-bit**, winning only
on `S`. **Corollary (reconcile-rs):** it sits at row 1 (`¬C`, E3 is the witness); its fixes are the
rows below, and **ECMH is the universality-preserving one** — the alternative to keying (#337) that
does not need a shared secret.

This is what Meyer–Scherer's *"neither option is strictly superior"* + their single lumped "overhead
induced by homomorphic hashing" **miss**: that overhead is inequivalent costs, and **ECMH — a
homomorphic point they did not consider — pays it entirely in compute, at zero wire cost.**

---

## 3. Factor A — `|T|` and the split distribution (cite, don't re-derive)

- `|T| = O(b·d·log_b(n/d))` (Meyer §3.2.2, restated). Instance-sensitive in Δ's **ordered shape** is
  open (Amparore §8); split-distribution optimality is Vogel et al. 2024 in the *channel* model —
  transfer to the RBSR objective (wire + `T_loc` + MTU ceiling) open; ties to **#318**.
- Honest-model false convergence `≤ 2|T|(2^{-w}+2^{-τ})` (Theorem 1) — union over the `|T|`
  comparisons, **not** a birthday bound; count-exactness (Theorem 2). Caveat: the `|T|` dependence of
  the *honest-model* probability is numerically trivial (`2^{-120}·const`). Do not oversell it.
- **Boundary with the investigation:** Factor A is theirs. We import its bound; we do not compete on
  split-distribution optimization.

---

## 4. The join — why "both" is one paper (and honestly, why it's separable)

`Total = |T| · w_fp (+ T_loc)`. The factors are **separable** (optimize each, multiply) — so this is
a systematization with a result per factor, not a single deep theorem. The **one genuinely joint
claim**: the censorship corner (Factor B) sets `w_fp`/`T_loc`, hence the *absolute* value of Factor
A's `|T|` work — ~16–24 B/range (keyed-narrow) vs ~336–520 B/range (wide-secure) vs narrow-but-heavy
`T_loc` (ECMH). **Designer-facing rule: pick the censorship corner first; it sets what the
split-distribution work is worth.** Neither the two papers nor the investigation states this.

---

## 5. Positioning (delta over each)

| prior | has | we add |
|---|---|---|
| Meyer §5.2 | Wagner on `ℤ/2^w` | the **escape map** (ECMH/keyed/wide) as three inequivalent costs |
| Meyer–Scherer | design plane, "neither superior", non-homomorphic corner | their "overhead" is 3 costs; ECMH is a homomorphic point they skipped |
| Amparore §6.1/§8 | defers honest-model bound; names instance-sensitivity | supply the bound; connect it to `\|T\|` |
| Investigation / Vogel | Factor A (split distribution) | the join (§4) + all of Factor B |

---

## 6. Experiments

E1 measure `|T|`(n,d,b,clustering) · E2 honest-model bound at reduced `w` · **E3 Wagner false-SKIP —
done** · E4 cost extrapolation · **E6 ECMH `T_loc` vs additive — done** (`scratchpad/e6`; combine 67×,
lift 126× off-the-shelf Ristretto; ~2.4× with the paper's GLS254) · E7 keyed-lift wire/compute
overhead · **E8 (new) full ECMH combiner inside `FingerprintTreeMap`** — end-to-end insert/query/round
`T_loc`, plus Ristretto point-compression cost per range on the wire path (E6 measured 4.9 µs/compress,
cacheable but real at `|T|` ranges/round).

## 7. Risks (ranked)

1. ~~S3~~ ✅ **resolved** — ECMH is Wagner-immune, narrow (32 B wire), universal, homomorphic. The map
   holds; the pivot away from the trilemma stands.
2. ~~E6 magnitude~~ ✅ **measured** — not cheap: ~67–126× per op off-the-shelf (µs-scale absolute),
   ~2.4× hand-optimized. So the compute corner has **real teeth** — this is a genuine speed/security
   trade, not a free "switch to ECMH". Strengthens the case that the *map* (not a single winner) is
   the contribution.
3. **S6 — now the top open risk.** A homomorphic-hash survey may already tabulate the width/compute
   zoo (ECMH's own §1 compares AdHash/MuHash/ECMH). Our delta must be the **RBSR-specific** framing:
   `C = |T|·w_fp`, the censorship/universality axes, and the false-convergence tie-in — not the
   crypto zoo itself. Verify no one has done *that*.
4. Factor A transfer (Vogel channel-model → RBSR) is the investigation's risk, not ours.
