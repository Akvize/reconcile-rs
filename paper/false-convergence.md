# False convergence in range-based set reconciliation

**Working draft.** The end-to-end collision analysis Amparore (arXiv:2603.19820) §6.1 declares out of
scope: a bound on the probability that an RBSR execution declares convergence on two sets that
differ, as a function of the comparison-map width `τ`, the summary width `w`, the set size `n`, the
symmetric difference `d`, the fan-out `b`, and the number of rounds.

The bound turns out not to be the interesting half. Stating it precisely forces a split into two
independent failure layers, and the layer that binds is **not** the one the 40 B/16 B debate is
about.

---

## 0. Verification gates

Load-bearing facts, and their status. Nothing downstream is safe until these are green.

| # | Claim | Status |
|---|---|---|
| V1 | Negentropy: `f_p = trunc_16B(SHA-256(Σ ‖ varint(count)))`, `Σ` = addition mod 2²⁵⁶ of 32-byte LE ids | ✅ verified against `hoytech/negentropy` `docs/negentropy-protocol-v1.md` |
| V2 | Amparore §6.1 calls `f_p` "probabilistically sound rather than information-theoretically exact" and leaves collision analysis out of scope | ✅ **confirmed verbatim** — PDF read, §0.1 |
| V3 | Wagner: collisions for AdHash on an `n`-bit modulus in `O(2^(2√n))`; modulus must exceed ~1600 bits for 80-bit security | ✅ verified (secondary sources, §10); Meyer §5.2 cites the concrete digest sizes ([MGS15]), §0.1 |
| V4 | Meyer formalizes fingerprint security and **dismisses** adversarial collisions on the withholding grounds | ❌ **REFUTED** — Meyer §5.1 does the opposite: he names the two-honest-node attack as *the* real one. §0.1 |
| V5 | Nobody has published the end-to-end `(τ, w, n, d, b, R)` bound | ◐ **split** — the *adversarial* half is Meyer §5.2 (Wagner, published 2023); the *honest-model quantitative* half is what Amparore §6.1 defers, and it is genuinely open. §0.1 |
| V6 | Clarke et al. (ASIACRYPT 2003): MSet-Add-Hash is set-collision-resistant **only** keyed | ⚠️ re-read; Meyer proposes *widening* or DLP/lattice, not keying — keying is the repo-specific fix (§7) |
| V7 | The k-tree applies to `ℤ/2^w` with no error term, and a planted solution makes the real driver SKIP | ✅ **executed** — `rbsr/tests/wagner_false_convergence.rs`, §6.1 |

## 0.1 Verification outcome (2026-08-14, both PDFs read)

**The proposed adversarial contribution (§6) is substantially pre-empted by Meyer 2023 §5.** The
verdicts, quote-backed:

- **V2 ✅.** Amparore §6.1, verbatim: *"Negentropy's concrete comparison rule fpNE should be read as
  probabilistically sound rather than information-theoretically exact. A false SKIP may arise
  whenever two unequal ranges yield the same comparison value, **either because the underlying
  aggregate A(·) is not injective as a set summary, or because distinct encodings of aggregates
  collide after hashing and truncation to 128 bits**. … A full end-to-end collision analysis of fpNE
  is outside the scope of the present paper."* The bold clause **is** this document's §2 L1/L2 split —
  Amparore states it in one sentence; §2 only makes it precise.

- **V4 ❌ refuted.** Meyer §5.1 does not dismiss the attack — he *defines* it as the real one:
  *"the cases in which a malicious node can do actual damage by finding a collision are those where
  it supplies data to two honest nodes such that these two nodes perform faulty synchronization
  amongst each other. Specifically: let M be a malicious node, A B be honest nodes, then a successful
  attack consists of crafting sets X_A, X_B and sending these to A and B respectively, so that when
  [they] run the synchronization protocol, they end up with distinct sets."* That is exactly §6.2.
  Withholding is the case Meyer dismisses (*"does not require finding collisions at all"*), and §6.4
  attacked that dismissal as if it covered the two-node attack. **It does not. §6.4's thesis targets
  a claim Meyer never made.**

