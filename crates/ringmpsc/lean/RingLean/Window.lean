import RingmpscRs.Funs

/-!
# Theorems about `crates/ringmpsc/src/window.rs`

The definitions in `RingmpscRs.Funs` are generated from `window.rs` by Charon
and Aeneas (`scripts/check.sh`); this file proves properties of them for all
inputs.

`usize` has an abstract width in Aeneas (`UScalarTy.Usize.numBits` is 32 or 64),
so `u64 as usize` is a truncating cast and every theorem here holds on both
32-bit and 64-bit targets.

Each `_spec` theorem also proves the function cannot panic: Aeneas models
arithmetic overflow as `fail`, and `f ⦃ r => P r ⦄` means `f` returns `ok r`
with `P r`.

Premises named after `spec.md` invariants are facts the ring maintains; they
are proved elsewhere (Verus, Quint) and assumed here.

Proof notes:
- `omega` does not see `Usize` comparisons that come from an `if` condition;
  they are restated on `.val` first (`have h' : a.val < b.val := h`).
- `scalar_tac` can loop on an equation `x = UScalar.cast _ y`; such hypotheses
  are turned into `.val` facts and cleared before calling it.
-/

open Aeneas Aeneas.Std Result WP

namespace ringmpsc_rs.window

/-- `tail.wrapping_sub(head)` as a natural number: the number of sequence
numbers from `head` up to `tail`, modulo 2^64. -/
def dist (tail head : U64) : Nat :=
  (tail.val + (UScalar.size .U64 - head.val)) % UScalar.size .U64

/-! ## Arithmetic helpers -/

/-- Without `u64` wrap-around (INV-SEQ-03), `dist` is plain subtraction. -/
theorem dist_of_le (tail head : U64) (h : head.val ≤ tail.val) :
    dist tail head = tail.val - head.val := by
  unfold dist
  have := tail.hBounds
  have := head.hBounds
  simp only [UScalar.size, UScalarTy.U64_numBits_eq] at *
  omega

/-- A power-of-two `usize` capacity `2^k` has `k` below the width of `usize`. -/
theorem pow_lt_numBits {cap : Usize} {k : Nat} (hcap : cap.val = 2 ^ k) :
    k < UScalarTy.Usize.numBits := by
  have h := cap.hBounds
  rw [hcap] at h
  exact (Nat.pow_lt_pow_iff_right (by decide)).mp h

/-- Masking the truncated sequence number with `2^k - 1` is `seq mod 2^k`,
on any `usize` width. -/
theorem masked_val (seq : U64) (mask : Usize) (k : Nat)
    (hk : k < UScalarTy.Usize.numBits) (hmask : mask.val = 2 ^ k - 1) :
    (UScalar.cast .Usize seq &&& mask).val = seq.val % 2 ^ k := by
  rw [UScalar.val_and, UScalar.cast_val_eq, hmask, Nat.and_two_pow_sub_one_eq_mod,
    Nat.mod_mod_of_dvd _ (Nat.pow_dvd_pow 2 hk.le)]

/-! ## `slot_index` -/

/-- `slot_index(seq, cap - 1)` is `seq mod cap`, hence a valid buffer index
(INV-MEM-02: `cap` is a power of two). -/
theorem slot_index_spec (seq : U64) (mask cap : Usize) (k : Nat)
    (hcap : cap.val = 2 ^ k) (hmask : mask.val = cap.val - 1) :
    slot_index seq mask ⦃ idx => idx.val = seq.val % cap.val ∧ idx.val < cap.val ⦄ := by
  unfold slot_index
  simp only [lift, bind_ok, spec_ok]
  have hval := masked_val seq mask k (pow_lt_numBits hcap) (by rw [hmask, hcap])
  rw [hval, hcap]
  exact ⟨rfl, Nat.mod_lt _ (Nat.two_pow_pos k)⟩

/-! ## `occupancy` -/

/-- `occupancy` is exact when the distance fits the capacity (INV-SEQ-01). -/
theorem occupancy_spec (tail head : U64) (cap : Usize)
    (hseq : dist tail head ≤ cap.val) :
    occupancy tail head ⦃ o => o.val = dist tail head ⦄ := by
  unfold occupancy
  simp only [lift, bind_ok, spec_ok]
  rw [UScalar.cast_val_eq, core.num.U64.wrapping_sub_val_eq]
  apply Nat.mod_eq_of_lt
  exact Nat.lt_of_le_of_lt hseq cap.hBounds

