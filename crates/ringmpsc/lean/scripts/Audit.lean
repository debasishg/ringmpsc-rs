import RingLean.Window

/-!
Axiom audit for `scripts/check.sh`.

Walks every theorem declared in a `RingLean.*` module (whatever its attributes,
modifiers or name) and fails if any depends on an axiom outside Lean's standard
three. That rejects `sorry` (`sorryAx`), user-declared `axiom`s, and
`native_decide` (`Lean.ofReduceBool`).

Prints `THEOREM <name>` for each audited theorem; `check.sh` checks the
`[lean: ...]` citations in `src/` against that list.

Add an `import` here for each new file in `RingLean/`.
-/

open Lean Elab Command

def allowedAxioms : List Name := [``propext, ``Classical.choice, ``Quot.sound]

elab "#audit_ringlean" : command => do
  let env ← getEnv
  let mut audited := 0
  for h : i in [0:env.header.moduleNames.size] do
    let mod := env.header.moduleNames[i]
    unless (`RingLean).isPrefixOf mod do continue
    for n in env.header.moduleData[i]!.constNames do
      if n.isInternal then continue
      let some (.thmInfo _) := env.find? n | continue
      let axioms ← collectAxioms n
      let extra := axioms.filter (fun a => !allowedAxioms.contains a)
      if extra.isEmpty then
        logInfo m!"THEOREM {n}"
        audited := audited + 1
      else
        logError m!"{n} depends on disallowed axioms: {extra.toList}"
  if audited == 0 then
    logError "no theorems found in RingLean.* modules"

#audit_ringlean