- **V5 ◐ split.** Meyer §5.2 already contains the entire cryptanalytic core of §6: *"For addition,
  the balance problem is as hard as subset sum … Wagner showed however in [Wag02] how to solve the
  balance problem in subexponential time for addition. [MGS15] suggests addition for combining SHA-3
  digests, and proposes using fingerprints of length between 2688 and 4160 or 6528 to 16512 bits to
  achieve security levels of 128 or 256 bit respectively against Wagner's attack. [Lyu05] gives an
  improvement over Wagner's attack finding collisions in O(2^(n^ε))."* So §6.1 (Wagner over `ℤ/2^w`),
  §6.3 (256-bit addition is weak), §6.5 (xor/GF(2) via [BM97]) and most of §10's bibliography are
  **Meyer 2023, not new.** What is *not* in either paper: the explicit honest-model bound
  `2C(n,d,b)·(2^−w + 2^−τ)` with the union taken over **comparisons** (Meyer's §5 opener is
  qualitative; his pigeonhole count is over all `2^n` subsets) — this is precisely what Amparore
  §6.1 defers, and precisely what the original prompt asked for. **Theorem 1 + Theorem 2 are the
  surviving open core; §6 is context.**

**Consequence for the framing.** This is not "the real paper" as an adversarial-crypto result — that
result is Meyer's. What survives is (a) the honest-model end-to-end bound Amparore explicitly left
open, (b) Theorem 2's count-exactness refinement, which is in neither paper, (c) the executable
demonstration that Meyer's paper attack **bites the shipped `reconcile-rs` driver** (E3), and (d) a
material correction to this repo's own docs — see §11, promoted from a footnote to the main result.

---

## 1. Model

Two peers hold finite sets `X, Y ⊆ U` of `(key, value)` pairs over a totally ordered key space.

- **Lift** `φ: U → G`, `G = (ℤ/2^w, +)`, `w = 256`. Here `φ = BLAKE3(enc(key) ‖ enc(value))` read as a
  little-endian integer (`rsos/src/fingerprint.rs`). Negentropy takes `φ(u) = id(u)`, an
  application-supplied 256-bit id; AELMDB extracts a byte slice at a configured offset.
- **Aggregate** `A(S) = (|S|, Σ(S))`, `Σ(S) = Σ_{u∈S} φ(u) ∈ G` — Def. 3.5's monoid `ℕ × G`.
- **Comparison map** `f_p: ℕ × G → C`. Two instantiations:
  - `rbsr`: `f_p = id`, `C = ℕ × G`, 40 B on the wire.
  - Negentropy: `f_p(c, σ) = trunc_τ(SHA-256(σ ‖ varint(c)))`, `τ = 128`, 16 B on the wire.
- **Protocol.** Peers exchange `(range, f_p(A(·∩range)))`. A range is **SKIP**ped iff the two
  comparison values are equal; otherwise it is split by **rank** into `≤ b` children, or enumerated.

**Definition (false convergence).** An execution *falsely converges* if it SKIPs some range `r` with
`X∩r ≠ Y∩r`. The consequence is not a lost element but a lost *subtree*: everything under `r` is
abandoned, silently and — see §6 — permanently.

**Definition (soundness assumption).** Prop. 4.1's SKIP rule is sound iff

> `f_p(A(X∩r)) = f_p(A(Y∩r)) ⟹ X∩r = Y∩r`.

This holds for no finite `C`; the whole question is the probability with which it fails, and under
which adversary.

---

## 2. The decomposition

A false SKIP on `r` is exactly one of two disjoint events. Write `a = A(X∩r)`, `b = A(Y∩r)`.