/-- Without INV-SEQ-01, `occupancy` is still the distance truncated to `usize`. -/
theorem occupancy_val (tail head : U64) :
    occupancy tail head ⦃ o => o.val = dist tail head % 2 ^ UScalarTy.Usize.numBits ⦄ := by
  unfold occupancy
  simp only [lift, bind_ok, spec_ok]
  rw [UScalar.cast_val_eq, core.num.U64.wrapping_sub_val_eq]
  rfl

/-! ## `free_space` -/

/-- `free_space` never panics and never exceeds the capacity. When INV-SEQ-01
holds it is exactly `cap - (tail - head)`, so occupancy and free space add up
to the capacity. -/
theorem free_space_spec (tail head : U64) (cap : Usize) :
    free_space tail head cap ⦃ f =>
      f.val ≤ cap.val ∧ (dist tail head ≤ cap.val → f.val = cap.val - dist tail head) ⦄ := by
  unfold free_space
  step with occupancy_val tail head as ⟨used, hused⟩
  have hfit : dist tail head ≤ cap.val → used.val = dist tail head := by
    intro h
    rw [hused]
    exact Nat.mod_eq_of_lt (Nat.lt_of_le_of_lt h cap.hBounds)
  split
  · rw [spec_ok]
    rename_i hge
    have hge' : cap.val ≤ used.val := hge
    refine ⟨by simp, fun h => ?_⟩
    have := hfit h
    simp
    omega
  · step*

/-- A stale `head` (the producer's `cached_head`, which only lags the real
`head`) never reports more free space than the real `head` would. -/
theorem free_space_stale_conservative (tail head stale : U64) (cap f_stale f_head : Usize)
    (hstale : dist tail stale ≤ cap.val)
    (hlag : dist tail head ≤ dist tail stale)
    (h1 : free_space tail stale cap = ok f_stale)
    (h2 : free_space tail head cap = ok f_head) :
    f_stale.val ≤ f_head.val := by
  have s1 := free_space_spec tail stale cap
  have s2 := free_space_spec tail head cap
  rw [h1, spec_ok] at s1
  rw [h2, spec_ok] at s2
  have e1 := s1.2 hstale
  have e2 := s2.2 (Nat.le_trans hlag hstale)
  omega

/-! ## `write_window` -/

/-- The producer's window starts at `tail mod cap`, stays inside the buffer,
never exceeds the `n` slots asked for, and is non-empty when `n ≥ 1`
(INV-MEM-02, INV-RES-01). -/
theorem write_window_spec (tail : U64) (n cap : Usize) (k : Nat) (hcap : cap.val = 2 ^ k) :
    write_window tail n cap ⦃ idx len =>
      idx.val = tail.val % cap.val ∧ idx.val < cap.val ∧
      idx.val + len.val ≤ cap.val ∧ len.val ≤ n.val ∧
      len.val = min n.val (cap.val - idx.val) ∧
      (1 ≤ n.val → 1 ≤ len.val) ⦄ := by
  have hpos : 1 ≤ cap.val := by rw [hcap]; exact Nat.one_le_two_pow
  unfold write_window
  step as ⟨i, hi⟩
  step with slot_index_spec tail i cap k hcap (by omega) as ⟨idx, hidx, hlt⟩
  step as ⟨room, hroom⟩
  split <;> simp only [spec_ok] <;> refine ⟨hidx, hlt, ?_, ?_, ?_, ?_⟩ <;> scalar_tac

/-! ## `read_window` -/