- **L1 — aggregate collision.** `a = b` while `X∩r ≠ Y∩r`. Equivalently `|X∩r| = |Y∩r|` **and**
  `Σ(X∩r) = Σ(Y∩r)` on different content. A property of `(φ, G)` alone: **the comparison map is not
  involved.**
- **L2 — comparison-map collision.** `a ≠ b` but `f_p(a) = f_p(b)`. A property of `f_p` alone.

> `P[false SKIP on r] = P[L1] + P[L2]`.

`f_p = id` ⟹ `P[L2] = 0` exactly. This is the whole of `rbsr`'s claim over Negentropy, and it is
correct as far as it goes. §6 is about how far that is.

L1 is *not* affected by widening or narrowing `f_p`. **No choice of comparison map can repair the
lift.** That single observation is what reframes the 40 B/16 B question.

---

## 3. Lemma 1 — how many comparisons an execution performs

Let `C(n, d, b)` be the number of range comparisons in one reconciliation of two sets of size `≤ n`
with `|Δ| = d`, `Δ = (X\Y) ∪ (Y\X)`, under fan-out `b`.

*Proof sketch.* A range containing no element of `Δ` satisfies `X∩r = Y∩r` and always agrees, so it
is never split. Hence every live range at depth `i` contains an element of `Δ`; live ranges at a
given depth are pairwise disjoint, so at most `min(b^i, d)` of them are live. Each emits `≤ b`
children. Splitting is by rank, so span falls by a factor `≥ b` per level and depth is
`h ≤ ⌈log_b n⌉`. Summing, with `i* = ⌊log_b d⌋`:

```
C(n, d, b)  ≤  1 + Σ_{i=0}^{h-1} b·min(b^i, d)
            ≤  1 + b·d·( ⌈log_b n⌉ − ⌊log_b d⌋ + b/(b−1) )
            =  O( b·d·log_b(n/d) )
```

Both peers compare, so an execution performs `≤ 2C` comparisons. `d = 0` gives `C = 1`.

*Sanity check against the repo.* `n = 10⁶, d = 1, b = 16` gives `C ≤ 98`; `benches/protocol.rs`
measures 155 `Aggregate` queries for that configuration (comparisons plus the children's own
aggregates). Same order — **§9 turns this into a real measurement rather than a spot check.**

**Why this lemma is legitimate at all:** the compared ranges are cut by `select`, i.e. by **rank**,
so the *set of ranges an execution compares is a function of the data alone and is independent of
`φ`*. That independence is what licenses the union bound in §4. A structure whose shape is
hash-derived (MST: level = `hash(key)`) does not get it for free.

---

## 4. Theorem 1 — the honest-model bound

Model `φ` as a random oracle into `G` and `SHA-256` as a random oracle into `{0,1}^τ`, with the two
sets fixed independently of both.

> **P[an execution falsely converges] ≤ 2·C(n, d, b) · ( 2^(−w) + 2^(−τ) )**
>
> with the `2^(−τ)` term identically zero when `f_p = id`.

*Proof.* Fix a compared range `r`; by §3 the range is `φ`-independent. **L1:** `Σ(X∩r) − Σ(Y∩r)` is
a signed sum of oracle outputs over the non-empty set `Δ∩r`, hence uniform on `G`; it is zero with
probability `2^(−w)`. **L2:** `a ≠ b` are two fixed distinct inputs to the oracle, colliding after
truncation with probability `2^(−τ)`. Union over the `≤ 2C` comparisons. ∎

**This is a union bound over comparisons, not a birthday bound.** The distinction is the practical
content of the theorem. Intuition says "128-bit output ⟹ 64 bits of security"; that is the cost of
finding *some* collision among many values. Here every comparison tests one *designated* pair, so
`τ` bits buy `τ − log₂(2C)` bits of margin, not `τ/2`.

Numerically, with `b = 16`:

| `n` | `d` | `C` | `τ = 128` | `τ = 64` | `w = 256`, `f_p = id` |
|---:|---:|---:|---:|---:|---:|
| 10⁶ | 1 | ~2⁶·⁶ | 2⁻¹²¹ | 2⁻⁵⁷ | 2⁻²⁴⁹ |
| 10⁹ | 10⁶ | ~2²⁶·³ | 2⁻¹⁰¹ | 2⁻³⁷ | 2⁻²²⁹ |

**Reading.** Over a system lifetime of `2³⁰` reconciliations at `d = 10⁶` (≈ 2⁵⁶ distinct
comparisons — deliberately generous), `τ = 128` still leaves `2⁻⁷²`. `τ = 64` does not survive the
same budget. So Negentropy's 128 bits is not a compromise, it is roughly the right number; and
`w = 256` with `f_p = id` is ~120 bits of margin nobody can spend.

**In the honest model the extra 24 B/range buys nothing measurable.** That is half the answer to the
question this document exists for. §5 and §6 are the other half, and they disagree with each other.

---

## 5. Theorem 2 — count exactness is free, and truncation spends it

> With `f_p = id`, a range on which the peers hold **different numbers of elements** is never
> SKIPped. Probability 1, no assumption on `φ`, no assumption on the hash.

*Proof.* `f_p = id` ⟹ SKIP requires equality in `ℕ × G`, whose first component is `|X∩r|`. ∎

**Corollary.** A false SKIP requires the difference inside the skipped range to be **balanced**:
`|X∩r \ Y| = |Y∩r \ X|`. In particular no execution can ever falsely converge on `d = 1`, or on any
unbalanced difference, and this is a *certainty*, not a probability.

Under `f_p = trunc_τ(H(σ ‖ count))` the count is folded into the digest before truncation, so the
guarantee degrades to `1 − 2^(−τ)`. **Truncation does not merely cost margin; it converts an exact
structural invariant into a probabilistic one.** This is a qualitative difference, and it is the
strongest argument on record for the wide aggregate.

**Two caveats, both sharpening it.**

1. The dominant real-world failure — a dropped update leaving one peer one element short — is
   unbalanced, so Theorem 2 covers exactly the case that actually happens.
2. The dominant *conflict* — same key, different value — contributes one element to each side and is
   therefore **balanced**. Theorem 2 does not cover LWW value divergence. This is worth stating
   loudly because it is the case a reader will assume is covered.

**And the trade is separable.** Nothing forces the count to travel inside the truncated digest.
`f_p = (count, trunc_τ(Σ))` keeps Theorem 2 exactly and costs `8 + 16 = 24 B`, or ~17–20 B with a
varint count — against 40 B today and 16 B for Negentropy. See §7.

---

## 6. Theorem 3 — the adversarial model, where the analysis actually bites

Theorem 1 assumes the sets are fixed independently of `φ`. Drop that. An attacker who can **write**
to the store chooses the elements, hence chooses `φ`-preimages, and the union bound is void.

### 6.1 The additive combiner is a k-sum instance

To force a false SKIP, an attacker needs a non-empty multiset difference with

```
Σ_{u ∈ P_X} φ(u)  ≡  Σ_{u ∈ P_Y} φ(u)   (mod 2^w),     |P_X| = |P_Y|
```

i.e. `Σ_i s_i·φ(u_i) ≡ 0 (mod 2^w)` with `s_i = ±1` — **exactly** Wagner's k-sum problem, with `φ`
freely samplable (grind the key or the value, one BLAKE3 per candidate).

Wagner's k-tree applies to `ℤ/2^w` **without any error term**: reduction mod `2^j` is a group
homomorphism `ℤ/2^w → ℤ/2^j`, so merging on *low-order* bits is exact — carries propagate upward and
never disturb a matched low window. The k-tree over `ℤ/2^w` is structurally the XOR case.

With `k = 2^t` lists of size `2^(w/(t+1))`, work is `2^(t + w/(t+1))`, minimized at `t + 1 = √w`:

> **`2^(2√w − 1)` operations, using `k ≈ 2^(√w − 1)` planted elements.**

For `w = 256`: **`t = 15`, `k = 32 768` lists of `2¹⁶` candidates, ≈ `2³¹` hash evaluations and
≈ 68 GB.** Hours on one machine. This matches the received bound for AdHash — `O(2^(2√n))`, modulus
> 1600 bits for 80-bit security (V3) — applied at `n = 256`. `ℤ/2^w` is in fact the *easy* case for
the attacker: Wagner needs interval tricks for a general modulus, and none here.

`t` is the attacker's dial, trading planted records against offline work — it is not a single point:

| planted elements `k` | 8 | 32 | 128 | 1 024 | 32 768 |
|---|---:|---:|---:|---:|---:|
| offline work `2^(t + w/(t+1))` | 2⁶⁷ | 2⁴⁷·⁷ | 2³⁹ | 2³³·³ | **2³¹** |

An attacker able to inject only 128 records still pays `2³⁹`. The `2¹²⁸` a 256-bit summary is
assumed to provide is reached only at `k = 2`, i.e. by an attacker who declines to use the
structure's homomorphism at all.

**This is executed, not argued** — `rbsr/tests/wagner_false_convergence.rs`. The k-tree is run at
`w ∈ {32, 48, 64}` against the shipped lift (`rsos::digest` reduced mod `2^w`) and the planted
solution is handed to the **unmodified `rbsr::protocol_round`** through a `RsosView` backend. At
every width the driver SKIPs the outer range on two stores that genuinely differ:

| `w` | planted keys `k` | offline lift evaluations |
|---:|---:|---:|
| 32 | 8 | 4 096 |
| 48 | 32 | 16 384 |
| 64 | 128 | 65 536 |

Three controls keep the result honest: an unsolved plant of the same shape must be *refined* (so
the test cannot pass against a driver that skips everything), an unbalanced plant must never be
skipped (Theorem 2, mechanically), and the cost formula is pinned against measured work so the
extrapolation to `w = 256` is arithmetic rather than assertion. **No error term appeared at any
width** — merging on low-order bits is exact, as §6.1 predicts, and `ℤ/2^w` behaves as the XOR
case.

### 6.2 What that gives the attacker

The two peers' honest content cancels: `Σ(X) − Σ(Y) = Σ(P_X) − Σ(P_Y)`. So planting `k/2` elements
on each side makes the **root range** `(−∞, +∞)` compare equal. Counts match by construction, so
Theorem 2 does not stop it. The protocol SKIPs at the root and reports convergence; the replicas
differ forever.

Targeted variant, which is the one that matters: to censor a record `z` that peer A holds and peer B
lacks, solve the same k-sum against target `φ(z)` and balance the counts. Cost is unchanged.
**Anti-entropy — the mechanism whose entire purpose is to repair exactly this divergence — is what
guarantees the record is never delivered.**

`reconcile-rs` makes this concrete rather than theoretical: UDP is unauthenticated by default
(`AGENTS.md` §8), so "can write to a replica" is "can reach the port", and unicasting the two halves
to two different peers is the delivery mechanism.

### 6.3 The consequence for the 40 B question

| design | B/range | honest `P_false` | Theorem 2 | adversarial work |
|---|---:|---|---|---|
| Negentropy, `τ` = 128 | 16 | `2C·2⁻¹²⁸` | ✗ probabilistic | `min(2⁶⁴ birthday, 2³¹ Wagner)` = **2³¹** |
| `rbsr` today, `f_p = id`, `w` = 256 | 40 | `2C·2⁻²⁵⁶` | ✓ exact | **2³¹** |

**The binding constraint is upstream of the comparison map and identical in both designs.** The 24
extra bytes per range do not change the adversarial security level by one bit. `SOTA.md` §2.1's
"the price is 40 B/range against 16 B" prices the wrong thing.

### 6.4 The objection, and why it fails here

Meyer's argument (V4 — **verify the exact wording before relying on this**) is that adversarial
fingerprint collisions do not matter, because a malicious peer can withhold data by simply claiming
not to have it, which requires no collision at all.