/-- The consumer's window: `None` exactly when nothing is readable
(`tail ≤ head`, including a cached `tail` that trails `head`, #17). Otherwise
it starts at `head mod cap`, stays inside the buffer, and lies within the
published range `[head, tail)` (INV-INIT-01). It is non-empty when
`tail - head ≤ cap` (INV-SEQ-01). -/
theorem read_window_spec (head tail : U64) (cap : Usize) (k : Nat) (hcap : cap.val = 2 ^ k) :
    read_window head tail cap ⦃ r =>
      match r with
      | none => tail.val ≤ head.val
      | some (idx, len) =>
          head.val < tail.val ∧
          idx.val = head.val % cap.val ∧ idx.val < cap.val ∧
          idx.val + len.val ≤ cap.val ∧
          len.val ≤ tail.val - head.val ∧
          (tail.val - head.val ≤ cap.val → 1 ≤ len.val) ⦄ := by
  have hpos : 1 ≤ cap.val := by rw [hcap]; exact Nat.one_le_two_pow
  unfold read_window
  split
  · simp only [spec_ok]; scalar_tac
  · rename_i hnot
    step as ⟨d, hd⟩
    step as ⟨avail, havail⟩
    step as ⟨i, hi⟩
    step with slot_index_spec head i cap k hcap (by omega) as ⟨idx, hidx, hlt⟩
    step as ⟨room, hroom⟩
    have hav : avail.val = (tail.val - head.val) % 2 ^ UScalarTy.Usize.numBits := by
      rw [havail, UScalar.cast_val_eq, hd]
    have hav_le : avail.val ≤ tail.val - head.val := by
      rw [hav]; exact Nat.mod_le _ _
    have hav_eq : tail.val - head.val ≤ cap.val → avail.val = tail.val - head.val := by
      intro h
      rw [hav]
      exact Nat.mod_eq_of_lt (by have := cap.hBounds; omega)
    have hht : head.val < tail.val := Nat.lt_of_not_le hnot
    clear havail
    split
    · rename_i hsel
      have hsel' : avail.val < room.val := hsel
      rw [spec_ok]
      exact ⟨hht, hidx, hlt, by omega, hav_le, fun h => by have := hav_eq h; omega⟩
    · rename_i hsel
      have hsel' : ¬ avail.val < room.val := hsel
      rw [spec_ok]
      exact ⟨hht, hidx, hlt, by omega, by omega, fun h => by have := hav_eq h; omega⟩

/-! ## `consume_count` -/

theorem consume_count_spec (avail max : Usize) :
    consume_count avail max ⦃ c => c.val = min avail.val max.val ⦄ := by
  unfold consume_count
  split <;> simp only [spec_ok] <;> scalar_tac

/-! ## Producer and consumer windows are disjoint

With unbounded sequence numbers (INV-SEQ-03: no `u64` wrap), the slots the
producer writes, `tail, tail+1, …, tail+len-1`, and the slots the consumer
owns, `head, …, tail-1`, map to different buffer indices whenever they fit in
the ring together: `(tail - head) + len ≤ cap`. `write_window_spec` and
`free_space_spec` give exactly that bound for a reservation of
`len ≤ n ≤ free_space`. -/
theorem windows_disjoint (head tail len cap : Nat)
    (horder : head ≤ tail) (hfit : (tail - head) + len ≤ cap)
    (i j : Nat) (hi : i < len) (hj : j < tail - head) :
    (tail + i) % cap ≠ (head + j) % cap := by
  intro heq
  have hlt : head + j < tail + i := by omega
  have hdvd : cap ∣ (tail + i) - (head + j) :=
    (Nat.modEq_iff_dvd' hlt.le).mp heq.symm
  have hpos : 0 < (tail + i) - (head + j) := by omega
  have hsmall : (tail + i) - (head + j) < cap := by omega
  exact absurd (Nat.le_of_dvd hpos hdvd) (by omega)

/-- A reservation that `free_space` allows, laid out by `write_window`, never
touches a slot the consumer has not yet read. -/
theorem reservation_disjoint (tail head : U64) (n cap f idx len : Usize) (k : Nat)
    (hcap : cap.val = 2 ^ k)
    (horder : head.val ≤ tail.val)                 -- INV-SEQ-03
    (hseq : tail.val - head.val ≤ cap.val)          -- INV-SEQ-01
    (hfree : free_space tail head cap = ok f)
    (hn : n.val ≤ f.val)
    (hw : write_window tail n cap = ok (idx, len))
    (i j : Nat) (hi : i < len.val) (hj : j < tail.val - head.val) :
    (tail.val + i) % cap.val ≠ (head.val + j) % cap.val := by
  have hd : dist tail head = tail.val - head.val := dist_of_le tail head horder
  have sf := free_space_spec tail head cap
  rw [hfree, spec_ok] at sf
  have sw := write_window_spec tail n cap k hcap
  rw [hw, spec_ok] at sw
  obtain ⟨-, -, -, hlen, -, -⟩ := sw
  have hf := sf.2 (by rw [hd]; exact hseq)
  rw [hd] at hf
  exact windows_disjoint head.val tail.val len.val cap.val horder (by omega) i j hi hj

end ringmpsc_rs.window