That is correct in the model it assumes — *malicious peer vs. honest peer* — and it does not cover
the deployment this repository targets. The attack in §6.2 needs the attacker to be a **writer**,
not a peer. Its victims are **two honest replicas**. Its effect is **persistent** (§8: a
deterministic collision never self-repairs) and **invisible** (both peers report convergence). None
of the three properties holds for withholding, where the honest peer's state is untouched and the
malicious peer is the only one deceived.

**This gap is the thesis.** If V4's wording survives contact with the PDF, this is the contribution.

### 6.5 The combiner rationale in this repo is half wrong

`SOTA.md` §2.4 P0-1 justifies addition mod 2²⁵⁶ as "non-GF(2)-linear ⟹ no Gaussian elimination,
unlike XOR", and calls it "THE criterion that separates a toy structure from a SOTA one". The first
half is right. The second does not follow: Wagner's k-tree needs a group with a chain of quotients,
not `GF(2)`-linearity, and `ℤ/2²⁵⁶` supplies one at every bit position. Against a k-sum attacker,
**addition mod 2^w and XOR on `w` bits are equally strong.** Non-linearity buys resistance to
*solving*, not to *searching*.

---

## 7. The repair, and why it is the constructive half

Key the lift: `φ_K = BLAKE3_keyed(K, enc(key) ‖ enc(value))`. The attacker cannot evaluate `φ_K`, so
cannot grind; §6 collapses and Theorem 1's bound applies with the ROM replaced by PRF security. This
is Clarke et al.'s MSet-Add-Hash result (V6) — and `reconcile-rs` **already has the key**: `gossip`'s
shared cluster secret.

Combining §5 and §7:

> **`f_p = (count, trunc_128(Σ_K))`** — ~24 B/range, Theorem 2 exact, adversarial level `2^128`.

Dominates both current designs on all three columns. Open questions it raises, none blocking the
analysis: `rsos` is domain-pure and holds no key (`AGENTS.md` §9), the cluster key is optional while
the fingerprint is not, and a keyed lift is a wire break.

---

## 8. Rounds — the parameter the question expected to matter

Both effects are non-obvious.

1. **Within one reconciliation**, rounds enter only through `C`: `R = O(log_b n)` is already inside
   Lemma 1. There is no separate per-round term.
2. **Across repeated anti-entropy cycles**, the union bound is over **distinct comparisons**, not
   over rounds. `f_p ∘ A` is deterministic: re-comparing identical content re-derives the identical
   wrong answer.

> On a **static** store, repeating anti-entropy `R` times adds *zero* probability of a new false
> SKIP and *zero* probability of detecting the existing one. Under churn, cut points move, the same
> difference is re-tested against a different partition, and it self-heals with probability
> `1 − ε` per changed partition.

So a static store is the worst case — it neither accumulates risk nor heals — and the accidental
(honest-model) failure is self-healing under churn while the **planted** failure is not: the attacker
re-plants, or simply picks a range that churn does not touch.

---

## 9. Experimental programme

Assertions this repository would not accept without a command behind them (`AGENTS.md` §10).

- **E1 — measure `C(n, d, b)`.** Instrument `protocol_round` (`RoundOutcome` already tallies
  skip/enumerate/split/children) and sweep `n × d × b` in `benches/protocol.rs`. Validates Lemma 1
  against the real driver, not against a model of it. *Cheap, no new machinery.*
- **E2 — validate Theorem 1 at reduced width.** Instantiate the protocol with `w ∈ {16, 24, 32}`,
  drive random pairs to a fixed point, count executions that terminate with `X ≠ Y`. Compare the
  empirical rate with `2C·2^(−w)`. Falsifiable, and it exercises the real `FingerprintTreeMap` and
  the real `protocol_round`. *The bound is worthless unless this matches.*
- **E3 — the k-tree at reduced width.** ✅ **done**, `rbsr/tests/wagner_false_convergence.rs`, four
  tests, 0.17 s, inside the standard `cargo test --workspace` gate. Results and controls in §6.1.
  This was the load-bearing risk and it resolved in favour of §6: **§6 and §7 stand.**
- **E4 — extrapolate E3 to `w = 256`.** Partly discharged: the cost formula is pinned by test
  against measured work, so `2³¹` is arithmetic on a verified formula rather than an assertion.
  What remains is a wall-clock measurement per list level at a width large enough to be a real
  timing (`w = 128`, `2²¹·⁶`), to state the `w = 256` cost in hours rather than in operations.
- **E5 — the end-to-end attack against `reconcile`.** E3 stops at `protocol_round`. The full chain
  is two live `ReplicatedMap`s over UDP, the two halves of the plant unicast one to each peer, and an
  assertion that the divergence survives a full anti-entropy cycle. Not needed to establish the
  result — E3 already exercises the real driver — but it is what turns "the mechanism is exploitable"
  into "this deployment is exploitable", and it is the demo that would accompany disclosure (§11).

E1–E3 live in-tree and stay CI-green; E3 already does. E4's timing and E5's live drive are the
remainder.

---

## 10. Related work and the novelty gates

- **Amparore, arXiv:2603.19820 §6.1** — states the gap. The premise of this document. *V2.*
- **Meyer, arXiv:2212.13567** — formalizes fingerprint schemes and surveys secure instantiations;
  reportedly dismisses adversarial collisions via the withholding argument. §6.4 is the rebuttal.
  *V4, V5 — the highest-risk gate: if Meyer already bounds false convergence end-to-end, Theorem 1
  is a restatement and only §5–§7 survive.*
- **Meyer & Scherer, RBSR without homomorphic hashing** — the orthogonal escape: no composable
  summary, no L1. Belongs in the comparison table.
- **Wagner, CRYPTO 2002** — the k-tree; AdHash at `O(2^(2√n))`, > 1600-bit modulus for 80-bit
  security. *V3.*
- **Bellare & Micciancio, EUROCRYPT 1997** — AdHash/MuHash, the construction Wagner attacks.
- **Clarke, Devadas, van Dijk, Gassend, Suh, ASIACRYPT 2003** — MSet-Add-Hash / MSet-Mu-Hash;
  keying is the repair. Already cited by `rsos/src/fingerprint.rs`. *V6.*
- **Lewi, Kim, Maykov, Weis, ePrint 2019/227 (LtHash)** — 16-bit × 1024 components, ~200-bit
  security via SIS. The industrial precedent for *not* using a single 256-bit modulus. Facebook
  chose 2 KB of hash over 32 B for exactly the reason in §6.1.
- **Lyubashevsky et al.** — lattice attacks on modular subset-sum; may beat Wagner at `w = 256` and
  would only strengthen §6.

---

## 11. Where this lands in the repo

Independent of publication:

- `SOTA.md` §2.1 — "the price is 40 B/range against 16 B" is the wrong frame (§6.3).
- `SOTA.md` §2.4 P0-1 — the non-`GF(2)`-linearity rationale does not carry the conclusion (§6.5).
- `rsos/src/fingerprint.rs` — the module doc's "an abelian group whose carries are not `GF(2)`-linear,
  unlike the XOR combiner it must never become" is correct and *insufficient*; same correction.
- A keyed-lift issue — no longer gated on E3, which has passed; gated now on the maintainer decision
  in §7 (a keyed lift is a wire break, and `rsos` holds no key).

**Disclosure posture (chosen 2026-08-14).** Notify the affected maintainers — `hoytech/negentropy`
(deployed on nostr), Amparore (AELMDB), and Willow/Earthstar — before any paper or full-width demo.
The in-tree E3 stays at reduced width: a mechanism demonstration against this repo's own driver, not
a turnkey attack on a third party. E5's live drive waits on that notification.
